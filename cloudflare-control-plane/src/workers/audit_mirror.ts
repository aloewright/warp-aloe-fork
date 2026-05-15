/**
 * D1 mirror of the symphony JSONL audit log (PDX-115 / PDX-22).
 *
 * Receives batches from `warp audit sync --remote ...` and writes them
 * into the `audit_log` D1 table. The schema is owned by PDX-22; this
 * module deliberately does not redefine it. Instead we issue parameterised
 * INSERTs against the live binding so the writer stays compatible with
 * whatever columns PDX-22 lands.
 *
 * The route is mounted as `POST /api/audit/sync`. Operators can either:
 *   * Hand it to a host Worker via [`auditMirrorRouter`]:
 *
 *       app.route("/api/audit/sync", auditMirrorRouter);
 *
 *     (the function signature matches Hono's `app.route(path, handler)`,
 *     but works equally well with any router that accepts a
 *     `(request, env, ctx) => Response`).
 *   * Or run this file standalone — `default.fetch` provides a tiny
 *     URL-pathname router for tests + dev.
 *
 * Every entry in the request body is validated against the symphony
 * audit-log shape from PDX-28 before insert. Malformed rows reject the
 * whole batch with a 400 (the CLI's `--batch-size` keeps blast radius
 * bounded).
 */

import { json, methodNotAllowed, notFound } from "../shared/http.js";

/** Cloudflare Workers `D1Database` minimal surface used here. */
export interface AuditMirrorEnv {
  /**
   * D1 binding for the table from PDX-22. Production wires this to the
   * `helm-audit` D1 in `wrangler.control-plane.toml`; tests pass an
   * `D1Database` returned from miniflare.
   */
  HELM_AUDIT_DB?: D1Database;
}

/** Maximum number of audit rows allowed in a single batch (DoS prevention). */
export const MAX_AUDIT_BATCH_SIZE = 500;

/** Single audit-log row, matching the JSONL shape from PDX-28. */
export interface AuditLogRow {
  timestamp: string;
  task_id?: string;
  agent_id?: string;
  rule?: string;
  action?: string;
  offending_path?: string;
  detail?: string;
  /** Free-form fields preserved verbatim under `extra` JSON. */
  [key: string]: unknown;
}

const KNOWN_FIELDS = new Set([
  "timestamp",
  "task_id",
  "agent_id",
  "rule",
  "action",
  "offending_path",
  "detail"
]);

/** Type-guard for a single row. */
export function isAuditLogRow(value: unknown): value is AuditLogRow {
  if (typeof value !== "object" || value === null) return false;
  const v = value as Record<string, unknown>;
  if (typeof v.timestamp !== "string" || v.timestamp.length === 0) return false;
  for (const key of [
    "task_id",
    "agent_id",
    "rule",
    "action",
    "offending_path",
    "detail"
  ]) {
    if (v[key] !== undefined && typeof v[key] !== "string") return false;
  }
  return true;
}

/** Split off the well-known fields from anything extra. */
export function partitionExtra(row: AuditLogRow): {
  known: Required<
    Pick<
      AuditLogRow,
      | "timestamp"
      | "task_id"
      | "agent_id"
      | "rule"
      | "action"
      | "offending_path"
      | "detail"
    >
  >;
  extra: Record<string, unknown>;
} {
  const extra: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(row)) {
    if (!KNOWN_FIELDS.has(k)) extra[k] = v;
  }
  return {
    known: {
      timestamp: row.timestamp,
      task_id: row.task_id ?? "",
      agent_id: row.agent_id ?? "",
      rule: row.rule ?? "",
      action: row.action ?? "",
      offending_path: row.offending_path ?? "",
      detail: row.detail ?? ""
    },
    extra
  };
}

/**
 * Insert a batch of rows into the `audit_log` D1 table.
 *
 * Returns the count of rows actually written. Uses `D1Database.batch` to
 * avoid round-trips per row.
 */
export async function insertBatch(
  db: D1Database,
  rows: AuditLogRow[]
): Promise<number> {
  if (rows.length === 0) return 0;

  // Bind a parameterised INSERT for each row. Using positional params keeps
  // us compatible with whatever exact column shape PDX-22 lands as long as
  // the column ordering below stays in sync.
  const sql =
    "INSERT INTO audit_log (timestamp, task_id, agent_id, rule, action, offending_path, detail, extra) " +
    "VALUES (?, ?, ?, ?, ?, ?, ?, ?)";
  const stmts = rows.map((row) => {
    const { known, extra } = partitionExtra(row);
    return db
      .prepare(sql)
      .bind(
        known.timestamp,
        known.task_id,
        known.agent_id,
        known.rule,
        known.action,
        known.offending_path,
        known.detail,
        Object.keys(extra).length === 0 ? null : JSON.stringify(extra)
      );
  });
  const results = await db.batch(stmts);
  return results.reduce((acc, r) => acc + (r.meta?.changes ?? 0), 0);
}

/**
 * Handle `POST /api/audit/sync` for a single request.
 *
 * Body must be a JSON array of audit-log rows; anything else returns 400.
 * On success returns `{ inserted: <n> }`.
 */
export async function handleAuditSync(
  request: Request,
  env: AuditMirrorEnv
): Promise<Response> {
  if (request.method !== "POST") return methodNotAllowed();

  let body: unknown;
  try {
    body = await request.json();
  } catch {
    return json({ error: "invalid_json" }, { status: 400 });
  }

  if (!Array.isArray(body)) {
    return json(
      { error: "expected_array", message: "Body must be a JSON array of audit rows." },
      { status: 400 }
    );
  }

  if (body.length > MAX_AUDIT_BATCH_SIZE) {
    return json(
      {
        error: "batch_too_large",
        message: `Batch size exceeds maximum of ${MAX_AUDIT_BATCH_SIZE} rows.`
      },
      { status: 400 }
    );
  }

  const rows: AuditLogRow[] = [];
  for (let i = 0; i < body.length; i += 1) {
    const row = body[i];
    if (!isAuditLogRow(row)) {
      return json(
        {
          error: "invalid_row",
          index: i,
          message: "Row missing `timestamp` or has wrong field types."
        },
        { status: 400 }
      );
    }
    rows.push(row);
  }

  if (!env.HELM_AUDIT_DB) {
    return json(
      { error: "no_binding", message: "HELM_AUDIT_DB D1 binding is not configured." },
      { status: 503 }
    );
  }

  try {
    const inserted = await insertBatch(env.HELM_AUDIT_DB, rows);
    return json({ inserted });
  } catch (err) {
    return json(
      {
        error: "insert_failed",
        message: err instanceof Error ? err.message : String(err)
      },
      { status: 500 }
    );
  }
}

/**
 * Hono-compatible router fragment.
 *
 * Mount in a host Worker (per the PDX-19 `app.route()` pattern):
 *
 * ```ts
 * app.route("/api/audit/sync", auditMirrorRouter);
 * ```
 *
 * The exported value is a function with the standard
 * `(request, env, ctx) => Response | Promise<Response>` shape that Hono's
 * `app.route()` accepts as a sub-app, and that any other Workers router
 * (itty, native fetch handler, etc.) accepts as well.
 */
export const auditMirrorRouter = {
  fetch: (request: Request, env: AuditMirrorEnv): Promise<Response> =>
    handleAuditSync(request, env)
};

/**
 * Standalone Worker entrypoint for tests / dev. The control-plane Worker
 * (`workers/control-plane.ts`) mounts the `/api/audit/sync` route via
 * [`auditMirrorRouter`]; this default export exists so the module can be
 * tested in isolation with `unstable_dev` / miniflare.
 */
export default {
  async fetch(request: Request, env: AuditMirrorEnv): Promise<Response> {
    const url = new URL(request.url);
    if (url.pathname === "/api/audit/sync") {
      return handleAuditSync(request, env);
    }
    return notFound();
  }
};
