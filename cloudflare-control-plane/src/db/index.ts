/**
 * Drizzle client factory for the Helm D1 database (PDX-22).
 *
 * Usage inside a Worker:
 *
 * ```ts
 * import { getDb } from "../db/index.js";
 * import { users } from "../db/schema.js";
 *
 * export default {
 *   async fetch(_req: Request, env: Env): Promise<Response> {
 *     const db = getDb(env);
 *     const all = await db.select().from(users).all();
 *     return Response.json(all);
 *   }
 * };
 * ```
 *
 * The `Env` type for control plane Workers includes the D1 binding `DB`
 * declared in `wrangler.control-plane.toml`.
 *
 * For PDX-19 (Workers) and PDX-20 (Durable Objects), import this module
 * and the schema module — never re-create the Drizzle client by hand.
 */

import { drizzle, type DrizzleD1Database } from "drizzle-orm/d1";

import * as schema from "./schema.js";

/** Minimal env shape required by `getDb`. Worker `Env` types should extend this. */
export interface DbEnv {
  DB: D1Database;
}

/** A Drizzle client typed against the full Helm schema. */
export type HelmDb = DrizzleD1Database<typeof schema>;

const dbCache = new WeakMap<D1Database, HelmDb>();

/**
 * Build a Drizzle client backed by `env.DB` (the D1 binding declared in
 * `wrangler.control-plane.toml`). Cheap to call per-request.
 */
export function getDb(env: DbEnv): HelmDb {
  const cached = dbCache.get(env.DB);
  if (cached) return cached;
  const db = drizzle(env.DB, { schema });
  dbCache.set(env.DB, db);
  return db;
}

export * as schemaNs from "./schema.js";
export {
  users,
  workspaces,
  resources,
  shares,
  syncEvents,
  sessions,
  tasks,
  auditLog
} from "./schema.js";
export type {
  User,
  UserInsert,
  Workspace,
  WorkspaceInsert,
  Resource,
  ResourceInsert,
  Share,
  ShareInsert,
  SyncEvent,
  SyncEventInsert,
  Session,
  SessionInsert,
  Task,
  TaskInsert,
  AuditLogEntry,
  AuditLogInsert
} from "./schema.js";
