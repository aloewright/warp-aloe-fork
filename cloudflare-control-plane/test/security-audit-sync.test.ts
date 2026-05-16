import { describe, expect, it } from "vitest";
import { createApp, type ControlPlaneEnv } from "../src/workers/app.js";
import { issueHelmJwt } from "../src/shared/auth.js";

const SIGNING_KEY = "test-signing-key-security";

const stubDb: D1Database = {
  prepare: (() => ({
    bind: () => ({
      all: async () => ({ results: [] }),
      first: async () => null,
      run: async () => ({ success: true })
    })
  })) as unknown,
  batch: async (stmts: any[]) => stmts.map(() => ({ meta: { changes: 1 } })),
  exec: async () => ({ count: 0, duration: 0 }),
  dump: async () => new ArrayBuffer(0)
} as unknown as D1Database;

function buildEnv(): ControlPlaneEnv {
  return {
    HELM_ENVIRONMENT: "dev",
    HELM_VERSION: "0.0.1-test",
    HELM_BUILD_ID: "test",
    HELM_MANIFEST_JSON: JSON.stringify({
      accountId: "acct-test",
      zone: { id: "zone-test", domain: "test.example" },
      environments: ["dev"],
      workers: {},
      resources: { d1: {}, r2: {}, durableObjects: {}, kv: {}, aiGateways: {} },
      containers: { dev: { enabled: true, instanceClass: "dev" }, staging: { enabled: false, instanceClass: "dev" }, production: { enabled: false, instanceClass: "dev" } },
      access: { required: false, teamDomain: "test.cloudflareaccess.com", audiences: { dev: "aud-dev" } },
      protected: []
    }),
    HELM_JWT_SIGNING_KEY: SIGNING_KEY,
    DB: stubDb,
    HELM_AUDIT_DB: stubDb,
    CONTROL_PLANE_REGISTRY: {} as DurableObjectNamespace
  };
}

const noopCtx = {
  waitUntil: (_: Promise<unknown>) => {},
  passThroughOnException: () => {}
} as unknown as ExecutionContext;

describe("Security: /api/audit/sync", () => {
  it("FIXED: rejects unauthenticated access", async () => {
    const app = createApp();
    const res = await app.fetch(
      new Request("https://h/api/audit/sync", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify([{ timestamp: new Date().toISOString(), action: "test" }])
      }),
      buildEnv(),
      noopCtx
    );
    expect(res.status).toBe(401);
  });

  it("FIXED: rejects large batches (DoS protection)", async () => {
    const app = createApp();
    const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
    const largeBatch = Array.from({ length: 600 }, () => ({
      timestamp: new Date().toISOString(),
      action: "spam"
    }));

    const res = await app.fetch(
      new Request("https://h/api/audit/sync", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Authorization": `Bearer ${issued.token}`
        },
        body: JSON.stringify(largeBatch)
      }),
      buildEnv(),
      noopCtx
    );
    expect(res.status).toBe(413);
    const body = await res.json() as { error: string };
    expect(body.error).toBe("batch_too_large");
  });

  it("FIXED: accepts authenticated batches within limits", async () => {
    const app = createApp();
    const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
    const batch = Array.from({ length: 10 }, () => ({
      timestamp: new Date().toISOString(),
      action: "legit"
    }));

    const res = await app.fetch(
      new Request("https://h/api/audit/sync", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          "Authorization": `Bearer ${issued.token}`
        },
        body: JSON.stringify(batch)
      }),
      buildEnv(),
      noopCtx
    );
    expect(res.status).toBe(200);
    const body = await res.json() as { inserted: number };
    expect(body.inserted).toBe(10);
  });
});
