import { describe, expect, it } from "vitest";
import { createApp } from "../src/workers/app.js";
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
  batch: async () => [],
  exec: async () => ({ count: 0, duration: 0 }),
  dump: async () => new ArrayBuffer(0)
} as unknown as D1Database;

function buildEnv(overrides: any = {}): any {
  return {
    HELM_ENVIRONMENT: "dev",
    HELM_VERSION: "0.0.1-test",
    HELM_BUILD_ID: "test",
    HELM_MANIFEST_JSON: JSON.stringify({
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
        access: { required: false, teamDomain: "test", audiences: { dev: "aud" } },
        protected: []
    }),
    HELM_JWT_SIGNING_KEY: SIGNING_KEY,
    DB: stubDb,
    HELM_AUDIT_DB: stubDb,
    ...overrides
  };
}

const noopCtx = {
  waitUntil: (_: Promise<unknown>) => {},
  passThroughOnException: () => {}
} as unknown as ExecutionContext;

describe("Security: /api/audit/sync protection", () => {
  it("FIXED: /api/audit/sync is now protected (expects 401)", async () => {
    const app = createApp();
    const res = await app.fetch(
      new Request("https://h/api/audit/sync", {
        method: "POST",
        body: JSON.stringify([{ timestamp: new Date().toISOString(), action: "test" }]),
        headers: { "Content-Type": "application/json" }
      }),
      buildEnv(),
      noopCtx
    );

    expect(res.status).toBe(401);
  });

  it("FIXED: /api/audit/sync has a batch size limit (expects 400)", async () => {
    const app = createApp();
    const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
    const largeBatch = Array.from({ length: 1000 }, () => ({
      timestamp: new Date().toISOString(),
      action: "spam"
    }));

    const res = await app.fetch(
      new Request("https://h/api/audit/sync", {
        method: "POST",
        body: JSON.stringify(largeBatch),
        headers: {
            "Content-Type": "application/json",
            "Authorization": `Bearer ${issued.token}`
        }
      }),
      buildEnv(),
      noopCtx
    );

    expect(res.status).toBe(400);
    const body = await res.json() as { error: string };
    expect(body.error).toBe("batch_too_large");
  });

  it("FIXED: /api/audit/sync works when authenticated and batch is within limit", async () => {
    const app = createApp();
    const issued = await issueHelmJwt({ userId: "user-1", signingKey: SIGNING_KEY });
    const res = await app.fetch(
      new Request("https://h/api/audit/sync", {
        method: "POST",
        body: JSON.stringify([{ timestamp: new Date().toISOString(), action: "test" }]),
        headers: {
            "Content-Type": "application/json",
            "Authorization": `Bearer ${issued.token}`
        }
      }),
      buildEnv(),
      noopCtx
    );

    expect(res.status).toBe(200);
  });
});
