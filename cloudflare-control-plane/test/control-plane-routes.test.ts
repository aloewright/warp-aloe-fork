/**
 * Hono control-plane route tests (PDX-19).
 *
 * Covers:
 *   - Health endpoint is public and returns the expected payload shape.
 *   - Unauthenticated requests to protected routes get 401.
 *   - Helm JWT in `Authorization: Bearer …` reaches the protected handler.
 *   - Helm JWT in `?token=` (WebSocket clients) also reaches the handler.
 *   - WS upgrade on `/api/sessions/:id/ws` delegates to SessionDO with
 *     `x-helm-user-id` + `x-helm-session-id` headers populated.
 *   - Webhook HMAC validators accept good signatures and reject bad ones.
 *   - Auth `/api/auth/session` (Doppler fallback) issues a helm JWT.
 *   - Auth `/api/auth/logout` writes the jti to the KV denylist.
 *
 * The Hono app is mounted directly against synthetic `Request`s — no actual
 * Workers runtime — so we don't need miniflare. The only Cloudflare bindings
 * the tests fake are `SESSION_DO`, `AUTH_KV`, `DB`, and the workflow
 * bindings, all via lightweight in-memory stubs.
 */

import { describe, expect, it } from "vitest";

import { createApp, type ControlPlaneEnv } from "../src/workers/app.js";
import { issueHelmJwt } from "../src/shared/auth.js";
import {
  signGitHubPayload,
  signGenericPayload,
  signSlackPayload
} from "../src/shared/webhooks.js";
import type { HelmManifest } from "../src/shared/manifest.js";

const SIGNING_KEY = "test-signing-key-control-plane";

// ── Fakes ──────────────────────────────────────────────────────────────────

class MemoryKv {
  private store = new Map<string, string>();
  async get<T = string>(key: string, type?: "json" | "text"): Promise<T | null> {
    const v = this.store.get(key);
    if (!v) return null;
    if (type === "json") return JSON.parse(v) as T;
    return v as unknown as T;
  }
  async put(key: string, value: string): Promise<void> {
    this.store.set(key, value);
  }
  async delete(key: string): Promise<void> {
    this.store.delete(key);
  }
  has(key: string): boolean {
    return this.store.has(key);
  }
  asKv(): KVNamespace {
    return this as unknown as KVNamespace;
  }
}

/** Trivial D1 stub — all writes are no-ops, all reads return empty. */
const stubDb: D1Database = {
  prepare: (() => ({
    bind: () => ({
      all: async () => ({ results: [] }),
      first: async () => null,
      run: async () => ({ success: true })
    })
  })) as unknown,
  batch: async () => [],
  exec: async () => ({ count: 0, duration: 0 }),
  dump: async () => new ArrayBuffer(0)
} as unknown as D1Database;

/**
 * SessionDO fake — records every fetch so the test can assert on attribution.
 *
 * In production the DO returns a 101 Switching Protocols. Node's undici-backed
 * `Response` constructor refuses statuses below 200, so we return 200 plus a
 * sentinel header instead — the test asserts on attribution headers landing
 * on the forwarded request, which is the actual contract under test.
 */
class FakeSessionDoStub {
  lastRequest: Request | null = null;
  async fetch(req: Request): Promise<Response> {
    this.lastRequest = req;
    return new Response("ok-from-do", {
      status: 200,
      headers: { "x-session-do-status": "would-be-101" }
    });
  }
}

class FakeSessionDoNamespace {
  stubs = new Map<string, FakeSessionDoStub>();
  idFromName(name: string): DurableObjectId {
    return { toString: () => `id:${name}`, name } as unknown as DurableObjectId;
  }
  get(id: DurableObjectId): FakeSessionDoStub {
    const key = (id as unknown as { name: string }).name ?? id.toString();
    let stub = this.stubs.get(key);
    if (!stub) {
      stub = new FakeSessionDoStub();
      this.stubs.set(key, stub);
    }
    return stub;
  }
}

function buildManifest(overrides?: Partial<HelmManifest["access"]>): HelmManifest {
  return {
    accountId: "acct-test",
    zone: { id: "zone-test", domain: "test.example" },
    environments: ["dev", "staging", "production"],
    workers: {},
    resources: { d1: {}, r2: {}, durableObjects: {}, kv: {}, aiGateways: {} },
    containers: {
      dev: { enabled: true, instanceClass: "dev" },
      staging: { enabled: true, instanceClass: "dev" },
      production: { enabled: true, instanceClass: "dev" }
    },
    access: {
      required: false,
      teamDomain: "test.cloudflareaccess.com",
      audiences: { dev: "aud-dev", staging: "aud-staging", production: "aud-production" },
      ...overrides
    },
    protected: []
  };
}

interface BuildEnvOpts {
  authKv?: MemoryKv;
  sessionDo?: FakeSessionDoNamespace;
  githubSecret?: string;
  slackSecret?: string;
  genericSecret?: string;
  manifest?: HelmManifest;
}

function buildEnv(opts: BuildEnvOpts = {}): ControlPlaneEnv {
  const manifest = opts.manifest ?? buildManifest();
  return {
    HELM_ENVIRONMENT: "dev",
    HELM_VERSION: "0.0.1-test",
    HELM_BUILD_ID: "test",
    HELM_MANIFEST_JSON: JSON.stringify(manifest),
    HELM_JWT_SIGNING_KEY: SIGNING_KEY,
    AUTH_KV: opts.authKv?.asKv(),
    SESSION_DO: opts.sessionDo as unknown as ControlPlaneEnv["SESSION_DO"],
    GITHUB_WEBHOOK_SECRET: opts.githubSecret,
    SLACK_WEBHOOK_SECRET: opts.slackSecret,
    GENERIC_WEBHOOK_SECRET: opts.genericSecret,
    DB: stubDb,
    CONTROL_PLANE_REGISTRY: {} as DurableObjectNamespace
  };
}

const noopCtx = {
  waitUntil: (_: Promise<unknown>) => {},
  passThroughOnException: () => {}
} as unknown as ExecutionContext;

// ── Tests ──────────────────────────────────────────────────────────────────

describe("control-plane Hono app", () => {
  describe("health", () => {
    it("returns the service descriptor unauthenticated", async () => {
      const app = createApp();
      const res = await app.fetch(new Request("https://h/api/health"), buildEnv(), noopCtx);
      expect(res.status).toBe(200);
      const body = (await res.json()) as { service: string; environment: string };
      expect(body.service).toBe("helm-control-plane");
      expect(body.environment).toBe("dev");
    });
  });

  describe("auth middleware", () => {
    it("rejects unauthenticated requests to protected routes", async () => {
      const app = createApp();
      const res = await app.fetch(
        new Request("https://h/api/environments"),
        buildEnv(),
        noopCtx
      );
      expect(res.status).toBe(401);
    });

    it("rejects unauthenticated requests to /api/audit/sync", async () => {
      const app = createApp();
      const res = await app.fetch(
        new Request("https://h/api/audit/sync", { method: "POST", body: "[]" }),
        buildEnv(),
        noopCtx
      );
      expect(res.status).toBe(401);
    });

    it("accepts a helm JWT for /api/audit/sync (but fails with 503 if DB unbound)", async () => {
      const app = createApp();
      const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
      const res = await app.fetch(
        new Request("https://h/api/audit/sync", {
          method: "POST",
          body: "[]",
          headers: {
            Authorization: `Bearer ${issued.token}`,
            "Content-Type": "application/json"
          }
        }),
        buildEnv(),
        noopCtx
      );
      // Reached the handler, but buildEnv doesn't set HELM_AUDIT_DB yet.
      expect(res.status).toBe(503);
    });

    it("accepts a helm JWT via Authorization header", async () => {
      const app = createApp();
      const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
      const res = await app.fetch(
        new Request("https://h/api/environments", {
          headers: { Authorization: `Bearer ${issued.token}` }
        }),
        buildEnv(),
        noopCtx
      );
      expect(res.status).toBe(200);
      const body = (await res.json()) as { environments: Array<{ name: string }> };
      expect(body.environments.map((e) => e.name)).toContain("dev");
    });

    it("accepts a helm JWT via ?token= for WebSocket clients", async () => {
      const app = createApp();
      const issued = await issueHelmJwt({ userId: "user-2", signingKey: SIGNING_KEY });
      const res = await app.fetch(
        new Request(`https://h/api/environments?token=${encodeURIComponent(issued.token)}`),
        buildEnv(),
        noopCtx
      );
      expect(res.status).toBe(200);
    });
  });

  describe("WebSocket upgrade → SessionDO", () => {
    it("forwards the upgrade request with attribution headers", async () => {
      const app = createApp();
      const session = new FakeSessionDoNamespace();
      const env = buildEnv({ sessionDo: session });
      const issued = await issueHelmJwt({ userId: "user-42", signingKey: SIGNING_KEY });

      const res = await app.fetch(
        new Request("https://h/api/sessions/sess-abc/ws", {
          headers: {
            Upgrade: "websocket",
            "Sec-WebSocket-Key": "dGhlIHNhbXBsZSBub25jZQ==",
            "Sec-WebSocket-Version": "13",
            Authorization: `Bearer ${issued.token}`
          }
        }),
        env,
        noopCtx
      );

      // 200 instead of 101 because Node's undici can't materialize 101.
      // The DO sets a sentinel header so we can confirm the response came
      // from the stub (and not from a Hono error path).
      expect(res.status).toBe(200);
      expect(res.headers.get("x-session-do-status")).toBe("would-be-101");
      // The fake DO recorded the forwarded request — assert attribution headers.
      const stub = session.get(session.idFromName("sess-abc"));
      expect(stub.lastRequest).not.toBeNull();
      expect(stub.lastRequest?.headers.get("x-helm-user-id")).toBe("user-42");
      expect(stub.lastRequest?.headers.get("x-helm-session-id")).toBe("sess-abc");
      expect(stub.lastRequest?.headers.get("Upgrade")).toBe("websocket");
    });

    it("rejects when the request is not a WS upgrade", async () => {
      const app = createApp();
      const session = new FakeSessionDoNamespace();
      const env = buildEnv({ sessionDo: session });
      const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
      const res = await app.fetch(
        new Request("https://h/api/sessions/sess-abc/ws", {
          headers: { Authorization: `Bearer ${issued.token}` }
        }),
        env,
        noopCtx
      );
      expect(res.status).toBe(426);
    });

    it("returns 503 when SESSION_DO is not bound", async () => {
      const app = createApp();
      const env = buildEnv();
      const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
      const res = await app.fetch(
        new Request("https://h/api/sessions/x/ws", {
          headers: {
            Upgrade: "websocket",
            Authorization: `Bearer ${issued.token}`
          }
        }),
        env,
        noopCtx
      );
      expect(res.status).toBe(503);
    });
  });

  describe("webhooks", () => {
    it("github: accepts a valid signature", async () => {
      const app = createApp();
      const secret = "ghsec";
      const body = JSON.stringify({ action: "opened" });
      const sig = await signGitHubPayload(secret, body);

      const res = await app.fetch(
        new Request("https://h/api/webhooks/github", {
          method: "POST",
          body,
          headers: {
            "Content-Type": "application/json",
            "X-GitHub-Event": "pull_request",
            "X-GitHub-Delivery": "deadbeef",
            "X-Hub-Signature-256": sig
          }
        }),
        buildEnv({ githubSecret: secret }),
        noopCtx
      );
      expect(res.status).toBe(202);
      const json = (await res.json()) as { accepted: boolean; event: string };
      expect(json.accepted).toBe(true);
      expect(json.event).toBe("pull_request");
    });

    it("github: rejects a bad signature", async () => {
      const app = createApp();
      const res = await app.fetch(
        new Request("https://h/api/webhooks/github", {
          method: "POST",
          body: "{}",
          headers: { "X-Hub-Signature-256": "sha256=00".padEnd(71, "0") }
        }),
        buildEnv({ githubSecret: "ghsec" }),
        noopCtx
      );
      expect(res.status).toBe(401);
    });

    it("github: 503 when secret not configured", async () => {
      const app = createApp();
      const res = await app.fetch(
        new Request("https://h/api/webhooks/github", { method: "POST", body: "{}" }),
        buildEnv(),
        noopCtx
      );
      expect(res.status).toBe(503);
    });

    it("slack: accepts a valid signature", async () => {
      const app = createApp();
      const secret = "slksec";
      const ts = Math.floor(Date.now() / 1000);
      const body = "token=foo&team_id=T1";
      const sig = await signSlackPayload(secret, ts, body);
      const res = await app.fetch(
        new Request("https://h/api/webhooks/slack", {
          method: "POST",
          body,
          headers: {
            "Content-Type": "application/x-www-form-urlencoded",
            "X-Slack-Signature": sig,
            "X-Slack-Request-Timestamp": String(ts)
          }
        }),
        buildEnv({ slackSecret: secret }),
        noopCtx
      );
      expect(res.status).toBe(202);
    });

    it("slack: rejects a stale timestamp", async () => {
      const app = createApp();
      const secret = "slksec";
      const ts = Math.floor(Date.now() / 1000) - 60 * 60;
      const sig = await signSlackPayload(secret, ts, "");
      const res = await app.fetch(
        new Request("https://h/api/webhooks/slack", {
          method: "POST",
          body: "",
          headers: {
            "X-Slack-Signature": sig,
            "X-Slack-Request-Timestamp": String(ts)
          }
        }),
        buildEnv({ slackSecret: secret }),
        noopCtx
      );
      expect(res.status).toBe(401);
    });

    it("generic: accepts a valid signature", async () => {
      const app = createApp();
      const secret = "gen";
      const body = "hello";
      const sig = await signGenericPayload(secret, body);
      const res = await app.fetch(
        new Request("https://h/api/webhooks/generic", {
          method: "POST",
          body,
          headers: {
            "X-Webhook-Source": "test",
            "X-Webhook-Signature": sig
          }
        }),
        buildEnv({ genericSecret: secret }),
        noopCtx
      );
      expect(res.status).toBe(202);
    });

    it("generic: rejects when signature is absent", async () => {
      const app = createApp();
      const res = await app.fetch(
        new Request("https://h/api/webhooks/generic", { method: "POST", body: "hi" }),
        buildEnv({ genericSecret: "g" }),
        noopCtx
      );
      expect(res.status).toBe(401);
    });
  });

  describe("auth/session — Doppler fallback", () => {
    it("issues a helm JWT scoped to the doppler project", async () => {
      // Hono uses global fetch when handlers re-call out — we patch global
      // fetch for the duration of this test so the Doppler validator returns
      // a deterministic project slug.
      const realFetch = globalThis.fetch;
      globalThis.fetch = (async (url: string) => {
        if (typeof url === "string" && url.includes("/v3/me")) {
          return new Response(JSON.stringify({ slug: "helm-test" }), { status: 200 });
        }
        return new Response("not found", { status: 404 });
      }) as typeof fetch;
      try {
        const app = createApp();
        const res = await app.fetch(
          new Request("https://h/api/auth/session", {
            method: "POST",
            headers: { "X-Doppler-Token": "dp.st.fake" }
          }),
          buildEnv(),
          noopCtx
        );
        expect(res.status).toBe(200);
        const body = (await res.json()) as { token: string; scope: string };
        expect(body.token.split(".")).toHaveLength(3);
        expect(body.scope).toBe("doppler:helm-test");
      } finally {
        globalThis.fetch = realFetch;
      }
    });

    it("rejects when Access is required but no Access JWT is present", async () => {
      const app = createApp();
      const res = await app.fetch(
        new Request("https://h/api/auth/session", { method: "POST" }),
        buildEnv({ manifest: buildManifest({ required: true }) }),
        noopCtx
      );
      expect(res.status).toBe(401);
    });
  });

  describe("auth/logout", () => {
    it("denylists the jti in AUTH_KV", async () => {
      const app = createApp();
      const kv = new MemoryKv();
      const env = buildEnv({ authKv: kv });
      const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
      const res = await app.fetch(
        new Request("https://h/api/auth/logout", {
          method: "POST",
          headers: { Authorization: `Bearer ${issued.token}` }
        }),
        env,
        noopCtx
      );
      expect(res.status).toBe(200);
      const body = (await res.json()) as { revoked: boolean; jti: string };
      expect(body.revoked).toBe(true);
      expect(body.jti).toBe(issued.payload.jti);
      expect(kv.has(`denylist:jti:${issued.payload.jti}`)).toBe(true);
    });
  });

  describe("404 fallback", () => {
    it("returns 404 for unknown paths", async () => {
      const app = createApp();
      const res = await app.fetch(new Request("https://h/api/nope"), buildEnv(), noopCtx);
      expect(res.status).toBe(404);
    });
  });
});
