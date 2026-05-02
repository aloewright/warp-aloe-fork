/**
 * Helm D1 schema (PDX-22).
 *
 * Drizzle ORM schema for the Cloudflare D1 database backing the Helm
 * control plane. This is the canonical source of truth for the persistent
 * relational state owned by the control plane Worker.
 *
 * Conceptual model
 * ----------------
 * - `users` are the principal identity. Workspaces, sessions, tasks, and
 *   audit log entries all reference users (audit_log allows null user_id
 *   for system-initiated actions).
 * - `workspaces` group `resources`. Resources are versioned blobs of JSON
 *   addressed by `kind`.
 * - `shares` grant a permission on a resource to another user.
 * - `sync_events` form a monotonic per-user log used for client mirror
 *   replay and conflict resolution. Consumed by PDX-20 (Durable Objects)
 *   for the real-time broadcast story.
 * - `sessions` represent a long-lived conversation between a user and an
 *   agent. `tasks` are individual prompt/response units inside a session.
 * - `audit_log` is global, append-only, and supports auth attribution
 *   (PDX-23) plus guardrail trips (PDX-28). user_id is nullable so the
 *   table can record system actions.
 *
 * The shape of the legacy on-device `skill_usage_events` table in
 * `crates/persistence` is preserved by `tasks.result` (JSON payload) plus
 * `audit_log.details` (JSON payload) so the local SQLite mirror story in
 * later phases of PDX-22 stays straightforward.
 */

import { sql } from "drizzle-orm";
import {
  index,
  integer,
  primaryKey,
  sqliteTable,
  text,
  uniqueIndex
} from "drizzle-orm/sqlite-core";

// ── users ───────────────────────────────────────────────────────────────────

export const users = sqliteTable(
  "users",
  {
    id: text("id").primaryKey(),
    email: text("email").notNull(),
    createdAt: integer("created_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`),
    updatedAt: integer("updated_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`)
  },
  (table) => ({
    emailUnique: uniqueIndex("users_email_unique").on(table.email)
  })
);

// ── workspaces ──────────────────────────────────────────────────────────────

export const workspaces = sqliteTable(
  "workspaces",
  {
    id: text("id").primaryKey(),
    ownerUserId: text("owner_user_id")
      .notNull()
      .references(() => users.id, { onDelete: "cascade" }),
    name: text("name").notNull(),
    createdAt: integer("created_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`)
  },
  (table) => ({
    ownerIdx: index("workspaces_owner_idx").on(table.ownerUserId)
  })
);

// ── resources ───────────────────────────────────────────────────────────────

export const resources = sqliteTable(
  "resources",
  {
    id: text("id").primaryKey(),
    workspaceId: text("workspace_id")
      .notNull()
      .references(() => workspaces.id, { onDelete: "cascade" }),
    kind: text("kind").notNull(),
    payload: text("payload", { mode: "json" }).notNull(),
    version: integer("version").notNull().default(1),
    createdAt: integer("created_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`),
    updatedAt: integer("updated_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`)
  },
  (table) => ({
    workspaceIdx: index("resources_workspace_idx").on(table.workspaceId),
    kindIdx: index("resources_kind_idx").on(table.kind)
  })
);

// ── shares ──────────────────────────────────────────────────────────────────

export const shares = sqliteTable(
  "shares",
  {
    id: text("id").primaryKey(),
    resourceId: text("resource_id")
      .notNull()
      .references(() => resources.id, { onDelete: "cascade" }),
    sharedWithUserId: text("shared_with_user_id")
      .notNull()
      .references(() => users.id, { onDelete: "cascade" }),
    permission: text("permission", { enum: ["read", "write", "admin"] }).notNull(),
    createdAt: integer("created_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`)
  },
  (table) => ({
    uniqueGrant: uniqueIndex("shares_resource_user_unique").on(
      table.resourceId,
      table.sharedWithUserId
    ),
    userIdx: index("shares_user_idx").on(table.sharedWithUserId)
  })
);

// ── sync_events ─────────────────────────────────────────────────────────────

export const syncEvents = sqliteTable(
  "sync_events",
  {
    id: text("id").primaryKey(),
    userId: text("user_id")
      .notNull()
      .references(() => users.id, { onDelete: "cascade" }),
    resourceId: text("resource_id")
      .notNull()
      .references(() => resources.id, { onDelete: "cascade" }),
    kind: text("kind", { enum: ["created", "updated", "deleted"] }).notNull(),
    sequence: integer("sequence").notNull(),
    createdAt: integer("created_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`)
  },
  (table) => ({
    userSequenceUnique: uniqueIndex("sync_events_user_sequence_unique").on(
      table.userId,
      table.sequence
    ),
    resourceIdx: index("sync_events_resource_idx").on(table.resourceId)
  })
);

// ── sessions ────────────────────────────────────────────────────────────────

export const sessions = sqliteTable(
  "sessions",
  {
    id: text("id").primaryKey(),
    userId: text("user_id")
      .notNull()
      .references(() => users.id, { onDelete: "cascade" }),
    agentId: text("agent_id").notNull(),
    startedAt: integer("started_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`),
    endedAt: integer("ended_at", { mode: "timestamp_ms" }),
    taskId: text("task_id")
  },
  (table) => ({
    userIdx: index("sessions_user_idx").on(table.userId),
    agentIdx: index("sessions_agent_idx").on(table.agentId)
  })
);

// ── tasks ───────────────────────────────────────────────────────────────────

export const tasks = sqliteTable(
  "tasks",
  {
    id: text("id").primaryKey(),
    sessionId: text("session_id")
      .notNull()
      .references(() => sessions.id, { onDelete: "cascade" }),
    prompt: text("prompt").notNull(),
    status: text("status", {
      enum: ["queued", "running", "succeeded", "failed", "cancelled"]
    })
      .notNull()
      .default("queued"),
    result: text("result", { mode: "json" }),
    createdAt: integer("created_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`),
    updatedAt: integer("updated_at", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`)
  },
  (table) => ({
    sessionIdx: index("tasks_session_idx").on(table.sessionId),
    statusIdx: index("tasks_status_idx").on(table.status)
  })
);

// ── audit_log ───────────────────────────────────────────────────────────────
//
// Shared by:
//   - PDX-23: auth attribution (action = "auth.signin", "auth.signout", ...)
//   - PDX-28: guardrail trips (action = "guardrail.trip", target_kind = the
//     subsystem that fired, details = JSON of the policy + payload).
//
// user_id is nullable so the table can record system actions (cron jobs,
// background sweeps, anonymous public endpoints).

export const auditLog = sqliteTable(
  "audit_log",
  {
    id: text("id").primaryKey(),
    timestamp: integer("timestamp", { mode: "timestamp_ms" })
      .notNull()
      .default(sql`(unixepoch() * 1000)`),
    userId: text("user_id").references(() => users.id, { onDelete: "set null" }),
    action: text("action").notNull(),
    targetKind: text("target_kind").notNull(),
    targetId: text("target_id"),
    details: text("details", { mode: "json" })
  },
  (table) => ({
    timestampIdx: index("audit_log_timestamp_idx").on(table.timestamp),
    userIdx: index("audit_log_user_idx").on(table.userId),
    actionIdx: index("audit_log_action_idx").on(table.action),
    targetIdx: index("audit_log_target_idx").on(table.targetKind, table.targetId)
  })
);

// ── Type exports ────────────────────────────────────────────────────────────
//
// These are the canonical row types. PDX-19 (Workers) and PDX-20 (Durable
// Objects) consume `User`, `Workspace`, etc. Use the `*Insert` types when
// constructing values for `db.insert(...).values(...)`.

export type User = typeof users.$inferSelect;
export type UserInsert = typeof users.$inferInsert;

export type Workspace = typeof workspaces.$inferSelect;
export type WorkspaceInsert = typeof workspaces.$inferInsert;

export type Resource = typeof resources.$inferSelect;
export type ResourceInsert = typeof resources.$inferInsert;

export type Share = typeof shares.$inferSelect;
export type ShareInsert = typeof shares.$inferInsert;

export type SyncEvent = typeof syncEvents.$inferSelect;
export type SyncEventInsert = typeof syncEvents.$inferInsert;

export type Session = typeof sessions.$inferSelect;
export type SessionInsert = typeof sessions.$inferInsert;

export type Task = typeof tasks.$inferSelect;
export type TaskInsert = typeof tasks.$inferInsert;

export type AuditLogEntry = typeof auditLog.$inferSelect;
export type AuditLogInsert = typeof auditLog.$inferInsert;

// Suppress the "primaryKey is unused" import warning when consumers pull
// just the tables. primaryKey is exported here for downstream composite-key
// extensions (e.g. join tables) added by future PRs.
export { primaryKey };
