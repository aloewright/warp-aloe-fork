/**
 * Share-aware permission middleware (PDX-116 [E4]).
 *
 * Wraps `requireHelmAuth` with two behaviours:
 *
 *   1. If a `?share_token=…` query parameter is present, validate it against
 *      `HELM_SHARE_TOKEN_SIGNING_KEY` (falling back to `HELM_JWT_SIGNING_KEY`
 *      so dev-mode environments can run with a single secret), look up the
 *      share row, and synthesize an `AuthenticatedRequestContext` with
 *      `source: "share"` and `userId = null` (public traffic). Audit-log
 *      attribution still runs — the `details` payload records the share id.
 *
 *   2. Otherwise, fall through to the helm session JWT path (PDX-23). After
 *      authentication succeeds, callers can use `assertResourceAccess` to
 *      check that the principal is the owner of the requested resource OR
 *      has an active `shares` row with sufficient permission.
 *
 * Resource model:
 *
 *   PDX-116 lets a user share three kinds of entities:
 *     - workspace  → `workspaces.id`
 *     - session    → `sessions.id` (owner = `sessions.user_id`)
 *     - run        → `tasks.id`    (owner = `sessions.user_id` of parent)
 *
 *   The `shares` table FK-references `resources.id`, so we materialize a
 *   single "share-handle" resource row per shared entity (`kind = "share-
 *   handle"`, `payload = { kind, targetId }`) and write `shares.resource_id`
 *   to that handle. Resolving a share = looking up the handle by its
 *   deterministic id `share-handle:{kind}:{targetId}`. This keeps the schema
 *   stable (we do NOT modify PDX-22) and lets the existing `shares` indexes
 *   continue to work.
 */

import { and, eq } from "drizzle-orm";

import {
  recordAuthEvent,
  validateShareToken,
  type AuthenticatedRequestContext,
  type AuthEnv
} from "./auth.js";
import { json } from "./http.js";
import {
  auditLog,
  getDb,
  resources,
  sessions,
  shares,
  tasks,
  users,
  workspaces,
  type HelmDb
} from "../db/index.js";

// ── Constants ───────────────────────────────────────────────────────────────

/**
 * Synthetic user row id used to satisfy the `shares.shared_with_user_id` FK
 * for public-link shares. The schema (PDX-22, frozen) makes the column NOT
 * NULL with a FK to `users(id)`, so we can't store `null`. Instead we
 * idempotently insert a `__public__` user the first time a public share is
 * created and then point `shared_with_user_id` at it. The prefix is reserved
 * — application code MUST NOT issue a real helm JWT with `sub = "__public__"`.
 */
export const PUBLIC_USER_ID = "__public__";

/** Resource kinds we share. */
export type ShareableKind = "workspace" | "session" | "run";

/** All permission strings the schema allows. */
export type SharePermission = "read" | "write" | "admin";

/** `kind` value written to the synthetic resources row that fronts a share. */
export const SHARE_HANDLE_KIND = "share-handle";

/** Stable id for the share-handle resource row that fronts an entity. */
export function shareHandleId(kind: ShareableKind, targetId: string): string {
  return `share-handle:${kind}:${targetId}`;
}

// ── Env shape ───────────────────────────────────────────────────────────────

export interface ShareAuthEnv extends AuthEnv {
  /**
   * Optional separate signing key for share tokens. Falls back to
   * `HELM_JWT_SIGNING_KEY` when unset so single-secret dev environments
   * still work. Production is expected to set both.
   */
  HELM_SHARE_TOKEN_SIGNING_KEY?: string;
  DB: D1Database;
}

function resolveShareSigningKey(env: ShareAuthEnv): string | null {
  return env.HELM_SHARE_TOKEN_SIGNING_KEY ?? env.HELM_JWT_SIGNING_KEY ?? null;
}

// ── Owner resolution ────────────────────────────────────────────────────────

/**
 * Resolve the owner user_id of a shareable entity. Returns `null` when the
 * entity does not exist (route layer turns this into a 404).
 */
export async function resolveOwnerUserId(
  db: HelmDb,
  kind: ShareableKind,
  targetId: string
): Promise<string | null> {
  if (kind === "workspace") {
    const rows = await db
      .select({ ownerUserId: workspaces.ownerUserId })
      .from(workspaces)
      .where(eq(workspaces.id, targetId))
      .all();
    return rows[0]?.ownerUserId ?? null;
  }
  if (kind === "session") {
    const rows = await db
      .select({ userId: sessions.userId })
      .from(sessions)
      .where(eq(sessions.id, targetId))
      .all();
    return rows[0]?.userId ?? null;
  }
  // kind === "run"
  const rows = await db
    .select({ userId: sessions.userId })
    .from(tasks)
    .innerJoin(sessions, eq(sessions.id, tasks.sessionId))
    .where(eq(tasks.id, targetId))
    .all();
  return rows[0]?.userId ?? null;
}

/**
 * Workspace id used by the synthetic share-handle resource row. For
 * workspace shares, this is the workspace itself. For sessions and runs we
 * fall back to a per-owner "personal" workspace, materialized lazily.
 */
async function resolveHandleWorkspaceId(
  db: HelmDb,
  kind: ShareableKind,
  targetId: string,
  ownerUserId: string
): Promise<string> {
  if (kind === "workspace") return targetId;
  // Sessions and runs aren't workspace-scoped in PDX-22's schema, so we
  // materialize a per-user "personal" workspace and attach the share-handle
  // resource row to it.
  const personalWorkspaceId = `personal:${ownerUserId}`;
  const existing = await db
    .select({ id: workspaces.id })
    .from(workspaces)
    .where(eq(workspaces.id, personalWorkspaceId))
    .all();
  if (existing.length === 0) {
    await db
      .insert(workspaces)
      .values({
        id: personalWorkspaceId,
        ownerUserId,
        name: "Personal"
      })
      .onConflictDoNothing();
  }
  return personalWorkspaceId;
}

/**
 * Idempotently materialize a share-handle resources row for the entity, and
 * return its id. The handle is what `shares.resource_id` points at — we use
 * it instead of mutating the schema to add new FK targets per kind.
 */
export async function ensureShareHandle(
  db: HelmDb,
  kind: ShareableKind,
  targetId: string,
  ownerUserId: string
): Promise<string> {
  const handleId = shareHandleId(kind, targetId);
  const existing = await db
    .select({ id: resources.id })
    .from(resources)
    .where(eq(resources.id, handleId))
    .all();
  if (existing.length > 0) return handleId;

  const workspaceId = await resolveHandleWorkspaceId(
    db,
    kind,
    targetId,
    ownerUserId
  );
  await db
    .insert(resources)
    .values({
      id: handleId,
      workspaceId,
      kind: SHARE_HANDLE_KIND,
      payload: { shareableKind: kind, targetId }
    })
    .onConflictDoNothing();
  return handleId;
}

/** Idempotently insert the synthetic public-user row. Safe to call repeatedly. */
export async function ensurePublicUser(db: HelmDb): Promise<void> {
  const existing = await db
    .select({ id: users.id })
    .from(users)
    .where(eq(users.id, PUBLIC_USER_ID))
    .all();
  if (existing.length > 0) return;
  await db
    .insert(users)
    .values({
      id: PUBLIC_USER_ID,
      email: "public@share.local"
    })
    .onConflictDoNothing();
}

// ── Permission lookup ───────────────────────────────────────────────────────

/** Ordered ranking — higher rank wins. */
const PERMISSION_RANK: Record<SharePermission, number> = {
  read: 1,
  write: 2,
  admin: 3
};

function permissionGrants(
  granted: SharePermission,
  required: SharePermission
): boolean {
  return PERMISSION_RANK[granted] >= PERMISSION_RANK[required];
}

/**
 * Find an active share row granting `userId` access to the given entity at
 * `required` permission or above. Returns the share row id when one exists,
 * otherwise `null`.
 */
export async function findActiveShareForUser(
  db: HelmDb,
  kind: ShareableKind,
  targetId: string,
  userId: string,
  required: SharePermission = "read"
): Promise<{ id: string; permission: SharePermission } | null> {
  const handleId = shareHandleId(kind, targetId);
  const rows = await db
    .select({ id: shares.id, permission: shares.permission })
    .from(shares)
    .where(
      and(
        eq(shares.resourceId, handleId),
        eq(shares.sharedWithUserId, userId)
      )
    )
    .all();
  for (const row of rows) {
    if (permissionGrants(row.permission as SharePermission, required)) {
      return { id: row.id, permission: row.permission as SharePermission };
    }
  }
  return null;
}

// ── Result type used by route handlers ──────────────────────────────────────

export type ShareAccessResult =
  | {
      ok: true;
      ctx: AuthenticatedRequestContext;
      /**
       * Either "owner" (caller owns the entity), "share" (caller has an
       * active share row), or "share_token" (caller presented a public link).
       */
      via: "owner" | "share" | "share_token";
      shareId?: string;
      shareTokenJti?: string;
    }
  | { ok: false; response: Response };

/**
 * Load and validate a `?share_token=…` query parameter against the configured
 * signing key. Returns the share row (and its handle's resource id) on
 * success. The caller is expected to verify the share row's resource matches
 * the entity in the URL — `assertResourceAccess` handles that.
 */
export async function authenticateShareToken(
  request: Request,
  env: ShareAuthEnv
): Promise<
  | { ok: true; shareId: string; jti: string; row: typeof shares.$inferSelect }
  | { ok: false; reason: string }
> {
  const url = new URL(request.url);
  const token = url.searchParams.get("share_token");
  if (!token) return { ok: false, reason: "no_token" };
  const signingKey = resolveShareSigningKey(env);
  if (!signingKey) return { ok: false, reason: "misconfigured" };

  const verified = await validateShareToken(token, {
    signingKey,
    authKv: env.AUTH_KV
  });
  if (!verified.ok) return { ok: false, reason: verified.reason };

  const db = getDb(env);
  const rows = await db
    .select()
    .from(shares)
    .where(eq(shares.id, verified.shareId))
    .all();
  const row = rows[0];
  if (!row) return { ok: false, reason: "share_not_found" };
  return { ok: true, shareId: verified.shareId, jti: verified.jti, row };
}

/**
 * Single entry-point used by share-protected routes:
 *
 *   - Public-link path: `?share_token=…` is set and the caller is otherwise
 *     unauthenticated. We verify the token, confirm the share row points at
 *     the requested entity, and return a synthetic context.
 *
 *   - Authenticated path: caller must have a helm-JWT-derived
 *     {@link AuthenticatedRequestContext} (already populated by `helmAuth`).
 *     We allow when the caller owns the entity OR has an active share row.
 *
 * On every successful access via a share or share token we write a row to
 * `audit_log` with `action = "share.access"`. The route's normal
 * `withAuditAttribution` wrapper still records the underlying HTTP
 * invocation; the `share.access` row is the access-control event.
 */
export async function assertResourceAccess(
  request: Request,
  env: ShareAuthEnv,
  args: {
    kind: ShareableKind;
    targetId: string;
    required?: SharePermission;
    /**
     * Pre-populated principal context from the helm-auth middleware. Pass
     * `null` when the route is share-token-only (e.g. public transcript).
     */
    principal: AuthenticatedRequestContext | null;
  },
  ctx?: { waitUntil: (p: Promise<unknown>) => void }
): Promise<ShareAccessResult> {
  const required: SharePermission = args.required ?? "read";
  const db = getDb(env);

  // Path 1 — public share-token in the query string. Always wins for routes
  // that opt into share-token access; the principal (if any) is ignored.
  const url = new URL(request.url);
  const hasShareToken = url.searchParams.has("share_token");
  if (hasShareToken) {
    const tokenResult = await authenticateShareToken(request, env);
    if (!tokenResult.ok) {
      return {
        ok: false,
        response: json(
          { error: "unauthorized", reason: tokenResult.reason },
          { status: 401 }
        )
      };
    }
    // The share row's resource_id must match the requested entity.
    const expectedHandle = shareHandleId(args.kind, args.targetId);
    if (tokenResult.row.resourceId !== expectedHandle) {
      return {
        ok: false,
        response: json(
          { error: "forbidden", reason: "share_target_mismatch" },
          { status: 403 }
        )
      };
    }
    if (
      !permissionGrants(
        tokenResult.row.permission as SharePermission,
        required
      )
    ) {
      return {
        ok: false,
        response: json(
          { error: "forbidden", reason: "insufficient_permission" },
          { status: 403 }
        )
      };
    }
    await recordShareAccess(env, {
      userId: null,
      shareId: tokenResult.shareId,
      via: "share_token",
      jti: tokenResult.jti,
      kind: args.kind,
      targetId: args.targetId
    }, ctx);
    return {
      ok: true,
      ctx: {
        userId: null,
        source: "share",
        scope: `share:${tokenResult.shareId}`
      },
      via: "share_token",
      shareId: tokenResult.shareId,
      shareTokenJti: tokenResult.jti
    };
  }

  // Path 2 — authenticated principal.
  if (!args.principal || !args.principal.userId) {
    return {
      ok: false,
      response: json(
        {
          error: "unauthorized",
          message: "A helm session JWT or share_token is required."
        },
        { status: 401 }
      )
    };
  }

  const ownerUserId = await resolveOwnerUserId(db, args.kind, args.targetId);
  if (!ownerUserId) {
    return {
      ok: false,
      response: json({ error: "not_found" }, { status: 404 })
    };
  }
  if (ownerUserId === args.principal.userId) {
    return { ok: true, ctx: args.principal, via: "owner" };
  }

  const share = await findActiveShareForUser(
    db,
    args.kind,
    args.targetId,
    args.principal.userId,
    required
  );
  if (!share) {
    return {
      ok: false,
      response: json(
        { error: "forbidden", reason: "no_share" },
        { status: 403 }
      )
    };
  }

  await recordShareAccess(env, {
    userId: args.principal.userId,
    shareId: share.id,
    via: "share",
    kind: args.kind,
    targetId: args.targetId
  }, ctx);
  return {
    ok: true,
    ctx: args.principal,
    via: "share",
    shareId: share.id
  };
}

// ── Audit helpers ───────────────────────────────────────────────────────────

/**
 * Write a `share.access` row to `audit_log`. The route layer's request-level
 * audit wrapper still writes the canonical `http.request` row; this is a
 * separate, explicit access-control event so administrators can audit
 * share-mediated traffic independent of HTTP path patterns.
 */
export async function recordShareAccess(
  env: ShareAuthEnv,
  args: {
    userId: string | null;
    shareId: string;
    via: "share" | "share_token";
    jti?: string;
    kind: ShareableKind;
    targetId: string;
  },
  ctx?: { waitUntil: (p: Promise<unknown>) => void }
): Promise<void> {
  const promise = (async () => {
    try {
      await getDb(env).insert(auditLog).values({
        id: crypto.randomUUID(),
        userId: args.userId,
        action: "share.access",
        targetKind: args.kind,
        targetId: args.targetId,
        details: {
          shareId: args.shareId,
          via: args.via,
          ...(args.jti ? { jti: args.jti } : {})
        }
      });
    } catch {
      // Audit failures must not break the access path.
    }
  })();

  if (ctx) {
    ctx.waitUntil(promise);
  } else {
    await promise;
  }
}

/** Helper used by the grant route. Logs `action = "share.granted"`. */
export async function recordShareGrant(
  env: ShareAuthEnv,
  args: {
    userId: string | null;
    shareId: string;
    kind: ShareableKind;
    targetId: string;
    permission: SharePermission;
    sharedWith: string | "public";
  },
  ctx?: { waitUntil: (p: Promise<unknown>) => void }
): Promise<void> {
  await recordAuthEvent(env, {
    userId: args.userId,
    action: "share.granted",
    jti: args.shareId,
    details: {
      shareId: args.shareId,
      kind: args.kind,
      targetId: args.targetId,
      permission: args.permission,
      sharedWith: args.sharedWith
    }
  }, ctx);
}

/** Helper used by the revoke route. Logs `action = "share.revoked"`. */
export async function recordShareRevoke(
  env: ShareAuthEnv,
  args: {
    userId: string | null;
    shareId: string;
    kind: ShareableKind;
    targetId: string;
  },
  ctx?: { waitUntil: (p: Promise<unknown>) => void }
): Promise<void> {
  await recordAuthEvent(env, {
    userId: args.userId,
    action: "share.revoked",
    jti: args.shareId,
    details: {
      shareId: args.shareId,
      kind: args.kind,
      targetId: args.targetId
    }
  }, ctx);
}
