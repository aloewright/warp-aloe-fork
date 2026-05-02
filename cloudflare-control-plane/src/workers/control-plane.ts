import { auditInventory, createCloudflareClient, fetchInventory } from "../shared/cloudflare.js";
import {
  denyJwt,
  extractHelmJwt,
  issueHelmJwt,
  recordAuthEvent,
  requireHelmAuth,
  validateDopplerToken,
  verifyHelmJwt,
  withAuditAttribution,
  type AuthEnv,
  type AuthenticatedRequestContext
} from "../shared/auth.js";
import {
  health,
  json,
  methodNotAllowed,
  notFound,
  requireAccess,
  verifyAccessJwt
} from "../shared/http.js";
import { assertEnvironment, manifestForRuntime, type HelmEnvironment } from "../shared/manifest.js";
import { getDb, users } from "../db/index.js";
import { eq } from "drizzle-orm";

// Re-export the PDX-20 Durable Object classes so they are available to the
// Workers runtime. wrangler.control-plane.toml lists each by `class_name`.
export { SessionDO, UserDO, SwarmDO, RepoDO } from "./durable-objects/index.js";

interface Env extends AuthEnv {
  HELM_ENVIRONMENT: HelmEnvironment;
  HELM_VERSION: string;
  HELM_BUILD_ID: string;
  HELM_MANIFEST_JSON: string;
  CLOUDFLARE_API_TOKEN?: string;
  CONTROL_PLANE_REGISTRY: DurableObjectNamespace;
  DB: D1Database;
  // PDX-20 — added in this PR. Workers route via these in PDX-19.
  SESSION_DO?: DurableObjectNamespace;
  USER_DO?: DurableObjectNamespace;
  SWARM_DO?: DurableObjectNamespace;
  REPO_DO?: DurableObjectNamespace;
}

export class ControlPlaneRegistry {
  constructor(private readonly state: DurableObjectState) {}

  async fetch(): Promise<Response> {
    const initializedAt = await this.state.storage.get<string>("initializedAt");
    if (!initializedAt) {
      await this.state.storage.put("initializedAt", new Date().toISOString());
    }
    return json({ ok: true, initializedAt: initializedAt ?? "created" });
  }
}

async function resources(env: Env): Promise<Response> {
  const manifest = manifestForRuntime(env);
  assertEnvironment(manifest, env.HELM_ENVIRONMENT);
  if (!env.CLOUDFLARE_API_TOKEN) {
    return json(
      {
        manifest,
        reconciliation: {
          skipped: true,
          reason: "CLOUDFLARE_API_TOKEN is not configured."
        }
      },
      { status: 200 },
    );
  }

  const client = createCloudflareClient(env.CLOUDFLARE_API_TOKEN);
  const inventory = await fetchInventory(client, manifest.accountId);
  return json({
    manifest,
    reconciliation: auditInventory(manifest, env.HELM_ENVIRONMENT, inventory)
  });
}

async function onboardingCheck(request: Request, env: Env): Promise<Response> {
  if (request.method !== "POST") return methodNotAllowed();

  const manifest = manifestForRuntime(env);
  const body = (await request.json().catch(() => ({}))) as { environment?: string };
  const targetEnvironment = body.environment ?? env.HELM_ENVIRONMENT;
  assertEnvironment(manifest, targetEnvironment);

  return json({
    environment: targetEnvironment,
    account: {
      id: manifest.accountId,
      configured: !manifest.accountId.startsWith("replace-with")
    },
    zone: {
      id: manifest.zone.id,
      domain: manifest.zone.domain,
      configured: !manifest.zone.id.startsWith("replace-with")
    },
    access: {
      required: manifest.access.required,
      teamDomain: manifest.access.teamDomain,
      audienceConfigured: Boolean(manifest.access.audiences[targetEnvironment])
    },
    resources: {
      d1: Object.keys(manifest.resources.d1).filter((name) =>
        manifest.resources.d1[name]?.environment === targetEnvironment ||
        manifest.resources.d1[name]?.environment === "all"
      ),
      r2: Object.keys(manifest.resources.r2).filter((name) =>
        manifest.resources.r2[name]?.environment === targetEnvironment ||
        manifest.resources.r2[name]?.environment === "all"
      ),
      durableObjects: Object.keys(manifest.resources.durableObjects)
    },
    containers: manifest.containers[targetEnvironment]
  });
}

// ── Auth routes (PDX-23) ────────────────────────────────────────────────────

/**
 * Exchange a Cloudflare Access JWT (or a Doppler service token, when
 * Access is not required by the manifest) for a short-lived helm session
 * JWT. The session JWT is what downstream Workers — agent-runtime,
 * workflows — accept on every subsequent request.
 *
 * Records both success and rejection in `audit_log`.
 */
async function authSession(request: Request, env: Env): Promise<Response> {
  if (request.method !== "POST") return methodNotAllowed();
  if (!env.HELM_JWT_SIGNING_KEY) {
    return json(
      { error: "misconfigured", message: "HELM_JWT_SIGNING_KEY is not set." },
      { status: 500 }
    );
  }

  const manifest = manifestForRuntime(env);
  const ip = request.headers.get("CF-Connecting-IP") ?? null;
  const userAgent = request.headers.get("User-Agent") ?? null;

  // Path 1 — Cloudflare Access JWT (preferred).
  const accessJwt = request.headers.get("Cf-Access-Jwt-Assertion");
  if (accessJwt && manifest.access.required) {
    const result = await verifyAccessJwt(
      accessJwt,
      manifest.access.teamDomain,
      manifest.access.audiences[env.HELM_ENVIRONMENT],
      env.AUTH_KV
    );
    if (!result.ok || !result.payload?.sub) {
      return json({ error: "unauthorized", reason: "access_invalid" }, { status: 401 });
    }
    const userId = await ensureUser(env, result.payload.sub, result.payload.email);
    const issued = await issueHelmJwt({
      userId,
      signingKey: env.HELM_JWT_SIGNING_KEY
    });
    await recordAuthEvent(env, {
      userId,
      action: "auth.session.issued",
      jti: issued.payload.jti,
      details: { ip, user_agent: userAgent, source: "access" }
    });
    return json({
      token: issued.token,
      expiresAt: issued.payload.exp,
      jti: issued.payload.jti
    });
  }

  // Path 2 — Doppler fallback. Only enabled when Access is not required.
  if (!manifest.access.required) {
    const dopplerToken = request.headers.get("X-Doppler-Token");
    if (dopplerToken) {
      const validation = await validateDopplerToken(dopplerToken);
      if (!validation.ok || !validation.project) {
        return json(
          { error: "unauthorized", reason: validation.reason ?? "doppler_invalid" },
          { status: 401 }
        );
      }
      const userId = `doppler:${validation.project}`;
      const issued = await issueHelmJwt({
        userId,
        signingKey: env.HELM_JWT_SIGNING_KEY,
        scope: `doppler:${validation.project}`
      });
      await recordAuthEvent(env, {
        userId: null, // Doppler principals are not in `users`.
        action: "auth.session.issued.doppler_fallback",
        jti: issued.payload.jti,
        details: {
          ip,
          user_agent: userAgent,
          project: validation.project,
          source: "doppler"
        }
      });
      return json({
        token: issued.token,
        expiresAt: issued.payload.exp,
        jti: issued.payload.jti,
        scope: `doppler:${validation.project}`
      });
    }
  }

  return json(
    { error: "unauthorized", message: "An Access JWT or Doppler token is required." },
    { status: 401 }
  );
}

/**
 * Revoke the caller's helm session JWT by writing its `jti` to the
 * `AUTH_KV` denylist. Records the revocation in `audit_log`.
 */
async function authLogout(request: Request, env: Env): Promise<Response> {
  if (request.method !== "POST") return methodNotAllowed();
  const token = extractHelmJwt(request);
  if (!token) return json({ error: "no_token" }, { status: 400 });
  if (!env.HELM_JWT_SIGNING_KEY) {
    return json(
      { error: "misconfigured", message: "HELM_JWT_SIGNING_KEY is not set." },
      { status: 500 }
    );
  }
  const result = await verifyHelmJwt(token, {
    signingKey: env.HELM_JWT_SIGNING_KEY,
    authKv: env.AUTH_KV
  });
  if (!result.ok) {
    // Logging out an already-bad token is a no-op success — clients shouldn't
    // get stuck because their token expired one second before they hit logout.
    return json({ revoked: false, reason: result.reason });
  }
  if (env.AUTH_KV) {
    await denyJwt(env.AUTH_KV, result.payload.jti, result.payload.exp);
  }
  await recordAuthEvent(env, {
    userId: result.payload.sub.startsWith("doppler:") ? null : result.payload.sub,
    action: "auth.session.revoked",
    jti: result.payload.jti,
    details: { source: result.payload.scope ? "doppler" : "helm" }
  });
  return json({ revoked: true, jti: result.payload.jti });
}

/**
 * Upsert a user row keyed on the Access `sub`. Returns the canonical
 * user_id used in audit_log attribution.
 *
 * For minimal coupling we use `sub` directly as the primary key; emails
 * may rotate, sub doesn't. If a future PR moves to UUID-keyed rows
 * lookup-by-sub we'll plug in here.
 */
async function ensureUser(env: Env, sub: string, email?: string): Promise<string> {
  const db = getDb(env);
  const existing = await db.select().from(users).where(eq(users.id, sub)).all();
  if (existing.length === 0) {
    await db
      .insert(users)
      .values({ id: sub, email: email ?? `${sub}@unknown.local` })
      .onConflictDoNothing();
  }
  return sub;
}

// ── Worker entry point ──────────────────────────────────────────────────────

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    if (url.pathname === "/api/health") {
      return health({
        service: "helm-control-plane",
        environment: env.HELM_ENVIRONMENT,
        version: env.HELM_VERSION,
        buildId: env.HELM_BUILD_ID
      });
    }

    // Auth routes are special: `/api/auth/session` must be reachable with
    // a Cloudflare Access JWT (we *exchange* it here), so we don't gate
    // it behind `requireHelmAuth`. Instead it goes through `requireAccess`
    // when Access is required by the manifest.
    if (url.pathname === "/api/auth/session") {
      const manifest = manifestForRuntime(env);
      const accessFailure = await requireAccess(
        request,
        manifest.access,
        env.HELM_ENVIRONMENT,
        env.AUTH_KV
      );
      if (accessFailure && manifest.access.required) return accessFailure;
      return authSession(request, env);
    }

    if (url.pathname === "/api/auth/logout") {
      // Logout doesn't need Access — the helm JWT is enough proof of identity.
      return authLogout(request, env);
    }

    // All other API routes require a helm session JWT. We resolve the
    // caller, then dispatch through `withAuditAttribution` so each route
    // automatically writes a row to `audit_log`.
    const manifest = manifestForRuntime(env);
    const auth = await requireHelmAuth(request, env, manifest, env.HELM_ENVIRONMENT);
    if ("response" in auth) return auth.response;

    const dispatch = withAuditAttribution<Env>(async (req, e, _ctx) => {
      const u = new URL(req.url);
      if (u.pathname === "/api/environments") {
        if (req.method !== "GET") return methodNotAllowed();
        return json({
          environments: manifest.environments.map((environment) => ({
            name: environment,
            configured: Boolean(manifest.access.audiences[environment]),
            containers: manifest.containers[environment]
          }))
        });
      }
      if (u.pathname === "/api/onboarding/check") {
        return onboardingCheck(req, e);
      }
      if (u.pathname === "/api/resources") {
        if (req.method !== "GET") return methodNotAllowed();
        return resources(e);
      }
      return notFound();
    });

    return dispatch(request, env, auth.ctx);
  }
};

// Re-export the auth context type so downstream tests can import it
// without reaching into `../shared/auth.js`.
export type { AuthenticatedRequestContext };
