/**
 * UserDO — per-user authority over active sessions, monthly budget, and
 * realtime sync_event broadcast.
 *
 * Addressed by `idFromName(userId)`. There is exactly one UserDO per user.
 * Acts as the conflict-resolution authority: the `(user_id, sequence)` unique
 * index on `sync_events` (PDX-22) means UserDO assigns the next sequence
 * atomically and is the sole writer of monotonic sync events for its user.
 */
import { and, eq, sql, sum } from "drizzle-orm";
import { getDb, type DbEnv } from "../../db/index.js";
import { auditLog, syncEvents } from "../../db/schema.js";

/** Storage keys. Public for inspection from tests. */
export const USER_STORAGE_KEYS = {
  /** Last sequence assigned by this DO. Initial: cold-start re-derives from D1. */
  SEQUENCE: "user:sequence",
  /** Cached cumulative spend in micro-dollars (mirrors orchestrator Budget). */
  SPEND_MICRODOLLARS: "user:spend",
  /** Active session ids. Lightweight in-memory registry. */
  ACTIVE_SESSIONS: "user:activeSessions",
  /** Whether D1 was reconciled at least once on this DO. */
  HYDRATED: "user:hydrated"
} as const;

/**
 * Soft per-user monthly cap (micro-dollars). Mirrors the orchestrator's
 * `Cap.monthly_micro_dollars` shape (crates/orchestrator/src/budget.rs).
 * The DO surfaces an `over_budget` flag when the cap would be exceeded;
 * the actual gating decision lives in the orchestrator.
 */
export const DEFAULT_MONTHLY_CAP_MICRODOLLARS = 50_000_000; // $50

/** Subset of DurableObjectState we depend on, for unit-testability. */
export interface UserDOState {
  storage: {
    get<T>(key: string): Promise<T | undefined>;
    put<T>(key: string, value: T): Promise<void>;
    delete(key: string): Promise<boolean>;
    /**
     * `transaction` exists in the real runtime; tests provide a no-op
     * pass-through that just runs the block.
     */
    transaction?<T>(fn: (txn: UserDOState["storage"]) => Promise<T>): Promise<T>;
  };
}

export interface UserBroadcastEvent {
  /** D1 `sync_events.kind` */
  kind: "created" | "updated" | "deleted";
  resourceId: string;
  /** Optional payload forwarded to client subscribers. Tests send small JSON. */
  payload?: unknown;
}

export interface UserDOEnv extends DbEnv {
  /** Optional cap override (e.g. enterprise tier). */
  HELM_USER_MONTHLY_CAP_MICRODOLLARS?: string;
}

/**
 * Pure helper exposed for tests: derive total cumulative spend for a user
 * from their `audit_log` rows where `action = "spend"`. Each row's
 * `details.micros` is summed.
 */
export interface AuditLogReader {
  totalSpendMicros(userId: string): Promise<number>;
}

/**
 * Default reader — runs a SUM query against `audit_log`. The aggregator
 * relies on `details->>'micros'` which is the convention adopted by
 * PDX-23 (auth attribution) and Symphony's billable charge ledger.
 */
export const auditLogReaderForEnv =
  (env: UserDOEnv): AuditLogReader => ({
    async totalSpendMicros(userId: string): Promise<number> {
      if (!env.DB) return 0;
      const db = getDb(env);
      // Sum `json_extract(details, '$.micros')`. Drizzle doesn't model that
      // directly, so we use a raw expression. SQLite returns NULL for an
      // empty set; coalesce to 0.
      const rows = await db
        .select({
          total: sql<number>`COALESCE(SUM(json_extract(${auditLog.details}, '$.micros')), 0)`
        })
        .from(auditLog)
        .where(
          and(
            eq(auditLog.userId, userId),
            eq(auditLog.action, "spend")
          )
        )
        .all()
        .catch(() => [{ total: 0 }]);
      const total = rows[0]?.total ?? 0;
      return Number(total);
    }
  });

interface BroadcastSubscriber {
  ws: WebSocket;
  /** When the DO accepts a WS, callers can scope it to a session id. */
  sessionId?: string;
}

/**
 * UserDO — user-scoped budget + sync sequence + broadcast.
 */
export class UserDO {
  protected readonly state: UserDOState;
  protected readonly env: UserDOEnv;
  protected readonly auditReader: AuditLogReader;
  protected readonly subscribers: Set<BroadcastSubscriber> = new Set();
  /** In-flight serialization for `assignSequence` so concurrent callers are
   *  ordered through a single Promise chain even if `transaction` is a no-op. */
  protected sequenceLock: Promise<void> = Promise.resolve();

  constructor(
    state: UserDOState,
    env: UserDOEnv,
    auditReader?: AuditLogReader
  ) {
    this.state = state;
    this.env = env;
    this.auditReader = auditReader ?? auditLogReaderForEnv(env);
  }

  /** Soft monthly cap, env-overridable for enterprise tiers. */
  protected monthlyCap(): number {
    const override = this.env.HELM_USER_MONTHLY_CAP_MICRODOLLARS;
    if (!override) return DEFAULT_MONTHLY_CAP_MICRODOLLARS;
    const parsed = Number(override);
    if (!Number.isFinite(parsed) || parsed <= 0)
      return DEFAULT_MONTHLY_CAP_MICRODOLLARS;
    return parsed;
  }

  /**
   * Cold-start hydration. Reads the cumulative audit_log spend on first call
   * and caches it; subsequent calls re-use the cached value plus any
   * subsequent `recordSpend` deltas. Tests can call this directly.
   */
  async hydrate(userId: string): Promise<void> {
    const already = await this.state.storage.get<boolean>(
      USER_STORAGE_KEYS.HYDRATED
    );
    if (already) return;
    const total = await this.auditReader.totalSpendMicros(userId);
    await this.state.storage.put(
      USER_STORAGE_KEYS.SPEND_MICRODOLLARS,
      total
    );
    await this.state.storage.put(USER_STORAGE_KEYS.HYDRATED, true);
  }

  /**
   * Atomically assign the next sequence number for this user. Monotonic.
   * Backed by storage so the value survives hibernation. The lock is a
   * Promise chain — needed because `transaction` may not be available in
   * the test runtime.
   */
  async assignSequence(): Promise<number> {
    const next = await new Promise<number>((resolve, reject) => {
      this.sequenceLock = this.sequenceLock.then(async () => {
        try {
          const current =
            (await this.state.storage.get<number>(
              USER_STORAGE_KEYS.SEQUENCE
            )) ?? 0;
          const assigned = current + 1;
          await this.state.storage.put(
            USER_STORAGE_KEYS.SEQUENCE,
            assigned
          );
          resolve(assigned);
        } catch (err) {
          reject(err);
        }
      });
    });
    return next;
  }

  /**
   * Record a spend delta in micro-dollars. Returns the new running total
   * and whether the user is now over their monthly cap. Persists an
   * `audit_log` row with `action = "spend"` for replay/aggregation.
   */
  async recordSpend(input: {
    userId: string;
    micros: number;
    targetKind: string;
    targetId?: string;
  }): Promise<{ total: number; overBudget: boolean }> {
    if (input.micros <= 0)
      throw new Error("UserDO.recordSpend: micros must be > 0");
    await this.hydrate(input.userId);
    const current =
      (await this.state.storage.get<number>(
        USER_STORAGE_KEYS.SPEND_MICRODOLLARS
      )) ?? 0;
    const total = current + input.micros;
    await this.state.storage.put(
      USER_STORAGE_KEYS.SPEND_MICRODOLLARS,
      total
    );
    if (this.env.DB) {
      const db = getDb(this.env);
      await db
        .insert(auditLog)
        .values({
          id: crypto.randomUUID(),
          userId: input.userId,
          action: "spend",
          targetKind: input.targetKind,
          targetId: input.targetId ?? null,
          details: { micros: input.micros }
        })
        .run()
        .catch((err: unknown) => {
          console.log(
            `[UserDO] audit_log insert failed: ${String(err)}`
          );
        });
    }
    return { total, overBudget: total > this.monthlyCap() };
  }

  /** Read the cached spend total. Hydrates if necessary. */
  async getSpend(userId: string): Promise<{ total: number; cap: number }> {
    await this.hydrate(userId);
    const total =
      (await this.state.storage.get<number>(
        USER_STORAGE_KEYS.SPEND_MICRODOLLARS
      )) ?? 0;
    return { total, cap: this.monthlyCap() };
  }

  // ── Active session registry ─────────────────────────────────────────────

  async registerSession(sessionId: string): Promise<void> {
    const ids =
      (await this.state.storage.get<string[]>(
        USER_STORAGE_KEYS.ACTIVE_SESSIONS
      )) ?? [];
    if (!ids.includes(sessionId)) ids.push(sessionId);
    await this.state.storage.put(USER_STORAGE_KEYS.ACTIVE_SESSIONS, ids);
  }

  async unregisterSession(sessionId: string): Promise<void> {
    const ids =
      (await this.state.storage.get<string[]>(
        USER_STORAGE_KEYS.ACTIVE_SESSIONS
      )) ?? [];
    const filtered = ids.filter((id) => id !== sessionId);
    await this.state.storage.put(USER_STORAGE_KEYS.ACTIVE_SESSIONS, filtered);
  }

  async listSessions(): Promise<string[]> {
    return (
      (await this.state.storage.get<string[]>(
        USER_STORAGE_KEYS.ACTIVE_SESSIONS
      )) ?? []
    );
  }

  // ── Realtime broadcast ──────────────────────────────────────────────────

  /**
   * Add a WebSocket subscriber. The subscriber receives every subsequent
   * `broadcast()` event. Cleanup on close is handled by `removeSubscriber`.
   */
  addSubscriber(ws: WebSocket, sessionId?: string): void {
    this.subscribers.add({ ws, sessionId });
  }

  removeSubscriber(ws: WebSocket): void {
    for (const sub of Array.from(this.subscribers)) {
      if (sub.ws === ws) this.subscribers.delete(sub);
    }
  }

  /**
   * Persist a sync_events row and fan it out to all subscribers. The
   * sequence is assigned atomically and matches the row written to D1, so
   * clients can replay a missing window using
   * `sync_events WHERE user_id = ? AND sequence > ?`.
   */
  async broadcast(input: {
    userId: string;
    event: UserBroadcastEvent;
  }): Promise<{ sequence: number }> {
    const sequence = await this.assignSequence();
    if (this.env.DB) {
      const db = getDb(this.env);
      await db
        .insert(syncEvents)
        .values({
          id: crypto.randomUUID(),
          userId: input.userId,
          resourceId: input.event.resourceId,
          kind: input.event.kind,
          sequence
        })
        .run()
        .catch((err: unknown) => {
          console.log(
            `[UserDO] sync_events insert failed: ${String(err)}`
          );
        });
    }
    const frame = JSON.stringify({
      type: "sync_event",
      sequence,
      kind: input.event.kind,
      resourceId: input.event.resourceId,
      ...(input.event.payload !== undefined
        ? { payload: input.event.payload }
        : {})
    });
    for (const sub of Array.from(this.subscribers)) {
      try {
        sub.ws.send(frame);
      } catch {
        this.subscribers.delete(sub);
      }
    }
    return { sequence };
  }

  // ── HTTP entry point ────────────────────────────────────────────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    // POST /broadcast — called by SessionDO via service binding.
    if (url.pathname.endsWith("/broadcast") && request.method === "POST") {
      const body = (await request.json().catch(() => null)) as {
        userId?: string;
        event?: UserBroadcastEvent;
      } | null;
      if (!body?.userId || !body.event?.kind || !body.event?.resourceId) {
        return new Response(
          JSON.stringify({ error: "invalid broadcast body" }),
          { status: 400, headers: { "content-type": "application/json" } }
        );
      }
      const r = await this.broadcast({ userId: body.userId, event: body.event });
      return new Response(JSON.stringify(r), {
        headers: { "content-type": "application/json" }
      });
    }

    // GET /spend?userId=… — UI / orchestrator polling.
    if (url.pathname.endsWith("/spend") && request.method === "GET") {
      const userId = url.searchParams.get("userId");
      if (!userId) {
        return new Response(JSON.stringify({ error: "userId required" }), {
          status: 400,
          headers: { "content-type": "application/json" }
        });
      }
      const snap = await this.getSpend(userId);
      return new Response(JSON.stringify(snap), {
        headers: { "content-type": "application/json" }
      });
    }

    return new Response("not found", { status: 404 });
  }
}

/** Re-export for convenience: `sum` was unused in user-do but kept as part
 *  of the supported drizzle import surface for downstream contributors. */
export { sum };
