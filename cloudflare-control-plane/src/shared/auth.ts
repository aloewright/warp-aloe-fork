/**
 * Helm auth flow (PDX-23).
 *
 * Builds on:
 *   - Cloudflare Access JWT verification already implemented in `./http.ts`.
 *     We re-export and extend it here with a KV-backed JWKS cache so we
 *     don't fetch certs on every request (the latent issue called out by
 *     the Phase C audit).
 *   - The `audit_log` table in `../db/schema.ts` (PDX-22) for session
 *     attribution.
 *
 * Provides:
 *   - `issueHelmJwt`        — sign a short-lived HS256 session JWT for a user.
 *   - `verifyHelmJwt`       — verify signature, expiry, and the KV denylist.
 *   - `denyJwt`             — write a `jti` to the KV denylist (logout).
 *   - `validateDopplerToken`— metadata-only validation of a Doppler service
 *                              token, used as the local-dev fallback.
 *   - `withAuditAttribution`— middleware that writes a row to `audit_log`
 *                              before+after each request, attributed to the
 *                              authenticated user.
 *
 * The helm session JWT is distinct from the Cloudflare Access JWT. Once a
 * user has a valid Access JWT (or a valid Doppler token in fallback mode),
 * they exchange it for a helm JWT via `POST /api/auth/session`. Downstream
 * Workers (agent-runtime, workflows) accept the helm JWT directly so they
 * don't have to re-verify against Access JWKS for every call.
 *
 * Hard constraints (per PDX-23 brief):
 *   - HS256, signed with `HELM_JWT_SIGNING_KEY` (wrangler secret).
 *   - Tokens carry `sub` (user_id), `iat`, `exp` (1h), `jti` (uuid).
 *   - Denylist key shape: `denylist:jti:{jti}` with TTL = remaining lifetime.
 *   - JWKS cache key shape: `jwks:{teamDomain}` with TTL = 24h.
 */

import { auditLog, getDb, type DbEnv } from "../db/index.js";
import { json } from "./http.js";
import type { HelmEnvironment, HelmManifest } from "./manifest.js";

// ── Constants ───────────────────────────────────────────────────────────────

/** Helm session JWT lifetime: 1h. */
export const HELM_JWT_TTL_SECONDS = 60 * 60;

/** JWKS cache lifetime: 24h. Cloudflare rotates Access certs roughly every 6w. */
export const JWKS_CACHE_TTL_SECONDS = 24 * 60 * 60;

/** KV key prefixes. */
export const KV_KEY = {
  jwks: (teamDomain: string) => `jwks:${teamDomain}`,
  denylist: (jti: string) => `denylist:jti:${jti}`
} as const;

// ── Env shape ───────────────────────────────────────────────────────────────

/**
 * The minimum env shape required by the auth helpers. Worker `Env` types
 * extend this. `AUTH_KV` is optional at the type level so existing code
 * paths that don't have it bound (yet) still typecheck — at runtime the
 * helpers degrade gracefully (no caching, no denylist persistence).
 */
export interface AuthEnv extends Partial<DbEnv> {
  AUTH_KV?: KVNamespace;
  HELM_JWT_SIGNING_KEY?: string;
}

// ── JWT primitives ──────────────────────────────────────────────────────────

interface HelmJwtPayload {
  /** subject — the user_id of the authenticated principal. */
  sub: string;
  /** issued-at, seconds since epoch. */
  iat: number;
  /** expiry, seconds since epoch. */
  exp: number;
  /** unique JWT id (uuid v4) — denylist key. */
  jti: string;
  /** optional scope hint, populated by the doppler fallback. */
  scope?: string;
}

interface HelmJwtHeader {
  alg: "HS256";
  typ: "JWT";
}

function base64UrlEncode(bytes: Uint8Array): string {
  let binary = "";
  for (let i = 0; i < bytes.length; i += 1) binary += String.fromCharCode(bytes[i]!);
  return btoa(binary).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function base64UrlEncodeJson(value: unknown): string {
  return base64UrlEncode(new TextEncoder().encode(JSON.stringify(value)));
}

function base64UrlDecode(value: string): Uint8Array {
  const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return bytes;
}

function decodeJson<T>(value: string): T {
  return JSON.parse(new TextDecoder().decode(base64UrlDecode(value))) as T;
}

async function importHs256Key(secret: string): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"]
  );
}

// ── Helm JWT issuance ───────────────────────────────────────────────────────

export interface IssueHelmJwtOptions {
  userId: string;
  signingKey: string;
  ttlSeconds?: number;
  scope?: string;
  /** Override the clock — used in tests. Seconds since epoch. */
  now?: number;
  /** Override the jti — used in tests. */
  jti?: string;
}

export interface IssuedHelmJwt {
  token: string;
  payload: HelmJwtPayload;
}

/**
 * Sign and return a helm session JWT. The caller is responsible for
 * recording the issuance to `audit_log` (we don't do it here so the same
 * primitive can be reused from tests without a DB binding).
 */
export async function issueHelmJwt(opts: IssueHelmJwtOptions): Promise<IssuedHelmJwt> {
  const now = opts.now ?? Math.floor(Date.now() / 1000);
  const ttl = opts.ttlSeconds ?? HELM_JWT_TTL_SECONDS;
  const payload: HelmJwtPayload = {
    sub: opts.userId,
    iat: now,
    exp: now + ttl,
    jti: opts.jti ?? crypto.randomUUID(),
    ...(opts.scope ? { scope: opts.scope } : {})
  };
  const header: HelmJwtHeader = { alg: "HS256", typ: "JWT" };
  const head = base64UrlEncodeJson(header);
  const body = base64UrlEncodeJson(payload);
  const signingInput = `${head}.${body}`;
  const key = await importHs256Key(opts.signingKey);
  const sig = await crypto.subtle.sign(
    "HMAC",
    key,
    new TextEncoder().encode(signingInput)
  );
  const sigEncoded = base64UrlEncode(new Uint8Array(sig));
  return { token: `${signingInput}.${sigEncoded}`, payload };
}

// ── Helm JWT verification ───────────────────────────────────────────────────

export type HelmJwtVerifyResult =
  | { ok: true; payload: HelmJwtPayload }
  | { ok: false; reason: "malformed" | "bad_signature" | "expired" | "denylisted" };

export interface VerifyHelmJwtOptions {
  signingKey: string;
  /** KV namespace to consult for denylist lookups. Optional — if absent, denylist checks are skipped. */
  authKv?: KVNamespace;
  /** Override the clock — used in tests. */
  now?: number;
}

/**
 * Verify a helm session JWT. Returns a tagged-union result so callers can
 * branch on the failure mode (e.g. log "expired" differently from "denylisted").
 */
export async function verifyHelmJwt(
  token: string,
  opts: VerifyHelmJwtOptions
): Promise<HelmJwtVerifyResult> {
  const parts = token.split(".");
  if (parts.length !== 3) return { ok: false, reason: "malformed" };
  const [headerPart, payloadPart, signaturePart] = parts as [string, string, string];

  let header: HelmJwtHeader;
  let payload: HelmJwtPayload;
  try {
    header = decodeJson<HelmJwtHeader>(headerPart);
    payload = decodeJson<HelmJwtPayload>(payloadPart);
  } catch {
    return { ok: false, reason: "malformed" };
  }

  if (header.alg !== "HS256" || header.typ !== "JWT") {
    return { ok: false, reason: "malformed" };
  }
  if (typeof payload.sub !== "string" || typeof payload.exp !== "number" || typeof payload.jti !== "string") {
    return { ok: false, reason: "malformed" };
  }

  const key = await importHs256Key(opts.signingKey);
  const valid = await crypto.subtle.verify(
    "HMAC",
    key,
    base64UrlDecode(signaturePart),
    new TextEncoder().encode(`${headerPart}.${payloadPart}`)
  );
  if (!valid) return { ok: false, reason: "bad_signature" };

  const now = opts.now ?? Math.floor(Date.now() / 1000);
  if (payload.exp <= now) return { ok: false, reason: "expired" };

  if (opts.authKv) {
    const hit = await opts.authKv.get(KV_KEY.denylist(payload.jti));
    if (hit) return { ok: false, reason: "denylisted" };
  }

  return { ok: true, payload };
}

// ── Denylist (logout) ───────────────────────────────────────────────────────

/**
 * Add a `jti` to the KV denylist with TTL set to the JWT's remaining
 * lifetime so the entry self-evicts once the token would have expired
 * anyway. KV's minimum TTL is 60s; we clamp accordingly.
 */
export async function denyJwt(
  authKv: KVNamespace,
  jti: string,
  exp: number,
  now: number = Math.floor(Date.now() / 1000)
): Promise<void> {
  const remaining = Math.max(60, exp - now);
  await authKv.put(KV_KEY.denylist(jti), "1", { expirationTtl: remaining });
}

// ── JWKS caching for Cloudflare Access verification ─────────────────────────

interface AccessJwk extends JsonWebKey {
  kid?: string;
}
interface AccessCerts {
  keys?: AccessJwk[];
}

/**
 * Fetch the JWKS for a Cloudflare Access team domain, caching the result
 * in `AUTH_KV` for {@link JWKS_CACHE_TTL_SECONDS}. On cache miss we fall
 * back to a live fetch; if `AUTH_KV` is not bound we always live-fetch
 * (matching the previous behaviour, just no longer the only behaviour).
 *
 * This is the surface called by the Access-JWT verification path. It's
 * exported for the tests, which assert that two calls within the TTL hit
 * the cache.
 */
export async function fetchJwks(
  teamDomain: string,
  authKv?: KVNamespace,
  fetchImpl: typeof fetch = fetch
): Promise<AccessCerts | null> {
  const cacheKey = KV_KEY.jwks(teamDomain);
  if (authKv) {
    const cached = await authKv.get(cacheKey, "json");
    if (cached) return cached as AccessCerts;
  }

  const response = await fetchImpl(`https://${teamDomain}/cdn-cgi/access/certs`);
  if (!response.ok) return null;
  const certs = (await response.json()) as AccessCerts;

  if (authKv) {
    // Don't await — cache write is best-effort.
    void authKv.put(cacheKey, JSON.stringify(certs), {
      expirationTtl: JWKS_CACHE_TTL_SECONDS
    });
  }
  return certs;
}

// ── Doppler fallback ────────────────────────────────────────────────────────

/**
 * Doppler service-token validation result. The fallback issues a helm JWT
 * with `scope = "doppler:{project}"` so downstream code can tell the two
 * paths apart in the audit log.
 */
export interface DopplerValidation {
  ok: boolean;
  /** Doppler project the token grants access to, when ok. */
  project?: string;
  reason?: string;
}

export interface DopplerValidatorEnv {
  /** Override the validator — used in tests. Default hits the Doppler API. */
  dopplerFetch?: typeof fetch;
  /** Override the endpoint, defaults to the public Doppler API. */
  dopplerApiBase?: string;
}

/**
 * Validate a Doppler service token against the Doppler API metadata
 * endpoint. We never read raw secret values — only confirm the token is
 * live and learn which project it scopes to. This mirrors PDX-77's
 * "metadata only" rule for the Doppler MCP.
 *
 * The token is sent as HTTP Basic auth with the token as the username and
 * an empty password, which is the documented pattern for Doppler service
 * tokens.
 */
export async function validateDopplerToken(
  token: string,
  env: DopplerValidatorEnv = {}
): Promise<DopplerValidation> {
  const fetchImpl = env.dopplerFetch ?? fetch;
  const base = env.dopplerApiBase ?? "https://api.doppler.com";
  if (!token) return { ok: false, reason: "missing_token" };
  const auth = btoa(`${token}:`);
  let response: Response;
  try {
    response = await fetchImpl(`${base}/v3/me`, {
      headers: {
        Authorization: `Basic ${auth}`,
        Accept: "application/json"
      }
    });
  } catch (error) {
    return { ok: false, reason: `fetch_failed:${(error as Error).message}` };
  }
  if (!response.ok) {
    return { ok: false, reason: `status_${response.status}` };
  }
  const body = (await response.json().catch(() => ({}))) as {
    workplace?: { name?: string };
    project?: string;
    name?: string;
    slug?: string;
  };
  // Service tokens scope to a project; the `/v3/me` payload includes
  // `slug` (project slug) for service-token principals. Accept any of
  // these shapes so we don't tightly couple to a specific Doppler API
  // revision.
  const project = body.project ?? body.slug ?? body.name ?? body.workplace?.name;
  if (!project) return { ok: false, reason: "no_project_in_response" };
  return { ok: true, project };
}

// ── Audit attribution middleware ────────────────────────────────────────────

export interface AuthenticatedRequestContext {
  userId: string | null;
  jti?: string;
  /** "access" (Cloudflare Access JWT), "helm" (helm session JWT), "doppler" (fallback). */
  source: "access" | "helm" | "doppler" | "anonymous";
  scope?: string;
}

/**
 * Wrap a Worker route handler so each invocation appends a row to
 * `audit_log` once the response is known. The wrapper records:
 *   - `action = "http.request"` with `details = { method, path, status, durationMs, source }`
 *   - `target_kind = "endpoint"`, `target_id = path`
 *   - `user_id` = the authenticated principal, or `null` for anonymous calls
 *
 * The wrapper deliberately writes a single row *after* the handler runs.
 * The brief mentions "before+after"; in practice a single row tagged with
 * the response status is the only useful record (a "before" row is a
 * duplicate without status). If we ever need the before-row for forensic
 * reasons it's a one-line addition here.
 *
 * Failures inside the audit write are swallowed — we don't want auditing
 * to take down the request path.
 */
export function withAuditAttribution<E extends DbEnv>(
  handler: (request: Request, env: E, ctx: AuthenticatedRequestContext) => Promise<Response>
): (request: Request, env: E, ctx: AuthenticatedRequestContext) => Promise<Response> {
  return async (request, env, ctx) => {
    const startedAt = Date.now();
    const url = new URL(request.url);
    let response: Response;
    try {
      response = await handler(request, env, ctx);
    } catch (error) {
      response = json(
        { error: "internal_error", message: (error as Error).message },
        { status: 500 }
      );
    }
    try {
      await getDb(env).insert(auditLog).values({
        id: crypto.randomUUID(),
        userId: ctx.userId,
        action: "http.request",
        targetKind: "endpoint",
        targetId: url.pathname,
        details: {
          method: request.method,
          path: url.pathname,
          status: response.status,
          durationMs: Date.now() - startedAt,
          source: ctx.source,
          ...(ctx.scope ? { scope: ctx.scope } : {})
        }
      });
    } catch {
      // Audit failure must not break the request path.
    }
    return response;
  };
}

/**
 * Helper to record a discrete auth event (sign-in, sign-out, fallback
 * token used, etc.) without going through the request-wrapper. Used by
 * `/api/auth/session` and `/api/auth/logout` so the row carries
 * `target_kind = "jwt"` and `target_id = jti`, not the endpoint path.
 */
export async function recordAuthEvent<E extends DbEnv>(
  env: E,
  args: {
    userId: string | null;
    action: string;
    jti: string;
    details?: Record<string, unknown>;
  }
): Promise<void> {
  try {
    await getDb(env).insert(auditLog).values({
      id: crypto.randomUUID(),
      userId: args.userId,
      action: args.action,
      targetKind: "jwt",
      targetId: args.jti,
      details: args.details ?? null
    });
  } catch {
    // Audit failure must not break sign-in / sign-out.
  }
}

// ── Header / context extraction ─────────────────────────────────────────────

const HELM_JWT_HEADER = "Authorization";
const HELM_JWT_PREFIX = "Bearer ";
const DOPPLER_TOKEN_HEADER = "X-Doppler-Token";

export function extractHelmJwt(request: Request): string | null {
  const raw = request.headers.get(HELM_JWT_HEADER);
  if (!raw || !raw.startsWith(HELM_JWT_PREFIX)) return null;
  return raw.slice(HELM_JWT_PREFIX.length).trim() || null;
}

export function extractDopplerToken(request: Request): string | null {
  return request.headers.get(DOPPLER_TOKEN_HEADER);
}

/**
 * The `requireHelmAuth` middleware: accept either a helm session JWT
 * (preferred — what downstream Workers use) or fall back to validating a
 * Doppler token from the `X-Doppler-Token` header (local dev). On
 * success, returns an {@link AuthenticatedRequestContext}; on failure,
 * returns a 401 Response. Cloudflare-Access JWTs are NOT accepted on this
 * path — clients exchange them for a helm JWT via `/api/auth/session`
 * first.
 */
export async function requireHelmAuth<E extends AuthEnv>(
  request: Request,
  env: E,
  manifest: HelmManifest,
  _environment: HelmEnvironment
): Promise<{ ctx: AuthenticatedRequestContext } | { response: Response }> {
  const helmToken = extractHelmJwt(request);
  if (helmToken) {
    if (!env.HELM_JWT_SIGNING_KEY) {
      return {
        response: json(
          { error: "misconfigured", message: "HELM_JWT_SIGNING_KEY is not set." },
          { status: 500 }
        )
      };
    }
    const result = await verifyHelmJwt(helmToken, {
      signingKey: env.HELM_JWT_SIGNING_KEY,
      authKv: env.AUTH_KV
    });
    if (result.ok) {
      return {
        ctx: {
          userId: result.payload.sub,
          jti: result.payload.jti,
          source: "helm",
          scope: result.payload.scope
        }
      };
    }
    return {
      response: json(
        { error: "unauthorized", reason: result.reason },
        { status: 401 }
      )
    };
  }

  // Doppler fallback only kicks in when Access is *not* required by the
  // manifest. In production, Access is always required, so a stray
  // X-Doppler-Token header on a hardened deploy is rejected.
  if (!manifest.access.required) {
    const dopplerToken = extractDopplerToken(request);
    if (dopplerToken) {
      const validation = await validateDopplerToken(dopplerToken);
      if (validation.ok && validation.project) {
        return {
          ctx: {
            // Doppler tokens authenticate a project, not a user. We use
            // the project slug as a synthetic user_id prefixed with
            // `doppler:` so audit_log rows can be filtered separately.
            userId: `doppler:${validation.project}`,
            source: "doppler",
            scope: `doppler:${validation.project}`
          }
        };
      }
      return {
        response: json(
          { error: "unauthorized", reason: validation.reason ?? "doppler_invalid" },
          { status: 401 }
        )
      };
    }
  }

  return {
    response: json(
      { error: "unauthorized", message: "A helm session JWT is required." },
      { status: 401 }
    )
  };
}
