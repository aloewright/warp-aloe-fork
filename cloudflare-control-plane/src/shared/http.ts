import type { HelmEnvironment, HelmManifest } from "./manifest.js";
import { fetchJwks } from "./auth.js";

export interface HealthPayload {
  service: string;
  environment: HelmEnvironment;
  version: string;
  buildId: string;
}

export function json(data: unknown, init: ResponseInit = {}): Response {
  return new Response(JSON.stringify(data, null, 2), {
    ...init,
    headers: {
      "Content-Type": "application/json",
      ...init.headers
    }
  });
}

export function methodNotAllowed(): Response {
  return json({ error: "method_not_allowed" }, { status: 405 });
}

export function notFound(): Response {
  return json({ error: "not_found" }, { status: 404 });
}

export function health(payload: HealthPayload): Response {
  return json(payload);
}

interface AccessJwtHeader {
  alg?: string;
  kid?: string;
}

interface AccessJwtPayload {
  aud?: string | string[];
  exp?: number;
  nbf?: number;
  iss?: string;
}

interface AccessJwk extends JsonWebKey {
  kid?: string;
}

interface AccessCerts {
  keys?: AccessJwk[];
}

function base64UrlDecode(value: string): Uint8Array {
  const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
  const padded = normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "=");
  const binary = atob(padded);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

function decodeJson<T>(value: string): T {
  const bytes = base64UrlDecode(value);
  return JSON.parse(new TextDecoder().decode(bytes)) as T;
}

function audienceMatches(actual: string | string[] | undefined, expected?: string): boolean {
  if (!expected) return true;
  if (Array.isArray(actual)) return actual.includes(expected);
  return actual === expected;
}

/**
 * Verify a Cloudflare Access JWT.
 *
 * PDX-23 refactor: JWKS lookup now goes through {@link fetchJwks}, which
 * caches certs in `AUTH_KV` for 24h (Phase C audit recommendation). Pass
 * the KV binding through `authKv` to opt in. When the binding is absent
 * the function falls back to the original live-fetch behaviour so this
 * stays drop-in compatible.
 *
 * The decoded payload is also surfaced (as the resolved value) so callers
 * that need `sub` / `email` (e.g. `/api/auth/session` to issue the helm
 * session JWT) don't have to decode the JWT a second time.
 */
export interface AccessVerificationResult {
  ok: boolean;
  payload?: AccessJwtPayload & { sub?: string; email?: string };
}

export async function verifyAccessJwt(
  jwt: string,
  teamDomain: string,
  expectedAudience?: string,
  authKv?: KVNamespace,
): Promise<AccessVerificationResult> {
  const [headerPart, payloadPart, signaturePart] = jwt.split(".");
  if (!headerPart || !payloadPart || !signaturePart) return { ok: false };

  const header = decodeJson<AccessJwtHeader>(headerPart);
  if (header.alg !== "RS256" || !header.kid) return { ok: false };

  const payload = decodeJson<AccessJwtPayload & { sub?: string; email?: string }>(payloadPart);
  const now = Math.floor(Date.now() / 1000);
  if (payload.exp != null && payload.exp <= now) return { ok: false };
  if (payload.nbf != null && payload.nbf > now) return { ok: false };
  if (!audienceMatches(payload.aud, expectedAudience)) return { ok: false };

  const certs = await fetchJwks(teamDomain, authKv);
  if (!certs) return { ok: false };
  const jwk = certs.keys?.find((key) => key.kid === header.kid);
  if (!jwk) return { ok: false };

  const key = await crypto.subtle.importKey(
    "jwk",
    jwk,
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["verify"],
  );

  const valid = await crypto.subtle.verify(
    "RSASSA-PKCS1-v1_5",
    key,
    base64UrlDecode(signaturePart),
    new TextEncoder().encode(`${headerPart}.${payloadPart}`),
  );
  if (!valid) return { ok: false };
  return { ok: true, payload };
}

export async function requireAccess(
  request: Request,
  access: HelmManifest["access"],
  environment: HelmEnvironment,
  authKv?: KVNamespace,
): Promise<Response | null> {
  if (!access.required) return null;

  const jwt = request.headers.get("Cf-Access-Jwt-Assertion");
  if (jwt) {
    try {
      const result = await verifyAccessJwt(
        jwt,
        access.teamDomain,
        access.audiences[environment],
        authKv,
      );
      if (result.ok) return null;
    } catch {
      return json(
        {
          error: "unauthorized",
          message: "Cloudflare Access JWT validation failed."
        },
        { status: 401 },
      );
    }
  }

  return json(
    {
      error: "unauthorized",
      message: "A valid Cloudflare Access JWT is required."
    },
    { status: 401 },
  );
}
