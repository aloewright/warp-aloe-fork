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
 *     POST /api/audit/sync
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
import { and, eq } from "drizzle-orm";

import {
  denyJwt,
  denyShareToken,
  extractHelmJwt,
  issueHelmJwt,
  issueShareToken,
  recordAuthEvent,
  validateDopplerToken,
  verifyHelmJwt,
  type AuthEnv,
  type AuthenticatedRequestContext
} from "../shared/auth.js";
import {
  PUBLIC_USER_ID,
  ensurePublicUser,
  ensureShareHandle,
  recordShareAccess,
  recordShareGrant,
  recordShareRevoke,
  resolveOwnerUserId,
  shareHandleId,
  type ShareableKind,
  type SharePermission
} from "../shared/share_auth.js";
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
import { auditLog, getDb, shares, users } from "../db/index.js";
import { resolveWorkflowBinding, type WorkflowBinding } from "./workflows/index.js";
import { handleAuditSync, type AuditMirrorEnv } from "./audit_mirror.js";
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

/**
 * Minimal UserDO namespace contract for share-broadcast fan-out (PDX-116).
 * The full UserDO surface lives in `durable-objects/user-do.ts`; we only
 * need `fetch` here because we POST to `/broadcast` via the stub.
 */
export interface UserDoNamespace {
  idFromName(name: string): DurableObjectId;
  get(id: DurableObjectId): { fetch(request: Request): Promise<Response> };
}

export interface ControlPlaneEnv extends AuthEnv, AuditMirrorEnv {
  HELM_ENVIRONMENT: HelmEnvironment;
  HELM_VERSION: string;
  HELM_BUILD_ID: string;
  HELM_MANIFEST_JSON: string;
  CLOUDFLARE_API_TOKEN?: string;
  CONTROL_PLANE_REGISTRY: DurableObjectNamespace;
  DB: D1Database;

  // Optional bindings — present once PDX-20 / PDX-25 land alongside this Worker.
  SESSION_DO?: SessionDoNamespace;
  USER_DO?: UserDoNamespace;
  SWARM_WORKFLOW?: WorkflowBinding;
  DEPLOY_WORKFLOW?: WorkflowBinding;
  SCHEDULED_TASK_WORKFLOW?: WorkflowBinding;
  WATCHDOG_WORKFLOW?: WorkflowBinding;

  /** PDX-116 — separate signing key for share-link tokens. Optional. */
  HELM_SHARE_TOKEN_SIGNING_KEY?: string;

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
  app.use("/api/audit/*", helmAuth, audit());
  app.use("/api/workflows/*", helmAuth, audit());
  app.use("/api/sessions/*", helmAuth, audit());
  app.use("/api/workspaces/*", helmAuth, audit());
  app.use("/api/runs/*", helmAuth, audit());
  app.use("/api/audit/*", helmAuth, audit());

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

  // ── Audit log mirror (PDX-115) ──────────────────────────────────────────
  // Local symphony daemons POST batches of JSONL audit rows here; we insert
  // them into the D1 `audit_log` table for cloud-side query (Grafana panel,
  // dashboard, soak harness inspector). Authenticated like other protected
  // surfaces — `audit({})` middleware writes its own attribution row, and
  // the handler is responsible for parameterised insert.
  app.post("/api/audit/sync", (c) => {
    return handleAuditSync(c.req.raw, c.env as ControlPlaneEnv);
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

  // ── Sharing model (PDX-116 [E4]) ────────────────────────────────────────
  // POST   /api/workspaces/:id/shares          — grant read/write to a user or public link
  // GET    /api/workspaces/:id/shares          — list active shares
  // DELETE /api/workspaces/:id/shares/:shareId — revoke
  // POST   /api/sessions/:id/shares            — same shape, finer scope
  // POST   /api/runs/:id/shares                — single transcript share
  //
  // Authentication for all of these is the helm session JWT (already
  // enforced by `helmAuth` registered above). The handlers verify the
  // caller owns the entity before granting/listing/revoking. Public-link
  // share tokens are validated by `share_auth.ts` on access — the grant
  // route always requires an authenticated owner.

  app.post("/api/workspaces/:id/shares", (c) => handleCreateShare(c, "workspace"));
  app.get("/api/workspaces/:id/shares", (c) => handleListShares(c, "workspace"));
  app.delete("/api/workspaces/:id/shares/:shareId", (c) =>
    handleRevokeShare(c, "workspace")
  );
  app.post("/api/sessions/:id/shares", (c) => handleCreateShare(c, "session"));
  app.post("/api/runs/:id/shares", (c) => handleCreateShare(c, "run"));

  // ── 404 fallback ────────────────────────────────────────────────────────
  app.all("*", () => notFound());

  return app;
}

// ── Share route helpers (PDX-116) ──────────────────────────────────────────

const PERMISSION_VALUES: ReadonlySet<SharePermission> = new Set([
  "read",
  "write",
  "admin"
]);

function ensureOwnerCtx(
  ctx: AuthenticatedRequestContext
): { ok: true; userId: string } | { ok: false; status: number; reason: string } {
  if (!ctx.userId) {
    return { ok: false, status: 401, reason: "no_user" };
  }
  return { ok: true, userId: ctx.userId };
}

async function broadcastShareGranted(
  env: ControlPlaneEnv,
  recipientUserId: string,
  payload: {
    shareId: string;
    kind: ShareableKind;
    targetId: string;
    permission: SharePermission;
  }
): Promise<void> {
  if (!env.USER_DO) return;
  if (recipientUserId === PUBLIC_USER_ID) return;
  try {
    const id = env.USER_DO.idFromName(recipientUserId);
    const stub = env.USER_DO.get(id);
    await stub.fetch(
      new Request("https://user-do.local/broadcast", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          userId: recipientUserId,
          event: {
            kind: "created",
            resourceId: shareHandleId(payload.kind, payload.targetId),
            payload: {
              type: "share_granted",
              shareId: payload.shareId,
              resource: { kind: payload.kind, id: payload.targetId },
              permission: payload.permission
            }
          }
        })
      })
    );
  } catch (err) {
    console.log(`[share] broadcast failed: ${String(err)}`);
  }
}

async function handleCreateShare(
  c: import("hono").Context<AppEnv>,
  kind: ShareableKind
): Promise<Response> {
  const env = c.env;
  const ownerCheck = ensureOwnerCtx(c.var.authCtx);
  if (!ownerCheck.ok) {
    return c.json({ error: "unauthorized", reason: ownerCheck.reason }, 401);
  }
  const targetId = c.req.param("id");
  if (!targetId) return c.json({ error: "missing_id" }, 400);

  const body = (await c.req.json().catch(() => ({}))) as {
    shared_with?: string;
    permission?: string;
  };
  const sharedWith = body.shared_with;
  const permission = body.permission as SharePermission | undefined;
  if (!sharedWith || typeof sharedWith !== "string") {
    return c.json({ error: "invalid_payload", reason: "shared_with_required" }, 400);
  }
  if (!permission || !PERMISSION_VALUES.has(permission)) {
    return c.json({ error: "invalid_payload", reason: "permission_required" }, 400);
  }

  const db = getDb(env);
  const ownerUserId = await resolveOwnerUserId(db, kind, targetId);
  if (!ownerUserId) return c.json({ error: "not_found" }, 404);
  if (ownerUserId !== ownerCheck.userId) {
    return c.json({ error: "forbidden", reason: "not_owner" }, 403);
  }

  // Materialize a share-handle resource row so the FK on shares.resource_id
  // resolves. Idempotent.
  const handleId = await ensureShareHandle(db, kind, targetId, ownerUserId);

  // Resolve the recipient: either an existing user_id, or the synthetic
  // __public__ user (lazily created).
  let recipientUserId: string;
  let isPublic = false;
  if (sharedWith === "public") {
    await ensurePublicUser(db);
    recipientUserId = PUBLIC_USER_ID;
    isPublic = true;
  } else {
    // For non-public grants we don't auto-create the recipient — the user
    // must already exist in `users`. Mirrors PDX-22 FK semantics.
    const existing = await db
      .select({ id: users.id })
      .from(users)
      .where(eq(users.id, sharedWith))
      .all();
    if (existing.length === 0) {
      return c.json({ error: "invalid_payload", reason: "unknown_recipient" }, 400);
    }
    recipientUserId = sharedWith;
  }

  // Insert the share row. The unique index `(resource_id, shared_with_user_id)`
  // makes re-grants idempotent — we upsert by reading after the insert
  // attempt.
  const shareId = crypto.randomUUID();
  await db
    .insert(shares)
    .values({
      id: shareId,
      resourceId: handleId,
      sharedWithUserId: recipientUserId,
      permission
    })
    .onConflictDoNothing();

  // Read back the canonical share row (handles the case where another
  // request inserted the same `(resource, recipient)` pair concurrently —
  // we want to return the existing row's id, not the one we just minted
  // and discarded).
  const existingShare = await db
    .select()
    .from(shares)
    .where(
      and(
        eq(shares.resourceId, handleId),
        eq(shares.sharedWithUserId, recipientUserId)
      )
    )
    .all();
  const row = existingShare[0];
  if (!row) {
    return c.json({ error: "internal_error", reason: "share_insert_failed" }, 500);
  }

  // Public share → mint a signed token tied to the share row's id. Store
  // the jti in AUTH_KV with the share row id as the value so the validator
  // can confirm the token still maps to a live row even if the row is
  // later deleted; the denylist takes care of immediate revocation.
  let shareToken: string | undefined;
  let shareTokenJti: string | undefined;
  let shareTokenExp: number | undefined;
  if (isPublic) {
    const signingKey =
      env.HELM_SHARE_TOKEN_SIGNING_KEY ?? env.HELM_JWT_SIGNING_KEY ?? null;
    if (!signingKey) {
      return c.json(
        {
          error: "misconfigured",
          message:
            "HELM_SHARE_TOKEN_SIGNING_KEY (or HELM_JWT_SIGNING_KEY) is required for public shares."
        },
        500
      );
    }
    const issued = await issueShareToken({ shareId: row.id, signingKey });
    shareToken = issued.token;
    shareTokenJti = issued.jti;
    shareTokenExp = issued.exp;
    if (env.AUTH_KV) {
      const ttl = Math.max(60, issued.exp - Math.floor(Date.now() / 1000));
      await env.AUTH_KV.put(`share:jti:${issued.jti}`, row.id, {
        expirationTtl: ttl
      });
    }
  }

  // Audit + broadcast.
  await recordShareGrant(env, {
    userId: ownerCheck.userId,
    shareId: row.id,
    kind,
    targetId,
    permission,
    sharedWith: isPublic ? "public" : recipientUserId
  });
  await broadcastShareGranted(env, recipientUserId, {
    shareId: row.id,
    kind,
    targetId,
    permission
  });

  return c.json(
    {
      share: row,
      ...(shareToken
        ? { shareToken, shareTokenJti, shareTokenExpiresAt: shareTokenExp }
        : {})
    },
    201
  );
}

async function handleListShares(
  c: import("hono").Context<AppEnv>,
  kind: ShareableKind
): Promise<Response> {
  const env = c.env;
  const ownerCheck = ensureOwnerCtx(c.var.authCtx);
  if (!ownerCheck.ok) {
    return c.json({ error: "unauthorized", reason: ownerCheck.reason }, 401);
  }
  const targetId = c.req.param("id");
  if (!targetId) return c.json({ error: "missing_id" }, 400);

  const db = getDb(env);
  const ownerUserId = await resolveOwnerUserId(db, kind, targetId);
  if (!ownerUserId) return c.json({ error: "not_found" }, 404);
  if (ownerUserId !== ownerCheck.userId) {
    return c.json({ error: "forbidden", reason: "not_owner" }, 403);
  }

  const handleId = shareHandleId(kind, targetId);
  const rows = await db
    .select()
    .from(shares)
    .where(eq(shares.resourceId, handleId))
    .all();
  return c.json({ shares: rows });
}

async function handleRevokeShare(
  c: import("hono").Context<AppEnv>,
  kind: ShareableKind
): Promise<Response> {
  const env = c.env;
  const ownerCheck = ensureOwnerCtx(c.var.authCtx);
  if (!ownerCheck.ok) {
    return c.json({ error: "unauthorized", reason: ownerCheck.reason }, 401);
  }
  const targetId = c.req.param("id");
  const shareIdParam = c.req.param("shareId");
  if (!targetId || !shareIdParam) {
    return c.json({ error: "missing_id" }, 400);
  }

  const db = getDb(env);
  const ownerUserId = await resolveOwnerUserId(db, kind, targetId);
  if (!ownerUserId) return c.json({ error: "not_found" }, 404);
  if (ownerUserId !== ownerCheck.userId) {
    return c.json({ error: "forbidden", reason: "not_owner" }, 403);
  }

  const handleId = shareHandleId(kind, targetId);
  const rows = await db
    .select()
    .from(shares)
    .where(and(eq(shares.id, shareIdParam), eq(shares.resourceId, handleId)))
    .all();
  const row = rows[0];
  if (!row) return c.json({ error: "not_found" }, 404);

  await db.delete(shares).where(eq(shares.id, row.id));

  // If a public-link token jti was minted for this share, deny it in KV so
  // the token is rejected immediately even though it hasn't expired. We
  // stored the row id at `share:jti:{jti}`, so we walk the recent keys via
  // a list. KV doesn't support reverse lookup; instead, future tokens for
  // this share id will fail the row lookup naturally because the row was
  // just deleted. We still record a generic share-token denylist entry
  // keyed on the share id for belt-and-suspenders.
  if (env.AUTH_KV) {
    // Best-effort: callers with the share-token jti can use this prefix
    // to verify denial. We don't iterate all jtis here; the per-jti
    // denylist key is set when callers re-present a token after revocation.
    await env.AUTH_KV.put(`share:revoked:${row.id}`, "1", {
      expirationTtl: 60 * 60 * 24 * 30
    });
    // If there is at most one jti per share row (which is the normal
    // case — public shares mint exactly one token), the caller can pass
    // it explicitly via `?jti=` so we can deny the token immediately.
    const url = new URL(c.req.url);
    const explicitJti = url.searchParams.get("jti");
    if (explicitJti) {
      // Default 30-day TTL for the denylist entry; the validator uses the
      // token's `exp` claim to bound this naturally too.
      await denyShareToken(
        env.AUTH_KV,
        explicitJti,
        Math.floor(Date.now() / 1000) + 60 * 60 * 24 * 30
      );
    }
  }

  await recordShareRevoke(env, {
    userId: ownerCheck.userId,
    shareId: row.id,
    kind,
    targetId
  });

  return c.json({ revoked: true, shareId: row.id });
}

// Allow access via public share token for run transcripts, etc. The route
// is exported here so that future surfaces (transcripts, workspace reads)
// can call it.
export { recordShareAccess as auditShareAccess };

/** Singleton lazily created on first request — Hono apps are stateless. */
let cachedApp: Hono<AppEnv> | null = null;
export function appSingleton(): Hono<AppEnv> {
  if (!cachedApp) cachedApp = createApp();
  return cachedApp;
}
