/**
 * Sharing model tests (PDX-116 [E4]).
 *
 * Covers the REST surface added in `src/workers/app.ts`:
 *   - POST   /api/workspaces/:id/shares       (grant)
 *   - GET    /api/workspaces/:id/shares       (list)
 *   - DELETE /api/workspaces/:id/shares/:sid  (revoke)
 *   - POST   /api/sessions/:id/shares
 *   - POST   /api/runs/:id/shares
 *
 * Also covers the share-token signing helpers added to `src/shared/auth.ts`
 * and the permission-resolution module `src/shared/share_auth.ts`.
 *
 * Test database: in-memory `better-sqlite3` with the PDX-22 init migration
 * applied — same pattern used by `auth.test.ts` and `db-schema.test.ts`.
 */

import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import Database from "better-sqlite3";
import { drizzle as drizzleSqlite } from "drizzle-orm/better-sqlite3";
import { describe, expect, it } from "vitest";

import { createApp, type ControlPlaneEnv } from "../src/workers/app.js";
import {
  issueHelmJwt,
  issueShareToken,
  validateShareToken
} from "../src/shared/auth.js";
import {
  PUBLIC_USER_ID,
  shareHandleId
} from "../src/shared/share_auth.js";
import {
  auditLog,
  shares,
  sessions,
  tasks,
  users,
  workspaces
} from "../src/db/schema.js";
import type { HelmManifest } from "../src/shared/manifest.js";
import { eq } from "drizzle-orm";

const MIGRATION_PATH = resolve(__dirname, "../migrations/0000_init.sql");
const SIGNING_KEY = "test-share-signing-key";

// ── Test harness: an in-memory DB faked as a D1Database ────────────────────

class MemoryKv {
  store = new Map<string, { value: string; expiresAt: number | null }>();
  putCalls = 0;

  async get<T = string>(key: string, type?: "json" | "text"): Promise<T | null> {
    const entry = this.store.get(key);
    if (!entry) return null;
    if (type === "json") return JSON.parse(entry.value) as T;
    return entry.value as unknown as T;
  }

  async put(
    key: string,
    value: string,
    options?: { expirationTtl?: number }
  ): Promise<void> {
    this.putCalls += 1;
    const expiresAt = options?.expirationTtl
      ? Date.now() + options.expirationTtl * 1000
      : null;
    this.store.set(key, { value, expiresAt });
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

class FakeUserDoStub {
  calls: Array<{ url: string; body: string }> = [];
  async fetch(req: Request): Promise<Response> {
    const body = await req.text();
    this.calls.push({ url: req.url, body });
    return new Response(JSON.stringify({ sequence: 1 }), {
      status: 200,
      headers: { "content-type": "application/json" }
    });
  }
}

class FakeUserDoNamespace {
  stubs = new Map<string, FakeUserDoStub>();
  idFromName(name: string): DurableObjectId {
    return { toString: () => `id:${name}`, name } as unknown as DurableObjectId;
  }
  get(id: DurableObjectId): FakeUserDoStub {
    const key = (id as unknown as { name: string }).name ?? id.toString();
    let stub = this.stubs.get(key);
    if (!stub) {
      stub = new FakeUserDoStub();
      this.stubs.set(key, stub);
    }
    return stub;
  }
}

function setupDb() {
  const sqlite = new Database(":memory:");
  const sql = readFileSync(MIGRATION_PATH, "utf8");
  for (const stmt of sql
    .split(/-->\s*statement-breakpoint/g)
    .map((s) => s.trim())
    .filter(Boolean)) {
    sqlite.exec(stmt);
  }
  sqlite.pragma("foreign_keys = ON");
  return sqlite;
}

/**
 * Adapt the better-sqlite3 backed Drizzle client to the D1Database surface
 * the Worker code expects. The Drizzle `getDb` factory ultimately calls
 * `drizzle(env.DB)` which uses `prepare`/`bind`/`all`/`first`/`run`.
 *
 * We re-bind the schema to the better-sqlite3 driver and stash it on the
 * Env so the tests can drive Hono routes directly.
 */
function buildEnv(opts: {
  sqlite: Database.Database;
  authKv?: MemoryKv;
  userDo?: FakeUserDoNamespace;
  signingKey?: string;
  shareSigningKey?: string;
}): ControlPlaneEnv & { __sqlite: Database.Database } {
  const manifest: HelmManifest = {
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
      audiences: { dev: "aud-dev", staging: "aud-staging", production: "aud-production" }
    },
    protected: []
  };

  // Wrap the better-sqlite3 driver in a thin facade that satisfies the
  // D1Database surface used by drizzle-orm/d1. drizzle-orm/d1 calls
  // `prepare(sql).bind(...args).all()/first()/run()`, returning shapes that
  // match D1's response. better-sqlite3 statements expose `all`, `get`,
  // `run` with similar semantics — we adapt them.
  const fakeD1 = makeBetterSqliteAsD1(opts.sqlite);

  return {
    HELM_ENVIRONMENT: "dev",
    HELM_VERSION: "0.0.1-test",
    HELM_BUILD_ID: "test",
    HELM_MANIFEST_JSON: JSON.stringify(manifest),
    HELM_JWT_SIGNING_KEY: opts.signingKey ?? SIGNING_KEY,
    HELM_SHARE_TOKEN_SIGNING_KEY: opts.shareSigningKey,
    AUTH_KV: opts.authKv?.asKv(),
    USER_DO: opts.userDo as unknown as ControlPlaneEnv["USER_DO"],
    DB: fakeD1,
    CONTROL_PLANE_REGISTRY: {} as DurableObjectNamespace,
    __sqlite: opts.sqlite
  };
}

/**
 * Build a `D1Database`-shaped facade over `better-sqlite3`. drizzle-orm/d1's
 * driver calls `prepare(sql).bind(...args).{all,first,run}()` and expects
 * `{ results, success, meta }` envelopes. better-sqlite3 supports the same
 * shapes synchronously; we wrap each in a Promise. Sufficient for the
 * subset of queries Drizzle issues (selects with where clauses, inserts,
 * deletes, joins).
 */
function makeBetterSqliteAsD1(sqlite: Database.Database): D1Database {
  const prepare = (sqlText: string) => {
    const stmt = sqlite.prepare(sqlText);
    let bound: unknown[] = [];
    const api = {
      bind(...args: unknown[]) {
        bound = args;
        return api;
      },
      async all<T = unknown>(): Promise<{ results: T[]; success: true; meta: object }> {
        const results = stmt.all(...bound) as T[];
        return { results, success: true, meta: {} };
      },
      async first<T = unknown>(): Promise<T | null> {
        const row = stmt.get(...bound) as T | undefined;
        return (row ?? null) as T | null;
      },
      async run(): Promise<{ success: true; meta: object }> {
        stmt.run(...bound);
        return { success: true, meta: {} };
      },
      async raw<T = unknown[]>(): Promise<T[]> {
        const rows = stmt.raw().all(...bound) as T[];
        return rows;
      }
    };
    return api;
  };
  return {
    prepare: prepare as unknown,
    batch: async (statements: unknown[]) => {
      const out: unknown[] = [];
      for (const s of statements as Array<{ all: () => Promise<unknown> }>) {
        out.push(await s.all());
      }
      return out as never;
    },
    exec: async (sqlText: string) => {
      sqlite.exec(sqlText);
      return { count: 0, duration: 0 } as never;
    },
    dump: async () => new ArrayBuffer(0)
  } as unknown as D1Database;
}

const noopCtx = {
  waitUntil: (_: Promise<unknown>) => {},
  passThroughOnException: () => {}
} as unknown as ExecutionContext;

// ── Helpers to seed test data via Drizzle directly ─────────────────────────

function seedUser(sqlite: Database.Database, id: string, email: string): void {
  sqlite
    .prepare("INSERT INTO users (id, email) VALUES (?, ?)")
    .run(id, email);
}

function seedWorkspace(
  sqlite: Database.Database,
  id: string,
  ownerUserId: string,
  name = "ws"
): void {
  sqlite
    .prepare("INSERT INTO workspaces (id, owner_user_id, name) VALUES (?, ?, ?)")
    .run(id, ownerUserId, name);
}

function seedSession(
  sqlite: Database.Database,
  id: string,
  userId: string,
  agentId = "agent-x"
): void {
  sqlite
    .prepare("INSERT INTO sessions (id, user_id, agent_id) VALUES (?, ?, ?)")
    .run(id, userId, agentId);
}

function seedTask(
  sqlite: Database.Database,
  id: string,
  sessionId: string,
  prompt = "p"
): void {
  sqlite
    .prepare("INSERT INTO tasks (id, session_id, prompt) VALUES (?, ?, ?)")
    .run(id, sessionId, prompt);
}

function countAuditByAction(
  sqlite: Database.Database,
  action: string
): number {
  const row = sqlite
    .prepare("SELECT COUNT(*) AS n FROM audit_log WHERE action = ?")
    .get(action) as { n: number };
  return row.n;
}

// ── Tests ──────────────────────────────────────────────────────────────────

describe("share-token signing helpers (PDX-116)", () => {
  it("issues a token and round-trips it", async () => {
    const issued = await issueShareToken({
      shareId: "share-1",
      signingKey: SIGNING_KEY
    });
    expect(issued.token.split(".")).toHaveLength(3);
    const verified = await validateShareToken(issued.token, {
      signingKey: SIGNING_KEY
    });
    expect(verified.ok).toBe(true);
    if (verified.ok) {
      expect(verified.shareId).toBe("share-1");
      expect(verified.jti).toBe(issued.jti);
    }
  });

  it("rejects a token signed with a different key", async () => {
    const issued = await issueShareToken({
      shareId: "share-1",
      signingKey: SIGNING_KEY
    });
    const verified = await validateShareToken(issued.token, {
      signingKey: "wrong-key"
    });
    expect(verified.ok).toBe(false);
    if (!verified.ok) expect(verified.reason).toBe("bad_signature");
  });

  it("rejects an expired token", async () => {
    const baseNow = 1_700_000_000;
    const issued = await issueShareToken({
      shareId: "share-1",
      signingKey: SIGNING_KEY,
      now: baseNow,
      ttlSeconds: 60
    });
    const verified = await validateShareToken(issued.token, {
      signingKey: SIGNING_KEY,
      now: baseNow + 120
    });
    expect(verified.ok).toBe(false);
    if (!verified.ok) expect(verified.reason).toBe("expired");
  });

  it("rejects a token presented as a helm JWT (typ guard)", async () => {
    // Sanity: the share token has `typ = share-jwt`. The helm JWT verifier
    // should refuse it. We re-use validateShareToken's negative path here
    // for type-safety; a separate test would verify the helm JWT path.
    const issued = await issueShareToken({
      shareId: "share-1",
      signingKey: SIGNING_KEY
    });
    // Tamper with the header `typ`.
    const [head, body, sig] = issued.token.split(".");
    const decodedHead = JSON.parse(
      Buffer.from(head!, "base64url").toString("utf8")
    );
    expect(decodedHead.typ).toBe("share-jwt");
    // Construct a header that claims helm-jwt typ (validateShareToken should
    // reject because the inputs no longer cover the new header).
    const badHeader = Buffer.from(
      JSON.stringify({ alg: "HS256", typ: "JWT" })
    ).toString("base64url");
    const badToken = `${badHeader}.${body}.${sig}`;
    const verified = await validateShareToken(badToken, {
      signingKey: SIGNING_KEY
    });
    expect(verified.ok).toBe(false);
  });
});

describe("share routes — happy paths", () => {
  it("grants a workspace share to another user and lists it", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "owner@x.com");
    seedUser(sqlite, "friend-1", "friend@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");

    const userDo = new FakeUserDoNamespace();
    const env = buildEnv({ sqlite, userDo });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });

    const res = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "friend-1", permission: "read" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(201);
    const body = (await res.json()) as { share: { id: string; permission: string } };
    expect(body.share.permission).toBe("read");

    // List shares.
    const listRes = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        headers: { Authorization: `Bearer ${issued.token}` }
      }),
      env,
      noopCtx
    );
    expect(listRes.status).toBe(200);
    const listBody = (await listRes.json()) as { shares: Array<{ id: string }> };
    expect(listBody.shares).toHaveLength(1);
    expect(listBody.shares[0]!.id).toBe(body.share.id);

    // Audit log: a `share.granted` row was written.
    expect(countAuditByAction(sqlite, "share.granted")).toBe(1);

    // UserDO broadcast was called for the recipient.
    const stub = userDo.stubs.get("friend-1");
    expect(stub).toBeDefined();
    expect(stub!.calls).toHaveLength(1);
    const broadcastBody = JSON.parse(stub!.calls[0]!.body) as {
      userId: string;
      event: { kind: string; payload: { type: string; permission: string } };
    };
    expect(broadcastBody.userId).toBe("friend-1");
    expect(broadcastBody.event.payload.type).toBe("share_granted");
    expect(broadcastBody.event.payload.permission).toBe("read");
  });

  it("grants a public workspace share and returns a share token", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-2", "owner2@x.com");
    seedWorkspace(sqlite, "ws-2", "owner-2");

    const kv = new MemoryKv();
    const env = buildEnv({ sqlite, authKv: kv });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-2",
      signingKey: SIGNING_KEY
    });

    const res = await app.fetch(
      new Request("https://h/api/workspaces/ws-2/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "public", permission: "read" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(201);
    const body = (await res.json()) as {
      share: { id: string; sharedWithUserId: string };
      shareToken: string;
      shareTokenJti: string;
    };
    expect(body.shareToken.split(".")).toHaveLength(3);
    expect(body.share.sharedWithUserId).toBe(PUBLIC_USER_ID);

    // KV stored the jti -> share row id mapping.
    expect(kv.has(`share:jti:${body.shareTokenJti}`)).toBe(true);

    // Token verifies against the helm signing key (no separate key set).
    const verified = await validateShareToken(body.shareToken, {
      signingKey: SIGNING_KEY,
      authKv: kv.asKv()
    });
    expect(verified.ok).toBe(true);
  });

  it("grants a session share", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-3", "owner3@x.com");
    seedUser(sqlite, "friend-3", "friend3@x.com");
    seedSession(sqlite, "sess-1", "owner-3");

    const userDo = new FakeUserDoNamespace();
    const env = buildEnv({ sqlite, userDo });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-3",
      signingKey: SIGNING_KEY
    });

    const res = await app.fetch(
      new Request("https://h/api/sessions/sess-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "friend-3", permission: "write" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(201);
    const body = (await res.json()) as { share: { resourceId: string } };
    expect(body.share.resourceId).toBe(shareHandleId("session", "sess-1"));
  });

  it("grants a single agent-run transcript share", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-4", "owner4@x.com");
    seedUser(sqlite, "friend-4", "friend4@x.com");
    seedSession(sqlite, "sess-4", "owner-4");
    seedTask(sqlite, "run-4", "sess-4");

    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-4",
      signingKey: SIGNING_KEY
    });

    const res = await app.fetch(
      new Request("https://h/api/runs/run-4/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "friend-4", permission: "read" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(201);
    const body = (await res.json()) as { share: { resourceId: string } };
    expect(body.share.resourceId).toBe(shareHandleId("run", "run-4"));
  });
});

describe("share routes — auth + ownership", () => {
  it("returns 401 when unauthenticated", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const env = buildEnv({ sqlite });
    const app = createApp();

    const res = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        body: JSON.stringify({ shared_with: "x", permission: "read" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(401);
  });

  it("returns 403 when the caller is not the workspace owner", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedUser(sqlite, "stranger", "s@x.com");
    seedUser(sqlite, "friend", "f@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "stranger",
      signingKey: SIGNING_KEY
    });

    const res = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "friend", permission: "read" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(403);
  });

  it("returns 404 when the workspace does not exist", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });
    const res = await app.fetch(
      new Request("https://h/api/workspaces/missing/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "x", permission: "read" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(404);
  });

  it("returns 400 for an unknown recipient user", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });
    const res = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "ghost", permission: "read" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(400);
  });

  it("returns 400 for an invalid permission value", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedUser(sqlite, "friend", "f@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });
    const res = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "friend", permission: "owner" })
      }),
      env,
      noopCtx
    );
    expect(res.status).toBe(400);
  });
});

describe("share routes — revoke", () => {
  it("revokes a share, writes an audit row, and removes the row", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedUser(sqlite, "friend", "f@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });

    // Grant
    const grantRes = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "friend", permission: "read" })
      }),
      env,
      noopCtx
    );
    const grantBody = (await grantRes.json()) as { share: { id: string } };
    const shareId = grantBody.share.id;

    // Revoke
    const revokeRes = await app.fetch(
      new Request(`https://h/api/workspaces/ws-1/shares/${shareId}`, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${issued.token}` }
      }),
      env,
      noopCtx
    );
    expect(revokeRes.status).toBe(200);
    const revokeBody = (await revokeRes.json()) as { revoked: boolean };
    expect(revokeBody.revoked).toBe(true);

    // Audit row recorded.
    expect(countAuditByAction(sqlite, "share.revoked")).toBe(1);

    // Listing returns no shares.
    const listRes = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        headers: { Authorization: `Bearer ${issued.token}` }
      }),
      env,
      noopCtx
    );
    const listBody = (await listRes.json()) as { shares: unknown[] };
    expect(listBody.shares).toHaveLength(0);
  });

  it("subsequent revoked shares cannot be re-listed (404 on revoke twice)", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedUser(sqlite, "friend", "f@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });
    const grantRes = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "friend", permission: "read" })
      }),
      env,
      noopCtx
    );
    const { share } = (await grantRes.json()) as { share: { id: string } };

    // First revoke succeeds.
    const r1 = await app.fetch(
      new Request(`https://h/api/workspaces/ws-1/shares/${share.id}`, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${issued.token}` }
      }),
      env,
      noopCtx
    );
    expect(r1.status).toBe(200);

    // Second revoke 404s.
    const r2 = await app.fetch(
      new Request(`https://h/api/workspaces/ws-1/shares/${share.id}`, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${issued.token}` }
      }),
      env,
      noopCtx
    );
    expect(r2.status).toBe(404);
  });
});

describe("share-token public access semantics", () => {
  it("validateShareToken accepts the issued token", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");

    const kv = new MemoryKv();
    const env = buildEnv({ sqlite, authKv: kv });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });

    const res = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "public", permission: "read" })
      }),
      env,
      noopCtx
    );
    const body = (await res.json()) as { shareToken: string };

    // The token validates against the configured share signing key.
    const verified = await validateShareToken(body.shareToken, {
      signingKey: SIGNING_KEY
    });
    expect(verified.ok).toBe(true);
  });

  it("`?share_token=…` is rejected after explicit revocation via denylist", async () => {
    // We don't yet have a public-link read endpoint to exercise here, so we
    // verify the denylist primitive directly: after `denyShareToken`, the
    // validator returns `denylisted`.
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const kv = new MemoryKv();
    const env = buildEnv({ sqlite, authKv: kv });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });
    const grant = await app.fetch(
      new Request("https://h/api/workspaces/ws-1/shares", {
        method: "POST",
        headers: {
          Authorization: `Bearer ${issued.token}`,
          "Content-Type": "application/json"
        },
        body: JSON.stringify({ shared_with: "public", permission: "read" })
      }),
      env,
      noopCtx
    );
    const grantBody = (await grant.json()) as {
      share: { id: string };
      shareToken: string;
      shareTokenJti: string;
    };

    // Revoke with explicit jti so the route writes the denylist entry.
    const revokeUrl = `https://h/api/workspaces/ws-1/shares/${grantBody.share.id}?jti=${encodeURIComponent(grantBody.shareTokenJti)}`;
    const revoke = await app.fetch(
      new Request(revokeUrl, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${issued.token}` }
      }),
      env,
      noopCtx
    );
    expect(revoke.status).toBe(200);

    // Re-validating the token now returns denylisted.
    const verified = await validateShareToken(grantBody.shareToken, {
      signingKey: SIGNING_KEY,
      authKv: kv.asKv()
    });
    expect(verified.ok).toBe(false);
    if (!verified.ok) expect(verified.reason).toBe("denylisted");
  });
});

describe("share routes — audit log row counts", () => {
  it("each grant + each revoke writes one row", async () => {
    const sqlite = setupDb();
    seedUser(sqlite, "owner-1", "o@x.com");
    seedUser(sqlite, "f1", "f1@x.com");
    seedUser(sqlite, "f2", "f2@x.com");
    seedWorkspace(sqlite, "ws-1", "owner-1");
    const env = buildEnv({ sqlite });
    const app = createApp();
    const issued = await issueHelmJwt({
      userId: "owner-1",
      signingKey: SIGNING_KEY
    });

    for (const recipient of ["f1", "f2"]) {
      const r = await app.fetch(
        new Request("https://h/api/workspaces/ws-1/shares", {
          method: "POST",
          headers: {
            Authorization: `Bearer ${issued.token}`,
            "Content-Type": "application/json"
          },
          body: JSON.stringify({ shared_with: recipient, permission: "read" })
        }),
        env,
        noopCtx
      );
      expect(r.status).toBe(201);
    }
    expect(countAuditByAction(sqlite, "share.granted")).toBe(2);

    // Revoke one
    const ses = drizzleSqlite(sqlite, { schema: { auditLog, shares, sessions, tasks, users, workspaces } });
    const allShares = ses.select().from(shares).all();
    expect(allShares.length).toBe(2);
    const first = allShares[0]!;
    const r = await app.fetch(
      new Request(`https://h/api/workspaces/ws-1/shares/${first.id}`, {
        method: "DELETE",
        headers: { Authorization: `Bearer ${issued.token}` }
      }),
      env,
      noopCtx
    );
    expect(r.status).toBe(200);
    expect(countAuditByAction(sqlite, "share.revoked")).toBe(1);
  });
});
