/**
 * Hono application that powers the helm control-plane Worker (PDX-19).
 *
 * Routing tree
 * ────────────
 *   Public:
 *     GET  /api/health
 *     POST /api/auth/session   (Cf-Access-Jwt-Assertion or X-Doppler-Token)
 *     POST /api/auth/logout
 *     POST /api/webhooks/github
 *     POST /api/webhooks/slack
 *     POST /api/webhooks/generic
 *
 *   Authenticated (helm session JWT, every request audited via withAuditAttribution):
 *     GET  /api/environments
 *     POST /api/onboarding/check
 *     GET  /api/resources
 *     POST /api/workflows/:slug/instances
 *     POST /api/workflows/deploy/instances/:id/approve
 *     GET  /api/workflows/:slug/instances/:id
 *     GET  /api/sessions/:sessionId/ws       (WebSocket upgrade → SessionDO)
 *
 * The Hono app is exported separately from the Worker entry so:
 *   - Tests can mount it against a `Request` without a full Workers runtime.
 *   - Future helm-cloud extraction can pick up `app.ts` verbatim and pair
 *     it with a different Worker shell (durable-objects/workflows live in
 *     this monorepo today; once they move, only `control-plane.ts` shifts).
 */
import { Hono } from "hono";
import { eq } from "drizzle-orm";

import {
  denyJwt,
  extractHelmJwt,
  issueHelmJwt,
  recordAuthEvent,
  validateDopplerToken,
  verifyHelmJwt,
  type AuthEnv,
  type AuthenticatedRequestContext
} from "../shared/auth.js";
import {
  json,
  notFound,
  requireAccess,
  verifyAccessJwt
} from "../shared/http.js";
import {
  assertEnvironment,
  manifestForRuntime,
  type HelmEnvironment
} from "../shared/manifest.js";
import {
  auditInventory,
  createCloudflareClient,
  fetchInventory
} from "../shared/cloudflare.js";
import {
  verifyGenericSignature,
  verifyGitHubSignature,
  verifySlackSignature
} from "../shared/webhooks.js";
import { auditLog, getDb, users } from "../db/index.js";
import { resolveWorkflowBinding, type WorkflowBinding } from "./workflows/index.js";
import type {
  DeployWorkflowParams,
  SwarmWorkflowParams
} from "./workflows/types.js";

// ── Env shape ───────────────────────────────────────────────────────────────

/** Minimal Durable Object namespace we need from the SessionDO binding (PDX-20). */
export interface SessionDoNamespace {
  idFromName(name: string): DurableObjectId;
  get(id: DurableObjectId): { fetch(request: Request): Promise<Response> };
}

export interface ControlPlaneEnv extends AuthEnv {
  HELM_ENVIRONMENT: HelmEnvironment;
  HELM_VERSION: string;
  HELM_BUILD_ID: string;
  HELM_MANIFEST_JSON: string;
  CLOUDFLARE_API_TOKEN?: string;
  CONTROL_PLANE_REGISTRY: DurableObjectNamespace;
  DB: D1Database;

  // Optional bindings — present once PDX-20 / PDX-25 land alongside this Worker.
  SESSION_DO?: SessionDoNamespace;
  SWARM_WORKFLOW?: WorkflowBinding;
  DEPLOY_WORKFLOW?: WorkflowBinding;
  SCHEDULED_TASK_WORKFLOW?: WorkflowBinding;
  WATCHDOG_WORKFLOW?: WorkflowBinding;

  // Webhook secrets (wrangler secret put …). Each is optional — the route
  // returns 503 with `webhook_disabled` if its secret isn't configured, so
  // hooks can be enabled per-environment without code changes.
  GITHUB_WEBHOOK_SECRET?: string;
  SLACK_WEBHOOK_SECRET?: string;
  GENERIC_WEBHOOK_SECRET?: string;
}

// ── Hono app variables ──────────────────────────────────────────────────────

interface AppVariables {
  /** Authenticated principal context populated by `helmAuth` middleware. */
  authCtx: AuthenticatedRequestContext;
}

// Hono is parameterised on a `Bindings` (env) and `Variables` (per-request) shape.
type AppEnv = { Bindings: ControlPlaneEnv; Variables: AppVariables };

// ── Helpers ────────────────────────────────────────────────────────────────

/**
 * Extract the helm session JWT from either the `Authorization: Bearer …`
 * header (programmatic clients) or the `?token=…` query parameter
 * (browser-initiated WebSocket upgrades, which can't set custom headers).
 */
function extractHelmJwtFromRequestOrQuery(req: Request): string | null {
  const headerToken = extractHelmJwt(req);
  if (headerToken) return headerToken;
  const url = new URL(req.url);
  const queryToken = url.searchParams.get("token");
  return queryToken ?? null;
}

async function ensureUser(env: ControlPlaneEnv, sub: string, email?: string): Promise<string> {
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

// ── Middleware: requireHelmAuth + withAuditAttribution composed for Hono ────

/**
 * Hono adapter for `requireHelmAuth` (PDX-23). Either populates `c.var.authCtx`
 * and calls `next()`, or short-circuits with a 401. The helm JWT may come from
 * the `Authorization` header *or* the `?token=` query parameter so browser
 * WebSocket clients (which can't set custom headers) work.
 */
async function helmAuth(c: import("hono").Context<AppEnv>, next: () => Promise<void>): Promise<Response | void> {
  const env = c.env;
  const manifest = manifestForRuntime(env);

  const helmToken = extractHelmJwtFromRequestOrQuery(c.req.raw);
  if (helmToken) {
    if (!env.HELM_JWT_SIGNING_KEY) {
      return c.json(
        { error: "misconfigured", message: "HELM_JWT_SIGNING_KEY is not set." },
        500
      );
    }
    const result = await verifyHelmJwt(helmToken, {
      signingKey: env.HELM_JWT_SIGNING_KEY,
      authKv: env.AUTH_KV
    });
    if (result.ok) {
      c.set("authCtx", {
        userId: result.payload.sub,
        jti: result.payload.jti,
        source: "helm",
        scope: result.payload.scope
      });
      await next();
      return;
    }
    return c.json({ error: "unauthorized", reason: result.reason }, 401);
  }

  // Doppler fallback only kicks in when Access is *not* required.
  if (!manifest.access.required) {
    const dopplerToken = c.req.header("X-Doppler-Token");
    if (dopplerToken) {
      const validation = await validateDopplerToken(dopplerToken);
      if (validation.ok && validation.project) {
        c.set("authCtx", {
          userId: `doppler:${validation.project}`,
          source: "doppler",
          scope: `doppler:${validation.project}`
        });
        await next();
        return;
      }
      return c.json(
        { error: "unauthorized", reason: validation.reason ?? "doppler_invalid" },
        401
      );
    }
  }

  return c.json(
    { error: "unauthorized", message: "A helm session JWT is required." },
    401
  );
}

/**
 * Hono adapter for `withAuditAttribution` (PDX-23). Runs the downstream
 * handler chain (`next()`), then writes a row to `audit_log` keyed on the
 * authenticated principal. Anonymous requests (e.g. the public webhook
 * endpoints) get `user_id = null` and `source = "anonymous"`.
 *
 * We deliberately don't wrap with `withAuditAttribution` directly — that
 * helper expects to *own* the inner handler call. Hono's middleware contract
 * is `next()`-based, so we replicate the audit-row write here against the
 * same `audit_log` shape `withAuditAttribution` writes. Failures swallowed.
 */
function audit(opts: { anonymous?: boolean } = {}) {
  return async (c: import("hono").Context<AppEnv>, next: () => Promise<void>): Promise<void> => {
    const ctx: AuthenticatedRequestContext = opts.anonymous
      ? { userId: null, source: "anonymous" }
      : c.var.authCtx;
    const startedAt = Date.now();
    const url = new URL(c.req.url);
    let threw: unknown = null;
    try {
      await next();
    } catch (err) {
      threw = err;
      c.res = json(
        { error: "internal_error", message: (err as Error).message },
        { status: 500 }
      );
    }
    const status = c.res?.status ?? 200;
    try {
      await getDb(c.env).insert(auditLog).values({
        id: crypto.randomUUID(),
        userId: ctx.userId,
        action: "http.request",
        targetKind: "endpoint",
        targetId: url.pathname,
        details: {
          method: c.req.method,
          path: url.pathname,
          status,
          durationMs: Date.now() - startedAt,
          source: ctx.source,
          ...(ctx.scope ? { scope: ctx.scope } : {})
        }
      });
    } catch {
      // Audit failures must not break the request path.
    }
    if (threw && !c.res) throw threw;
  };
}

// ── App factory ────────────────────────────────────────────────────────────

export function createApp(): Hono<AppEnv> {
  const app = new Hono<AppEnv>();

  // ── Health ──────────────────────────────────────────────────────────────
  app.get("/api/health", (c) => {
    return c.json({
      service: "helm-control-plane",
      environment: c.env.HELM_ENVIRONMENT,
      version: c.env.HELM_VERSION,
      buildId: c.env.HELM_BUILD_ID
    });
  });

  // ── Auth: session exchange ──────────────────────────────────────────────
  // Public endpoint — gated by Cloudflare Access (when the manifest requires
  // it) rather than helm JWT, since the whole point is to *issue* a helm JWT.
  app.post("/api/auth/session", async (c) => {
    const env = c.env;
    if (!env.HELM_JWT_SIGNING_KEY) {
      return c.json(
        { error: "misconfigured", message: "HELM_JWT_SIGNING_KEY is not set." },
        500
      );
    }

    const manifest = manifestForRuntime(env);
    const accessFailure = await requireAccess(
      c.req.raw,
      manifest.access,
      env.HELM_ENVIRONMENT,
      env.AUTH_KV
    );
    if (accessFailure && manifest.access.required) return accessFailure;

    const ip = c.req.header("CF-Connecting-IP") ?? null;
    const userAgent = c.req.header("User-Agent") ?? null;

    // Path 1 — Cloudflare Access JWT.
    const accessJwt = c.req.header("Cf-Access-Jwt-Assertion");
    if (accessJwt && manifest.access.required) {
      const result = await verifyAccessJwt(
        accessJwt,
        manifest.access.teamDomain,
        manifest.access.audiences[env.HELM_ENVIRONMENT],
        env.AUTH_KV
      );
      if (!result.ok || !result.payload?.sub) {
        return c.json({ error: "unauthorized", reason: "access_invalid" }, 401);
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
      return c.json({
        token: issued.token,
        expiresAt: issued.payload.exp,
        jti: issued.payload.jti
      });
    }

    // Path 2 — Doppler fallback.
    if (!manifest.access.required) {
      const dopplerToken = c.req.header("X-Doppler-Token");
      if (dopplerToken) {
        const validation = await validateDopplerToken(dopplerToken);
        if (!validation.ok || !validation.project) {
          return c.json(
            { error: "unauthorized", reason: validation.reason ?? "doppler_invalid" },
            401
          );
        }
        const userId = `doppler:${validation.project}`;
        const issued = await issueHelmJwt({
          userId,
          signingKey: env.HELM_JWT_SIGNING_KEY,
          scope: `doppler:${validation.project}`
        });
        await recordAuthEvent(env, {
          userId: null,
          action: "auth.session.issued.doppler_fallback",
          jti: issued.payload.jti,
          details: {
            ip,
            user_agent: userAgent,
            project: validation.project,
            source: "doppler"
          }
        });
        return c.json({
          token: issued.token,
          expiresAt: issued.payload.exp,
          jti: issued.payload.jti,
          scope: `doppler:${validation.project}`
        });
      }
    }

    return c.json(
      { error: "unauthorized", message: "An Access JWT or Doppler token is required." },
      401
    );
  });

  // ── Auth: logout ────────────────────────────────────────────────────────
  app.post("/api/auth/logout", async (c) => {
    const env = c.env;
    const token = extractHelmJwt(c.req.raw);
    if (!token) return c.json({ error: "no_token" }, 400);
    if (!env.HELM_JWT_SIGNING_KEY) {
      return c.json(
        { error: "misconfigured", message: "HELM_JWT_SIGNING_KEY is not set." },
        500
      );
    }
    const result = await verifyHelmJwt(token, {
      signingKey: env.HELM_JWT_SIGNING_KEY,
      authKv: env.AUTH_KV
    });
    if (!result.ok) {
      return c.json({ revoked: false, reason: result.reason });
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
    return c.json({ revoked: true, jti: result.payload.jti });
  });

  // ── Webhooks (PDX-19 stubs) ─────────────────────────────────────────────
  // Public — auth is the HMAC, not a helm JWT. Each route validates the
  // signature against the per-source secret, logs minimal metadata, and
  // returns 202. Event handling is deliberately deferred (TODO PDX-26).
  app.post("/api/webhooks/github", audit({ anonymous: true }), async (c) => {
    const env = c.env;
    if (!env.GITHUB_WEBHOOK_SECRET) {
      return c.json({ error: "webhook_disabled", reason: "no_secret" }, 503);
    }
    const rawBody = await c.req.text();
    const ok = await verifyGitHubSignature(
      env.GITHUB_WEBHOOK_SECRET,
      rawBody,
      c.req.header("X-Hub-Signature-256") ?? null
    );
    if (!ok) return c.json({ error: "bad_signature" }, 401);
    const event = c.req.header("X-GitHub-Event") ?? "unknown";
    const delivery = c.req.header("X-GitHub-Delivery") ?? null;
    // TODO(PDX-26): dispatch into the cloud port of crates/github_webhook_receiver.
    console.log(`[webhook][github] event=${event} delivery=${delivery ?? "-"}`);
    return c.json({ accepted: true, event, delivery }, 202);
  });

  app.post("/api/webhooks/slack", audit({ anonymous: true }), async (c) => {
    const env = c.env;
    if (!env.SLACK_WEBHOOK_SECRET) {
      return c.json({ error: "webhook_disabled", reason: "no_secret" }, 503);
    }
    const rawBody = await c.req.text();
    const ok = await verifySlackSignature(
      env.SLACK_WEBHOOK_SECRET,
      rawBody,
      c.req.header("X-Slack-Signature") ?? null,
      c.req.header("X-Slack-Request-Timestamp") ?? null
    );
    if (!ok) return c.json({ error: "bad_signature" }, 401);
    // TODO(PDX-26): route Slack events into the symphony pipeline.
    console.log(`[webhook][slack] payload_bytes=${rawBody.length}`);
    return c.json({ accepted: true }, 202);
  });

  app.post("/api/webhooks/generic", audit({ anonymous: true }), async (c) => {
    const env = c.env;
    if (!env.GENERIC_WEBHOOK_SECRET) {
      return c.json({ error: "webhook_disabled", reason: "no_secret" }, 503);
    }
    const rawBody = await c.req.text();
    const ok = await verifyGenericSignature(
      env.GENERIC_WEBHOOK_SECRET,
      rawBody,
      c.req.header("X-Webhook-Signature") ?? null
    );
    if (!ok) return c.json({ error: "bad_signature" }, 401);
    const source = c.req.header("X-Webhook-Source") ?? "unknown";
    // TODO(PDX-26): per-source dispatch.
    console.log(`[webhook][generic] source=${source} payload_bytes=${rawBody.length}`);
    return c.json({ accepted: true, source }, 202);
  });

  // ── Authenticated subtree ───────────────────────────────────────────────
  // Every protected route gets:
  //   1. helmAuth  — verify helm JWT (header or ?token=)
  //   2. audit()   — write the request into audit_log keyed on userId
  app.use("/api/environments", helmAuth, audit());
  app.use("/api/onboarding/check", helmAuth, audit());
  app.use("/api/resources", helmAuth, audit());
  app.use("/api/workflows/*", helmAuth, audit());
  app.use("/api/sessions/*", helmAuth, audit());

  app.get("/api/environments", (c) => {
    const manifest = manifestForRuntime(c.env);
    return c.json({
      environments: manifest.environments.map((environment) => ({
        name: environment,
        configured: Boolean(manifest.access.audiences[environment]),
        containers: manifest.containers[environment]
      }))
    });
  });

  app.post("/api/onboarding/check", async (c) => {
    const env = c.env;
    const manifest = manifestForRuntime(env);
    const body = (await c.req.json().catch(() => ({}))) as { environment?: string };
    const targetEnvironment = body.environment ?? env.HELM_ENVIRONMENT;
    assertEnvironment(manifest, targetEnvironment);
    return c.json({
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
        d1: Object.keys(manifest.resources.d1).filter(
          (name) =>
            manifest.resources.d1[name]?.environment === targetEnvironment ||
            manifest.resources.d1[name]?.environment === "all"
        ),
        r2: Object.keys(manifest.resources.r2).filter(
          (name) =>
            manifest.resources.r2[name]?.environment === targetEnvironment ||
            manifest.resources.r2[name]?.environment === "all"
        ),
        durableObjects: Object.keys(manifest.resources.durableObjects)
      },
      containers: manifest.containers[targetEnvironment]
    });
  });

  app.get("/api/resources", async (c) => {
    const env = c.env;
    const manifest = manifestForRuntime(env);
    assertEnvironment(manifest, env.HELM_ENVIRONMENT);
    if (!env.CLOUDFLARE_API_TOKEN) {
      return c.json({
        manifest,
        reconciliation: {
          skipped: true,
          reason: "CLOUDFLARE_API_TOKEN is not configured."
        }
      });
    }
    const client = createCloudflareClient(env.CLOUDFLARE_API_TOKEN);
    const inventory = await fetchInventory(client, manifest.accountId);
    return c.json({
      manifest,
      reconciliation: auditInventory(manifest, env.HELM_ENVIRONMENT, inventory)
    });
  });

  // ── Workflows (PDX-25) ──────────────────────────────────────────────────
  // The control-plane Worker proxies into the workflow bindings declared in
  // wrangler.control-plane.toml. The workflows Worker still owns the cron
  // handler — this is just the synchronous create / approve / status surface
  // exposed under one origin.
  app.post("/api/workflows/:slug/instances", async (c) => {
    const env = c.env;
    const slug = c.req.param("slug");
    const binding = resolveWorkflowBinding(env as never, slug);
    if (!binding) return c.notFound();
    const body = (await c.req.json().catch(() => ({}))) as {
      id?: string;
      params?: unknown;
    };
    if (slug === "swarm") {
      const p = body.params as Partial<SwarmWorkflowParams> | undefined;
      if (!p?.swarmId || !p?.taskId || !Array.isArray(p?.agents)) {
        return c.json({ error: "invalid swarm params" }, 400);
      }
    } else if (slug === "deploy") {
      const p = body.params as Partial<DeployWorkflowParams> | undefined;
      if (!p?.deployId || !p?.target || !p?.artifact || !Array.isArray(p?.approvers)) {
        return c.json({ error: "invalid deploy params" }, 400);
      }
    }
    const instance = await binding.create({ id: body.id, params: body.params });
    return c.json({ id: instance.id, slug }, 201);
  });

  app.post("/api/workflows/deploy/instances/:id/approve", async (c) => {
    const env = c.env;
    if (!env.DEPLOY_WORKFLOW) return c.notFound();
    const id = c.req.param("id");
    const body = (await c.req.json().catch(() => ({}))) as {
      approval?: { approver?: string; approvedAt?: string };
    };
    if (!body.approval?.approver || !body.approval?.approvedAt) {
      return c.json({ error: "invalid approval payload" }, 400);
    }
    const instance = await env.DEPLOY_WORKFLOW.get(id);
    await instance.sendEvent({ type: "approval", payload: body.approval });
    return c.json({ id, accepted: true });
  });

  app.get("/api/workflows/:slug/instances/:id", async (c) => {
    const env = c.env;
    const slug = c.req.param("slug");
    const id = c.req.param("id");
    const binding = resolveWorkflowBinding(env as never, slug);
    if (!binding) return c.notFound();
    const instance = await binding.get(id);
    const status = await instance.status();
    return c.json({ id, slug, status: status.status });
  });

  // ── WebSocket → SessionDO (PDX-19 + PDX-20) ────────────────────────────
  // GET /api/sessions/:sessionId/ws — forward the upgrade to the per-session
  // Durable Object. The DO reads `x-helm-user-id` and `x-helm-session-id`
  // from the rewritten request for D1 attribution. We intentionally do NOT
  // strip the original `Upgrade` / `Sec-WebSocket-*` headers — the DO is
  // expected to call `acceptWebSocket` against the inbound connection.
  app.get("/api/sessions/:sessionId/ws", async (c) => {
    const env = c.env;
    if (!env.SESSION_DO) {
      return c.json({ error: "session_do_unbound" }, 503);
    }
    const upgrade = c.req.header("Upgrade")?.toLowerCase();
    if (upgrade !== "websocket") {
      return c.json({ error: "expected_websocket_upgrade" }, 426);
    }
    const sessionId = c.req.param("sessionId");
    const ctx = c.var.authCtx;
    const id = env.SESSION_DO.idFromName(sessionId);
    const stub = env.SESSION_DO.get(id);

    // Rewrite the request with attribution headers — the DO uses these for
    // D1 inserts so it doesn't have to re-verify the JWT. We construct the
    // forwarded Request from URL + init rather than `new Request(req, ...)`
    // because the GET-with-WebSocket-upgrade case doesn't carry a body and
    // some runtimes refuse to clone the original.
    const headers = new Headers(c.req.raw.headers);
    if (ctx.userId) headers.set("x-helm-user-id", ctx.userId);
    headers.set("x-helm-session-id", sessionId);
    const forwarded = new Request(c.req.raw.url, {
      method: c.req.raw.method,
      headers,
      // GET upgrades have no body, but pass it through if present (e.g. test
      // harness with a stubbed body).
      body: c.req.raw.method === "GET" || c.req.raw.method === "HEAD"
        ? undefined
        : c.req.raw.body,
      // @ts-expect-error: `duplex` is required when streaming a body in undici
      // but absent on standard RequestInit; harmless on Workers runtime.
      duplex: "half"
    });
    return stub.fetch(forwarded);
  });

  // ── 404 fallback ────────────────────────────────────────────────────────
  app.all("*", () => notFound());

  return app;
}

/** Singleton lazily created on first request — Hono apps are stateless. */
let cachedApp: Hono<AppEnv> | null = null;
export function appSingleton(): Hono<AppEnv> {
  if (!cachedApp) cachedApp = createApp();
  return cachedApp;
}
