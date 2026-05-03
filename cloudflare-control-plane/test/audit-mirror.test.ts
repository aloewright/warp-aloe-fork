import { describe, expect, it } from "vitest";
import {
  auditMirrorRouter,
  handleAuditSync,
  insertBatch,
  isAuditLogRow,
  partitionExtra,
  type AuditLogRow,
  type AuditMirrorEnv
} from "../src/workers/audit_mirror.js";

/**
 * Tiny in-memory D1Database stand-in that captures inserts so the test
 * can read them back. Mirrors just enough of the `D1Database` /
 * `D1PreparedStatement` surface used by `audit_mirror.ts`.
 */
function makeFakeD1() {
  const rows: Array<Record<string, unknown>> = [];

  const stmt = (sql: string, params: unknown[] = []) => ({
    bind(...newParams: unknown[]) {
      return stmt(sql, newParams);
    },
    async run() {
      rows.push({
        timestamp: params[0],
        task_id: params[1],
        agent_id: params[2],
        rule: params[3],
        action: params[4],
        offending_path: params[5],
        detail: params[6],
        extra: params[7]
      });
      return { meta: { changes: 1 } };
    },
    async all() {
      // Used by the readback path; sql is the simple SELECT below.
      return { results: [...rows] };
    }
  });

  const db = {
    prepare(sql: string) {
      return stmt(sql);
    },
    async batch(stmts: Array<ReturnType<typeof stmt>>) {
      const out = [];
      for (const s of stmts) out.push(await s.run());
      return out;
    }
  };

  return {
    db: db as unknown as D1Database,
    rows
  };
}

function makeRequest(body: unknown, init: RequestInit = {}): Request {
  return new Request("https://control.example/api/audit/sync", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: typeof body === "string" ? body : JSON.stringify(body),
    ...init
  });
}

describe("audit_mirror.partitionExtra", () => {
  it("splits known fields from unknowns", () => {
    const row: AuditLogRow = {
      timestamp: "2026-05-01T00:00:00Z",
      task_id: "t1",
      agent_id: "a1",
      rule: "diff_size",
      action: "blocked",
      offending_path: "src/main.rs",
      detail: "too big",
      severity: "error",
      meta: { build_id: "abc" }
    };
    const { known, extra } = partitionExtra(row);
    expect(known.timestamp).toBe("2026-05-01T00:00:00Z");
    expect(known.task_id).toBe("t1");
    expect(extra).toEqual({ severity: "error", meta: { build_id: "abc" } });
  });

  it("defaults missing string fields to empty", () => {
    const { known } = partitionExtra({ timestamp: "2026-05-01T00:00:00Z" });
    expect(known.task_id).toBe("");
    expect(known.detail).toBe("");
  });
});

describe("audit_mirror.isAuditLogRow", () => {
  it("rejects non-object bodies", () => {
    expect(isAuditLogRow("foo")).toBe(false);
    expect(isAuditLogRow(null)).toBe(false);
    expect(isAuditLogRow(42)).toBe(false);
  });

  it("rejects rows without a timestamp", () => {
    expect(isAuditLogRow({ action: "blocked" })).toBe(false);
  });

  it("rejects rows with a wrong-typed known field", () => {
    expect(
      isAuditLogRow({ timestamp: "2026-05-01T00:00:00Z", task_id: 42 })
    ).toBe(false);
  });

  it("accepts a valid row", () => {
    expect(
      isAuditLogRow({
        timestamp: "2026-05-01T00:00:00Z",
        action: "blocked"
      })
    ).toBe(true);
  });
});

describe("audit_mirror.insertBatch", () => {
  it("is a no-op for an empty batch", async () => {
    const { db, rows } = makeFakeD1();
    const inserted = await insertBatch(db, []);
    expect(inserted).toBe(0);
    expect(rows).toHaveLength(0);
  });

  it("writes one row per entry", async () => {
    const { db, rows } = makeFakeD1();
    const batch: AuditLogRow[] = [
      {
        timestamp: "2026-05-01T00:00:00Z",
        task_id: "t1",
        agent_id: "a1",
        rule: "diff_size",
        action: "blocked",
        offending_path: "src/main.rs",
        detail: "too big"
      },
      {
        timestamp: "2026-05-01T00:01:00Z",
        action: "allowed",
        custom_field: "x"
      }
    ];
    const inserted = await insertBatch(db, batch);
    expect(inserted).toBe(2);
    expect(rows).toHaveLength(2);
    expect(rows[0]!.task_id).toBe("t1");
    expect(rows[1]!.extra).toBe(JSON.stringify({ custom_field: "x" }));
  });
});

describe("audit_mirror.handleAuditSync", () => {
  it("rejects non-POST", async () => {
    const env = { HELM_AUDIT_DB: makeFakeD1().db } as AuditMirrorEnv;
    const resp = await handleAuditSync(
      new Request("https://x/api/audit/sync"),
      env
    );
    expect(resp.status).toBe(405);
  });

  it("rejects malformed JSON", async () => {
    const env = { HELM_AUDIT_DB: makeFakeD1().db } as AuditMirrorEnv;
    const resp = await handleAuditSync(makeRequest("not json", {}), env);
    expect(resp.status).toBe(400);
    expect(((await resp.json()) as { error: string }).error).toBe(
      "invalid_json"
    );
  });

  it("rejects non-array bodies", async () => {
    const env = { HELM_AUDIT_DB: makeFakeD1().db } as AuditMirrorEnv;
    const resp = await handleAuditSync(makeRequest({ foo: 1 }), env);
    expect(resp.status).toBe(400);
    expect(((await resp.json()) as { error: string }).error).toBe(
      "expected_array"
    );
  });

  it("rejects rows that miss `timestamp`", async () => {
    const env = { HELM_AUDIT_DB: makeFakeD1().db } as AuditMirrorEnv;
    const resp = await handleAuditSync(
      makeRequest([{ action: "blocked" }]),
      env
    );
    expect(resp.status).toBe(400);
    const body = (await resp.json()) as { error: string; index: number };
    expect(body.error).toBe("invalid_row");
    expect(body.index).toBe(0);
  });

  it("returns 503 when the D1 binding is missing", async () => {
    const resp = await handleAuditSync(
      makeRequest([{ timestamp: "2026-05-01T00:00:00Z" }]),
      {} as AuditMirrorEnv
    );
    expect(resp.status).toBe(503);
  });

  it("round-trips 10 rows: POST -> insert -> select", async () => {
    const fake = makeFakeD1();
    const env = { HELM_AUDIT_DB: fake.db } as AuditMirrorEnv;

    const batch: AuditLogRow[] = Array.from({ length: 10 }, (_, i) => ({
      timestamp: `2026-05-01T00:${String(i).padStart(2, "0")}:00Z`,
      task_id: `t${i}`,
      agent_id: `a${i % 3}`,
      rule: i % 2 === 0 ? "diff_size" : "budget_exceeded",
      action: i === 7 ? "blocked" : "allowed",
      offending_path: i === 7 ? "src/secret.rs" : "",
      detail: `row ${i}`
    }));

    const resp = await handleAuditSync(makeRequest(batch), env);
    expect(resp.status).toBe(200);
    const body = (await resp.json()) as { inserted: number };
    expect(body.inserted).toBe(10);

    // Read them back via the same fake binding (simulating
    // `getDb(env).select().from(auditLog)` in the PDX-22 schema).
    const select = await fake.db
      .prepare("SELECT * FROM audit_log ORDER BY timestamp ASC")
      .all<Record<string, unknown>>();
    const results = select.results ?? [];
    expect(results).toHaveLength(10);
    expect(results[0]!.task_id).toBe("t0");
    expect(results[7]!.action).toBe("blocked");
    expect(results[7]!.offending_path).toBe("src/secret.rs");
    // No `extra` was set on these rows -- the column should be null.
    expect(results[0]!.extra).toBeNull();
  });
});

describe("audit_mirror.auditMirrorRouter", () => {
  it("exposes a fetch handler matching the standalone entrypoint", async () => {
    const fake = makeFakeD1();
    const env = { HELM_AUDIT_DB: fake.db } as AuditMirrorEnv;
    const resp = await auditMirrorRouter.fetch(
      makeRequest([{ timestamp: "2026-05-01T00:00:00Z", action: "x" }]),
      env
    );
    expect(resp.status).toBe(200);
  });
});
