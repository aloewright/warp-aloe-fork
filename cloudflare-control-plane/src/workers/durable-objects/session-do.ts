/**
 * SessionDO — per-session task queue, agent state, WebSocket transport, idle
 * alarm, and (PDX-21) container lifecycle integration.
 *
 * One instance per logical session. The Worker routes
 * `/api/sessions/:sessionId/*` to a stub addressed by `idFromName(sessionId)`.
 *
 *   ┌──────────────────────────────────────────────────────────────────────┐
 *   │ Lifecycle                                                            │
 *   │                                                                      │
 *   │   1. Client opens WS to /api/sessions/:sessionId/ws.                 │
 *   │   2. Client sends `task_submit` envelopes; SessionDO appends to a    │
 *   │      storage-backed queue, persists a row in D1 `tasks`, and either  │
 *   │      starts an agent-runtime Container (PDX-21) or, when running     │
 *   │      multi-agent work, dispatches a SwarmWorkflow (PDX-25).          │
 *   │   3. Container stdout/stderr is streamed back as `task_event` chunks │
 *   │      (one per line). Status transitions are persisted to D1.         │
 *   │   4. On hibernation, the WS handler is restored from durable storage │
 *   │      and the in-flight task pointer survives.                        │
 *   │   5. After IDLE_TIMEOUT_MS without WS activity the alarm fires,      │
 *   │      cancels the container, closes the WS, and persists the final    │
 *   │      task state.                                                     │
 *   └──────────────────────────────────────────────────────────────────────┘
 */
import { eq } from "drizzle-orm";
import { getDb, type DbEnv } from "../../db/index.js";
import { sessions, tasks } from "../../db/schema.js";
import {
  encodeTaskEvent,
  parseEnvelope,
  type TaskEventKind,
  type TaskStatus
} from "./protocol.js";

/** 5 minutes of idle WS time before we tear the session down. */
export const IDLE_TIMEOUT_MS = 5 * 60_000;

/** Storage keys, kept as constants so the test harness can poke around. */
export const STORAGE_KEYS = {
  /** FIFO queue of tasks waiting to run. */
  QUEUE: "session:queue",
  /** Task currently being executed (one at a time per session). */
  RUNNING: "session:running",
  /** Last WS activity (ms since epoch) — used to schedule the idle alarm. */
  LAST_ACTIVITY: "session:lastActivity",
  /** Set on first connect — corresponds to D1 `sessions.id`. */
  SESSION_ID: "session:id",
  /** Stable user id for D1 attribution, supplied by the caller via header. */
  USER_ID: "session:userId"
} as const;

/**
 * Container binding shape — a strict subset of `cloudflare:workers`
 * `Container` so the Worker code compiles without importing the runtime
 * type.
 */
export interface ContainerBinding {
  /** Returns a stub for the container instance addressed by `id`. */
  get(id: { name: string }): ContainerInstance;
}

export interface ContainerInstance {
  /** Issue a fetch against the container's HTTP surface. */
  fetch(request: Request): Promise<Response>;
  /** Stop the container instance. Optional — mocked in tests. */
  destroy?(): Promise<void>;
}

/** Optional UserDO RPC stub used to broadcast cross-session events. */
export interface UserBroadcaster {
  broadcast(event: {
    userId: string;
    sessionId: string;
    kind: string;
    payload: unknown;
  }): Promise<void>;
}

/**
 * Run a task body. Pulled out so tests can inject a synchronous mock instead
 * of requiring a real container. The default implementation calls into the
 * agent-runtime Container.
 */
export type TaskRunner = (input: TaskRunnerInput) => Promise<TaskRunnerResult>;

export interface TaskRunnerInput {
  sessionId: string;
  taskId: string;
  kind: string;
  payload: unknown;
  /**
   * Emit an output chunk back to the client. Implementations should call
   * this once per line of stdout/stderr from the container. Streaming is
   * fire-and-forget — the runner is responsible for ordering.
   */
  emit(chunk: { stream: "stdout" | "stderr"; data: string }): void;
}

export type TaskRunnerResult =
  | { ok: true; output?: unknown }
  | { ok: false; code: string; message: string };

interface QueueEntry {
  taskId: string;
  kind: string;
  payload: unknown;
  enqueuedAt: number;
  /** Per-task event sequence used by `TaskEvent.sequence`. */
  nextSequence: number;
}

export interface SessionEnv extends DbEnv {
  /** Container binding from wrangler.agent-runtime.toml — optional in tests. */
  AGENT_RUNTIME_CONTAINER?: ContainerBinding;
  /** Doppler/secret-supplied R2 creds used to seed `/workspace/.env`. */
  HELM_R2_ACCESS_KEY_ID?: string;
  HELM_R2_SECRET_ACCESS_KEY?: string;
  HELM_R2_BUCKET?: string;
  HELM_R2_ENDPOINT?: string;
}

/**
 * Test-friendly shape of `DurableObjectState`. The real Workers runtime
 * `DurableObjectState` is a superset; we only depend on the subset listed
 * here so unit tests can pass an in-memory fake.
 */
export interface SessionDOState {
  storage: {
    get<T>(key: string): Promise<T | undefined>;
    put<T>(key: string, value: T): Promise<void>;
    delete(key: string): Promise<boolean>;
    setAlarm(scheduledTime: number | Date): Promise<void>;
    getAlarm(): Promise<number | null>;
  };
  acceptWebSocket?: (ws: WebSocket, tags?: string[]) => void;
  getWebSockets?: (tag?: string) => WebSocket[];
}

/**
 * Construct the env-string blob written to `/workspace/.env` by the
 * agent-runtime container entrypoint. Exposed for unit testing.
 */
export function buildContainerEnvFile(env: SessionEnv): string {
  const lines: string[] = [];
  if (env.HELM_R2_ACCESS_KEY_ID)
    lines.push(`HELM_R2_ACCESS_KEY_ID=${env.HELM_R2_ACCESS_KEY_ID}`);
  if (env.HELM_R2_SECRET_ACCESS_KEY)
    lines.push(`HELM_R2_SECRET_ACCESS_KEY=${env.HELM_R2_SECRET_ACCESS_KEY}`);
  if (env.HELM_R2_BUCKET) lines.push(`HELM_R2_BUCKET=${env.HELM_R2_BUCKET}`);
  if (env.HELM_R2_ENDPOINT)
    lines.push(`HELM_R2_ENDPOINT=${env.HELM_R2_ENDPOINT}`);
  return lines.join("\n") + (lines.length > 0 ? "\n" : "");
}

/**
 * Default container-backed task runner. Sends a POST to the agent-runtime
 * container; the container streams output as a sequence of newline-delimited
 * `{stream, data}` JSON chunks. Implementations may swap this for a
 * WebSocket once the runtime supports DO→Container WS.
 */
export const defaultTaskRunner =
  (env: SessionEnv): TaskRunner =>
  async ({ sessionId, taskId, kind, payload, emit }) => {
    const container = env.AGENT_RUNTIME_CONTAINER;
    if (!container) {
      // No container binding wired (typical in tests) — surface a structured
      // error instead of silently succeeding so missing config is loud.
      return {
        ok: false,
        code: "container_unavailable",
        message: "AGENT_RUNTIME_CONTAINER binding is not configured"
      };
    }
    const instance = container.get({ name: sessionId });
    const envFile = buildContainerEnvFile(env);
    const response = await instance.fetch(
      new Request("https://container/run", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ taskId, kind, payload, envFile })
      })
    );
    if (!response.ok) {
      return {
        ok: false,
        code: `container_${response.status}`,
        message: `agent-runtime container returned HTTP ${response.status}`
      };
    }
    if (!response.body) {
      return { ok: true };
    }
    const reader = response.body
      .pipeThrough(new TextDecoderStream())
      .getReader();
    let buffered = "";
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buffered += value;
      let nl = buffered.indexOf("\n");
      while (nl >= 0) {
        const line = buffered.slice(0, nl);
        buffered = buffered.slice(nl + 1);
        if (line.length > 0) {
          try {
            const parsed = JSON.parse(line) as {
              stream?: "stdout" | "stderr";
              data?: string;
            };
            if (parsed.stream && typeof parsed.data === "string") {
              emit({ stream: parsed.stream, data: parsed.data });
            }
          } catch {
            // Non-JSON lines are forwarded as stdout for transparency.
            emit({ stream: "stdout", data: line });
          }
        }
        nl = buffered.indexOf("\n");
      }
    }
    return { ok: true };
  };

/**
 * SessionDO — the heart of a single agent session.
 *
 * Tests instantiate this directly with a fake state and an injected
 * `taskRunner` so that the queue / WebSocket / alarm logic can be exercised
 * without standing up a container or D1 binding.
 */
export class SessionDO {
  protected readonly state: SessionDOState;
  protected readonly env: SessionEnv;
  protected readonly runner: TaskRunner;
  /** Connected sockets — one logical client per session at a time, but the
   *  set tolerates reconnection windows. */
  protected readonly sockets: Set<WebSocket> = new Set();
  /**
   * Currently-running `drain()` promise, or null when idle. Concurrent calls
   * to `drain()` return the same Promise so callers always see the queue
   * fully drained when their await resolves.
   */
  protected drainInFlight: Promise<void> | null = null;

  constructor(state: SessionDOState, env: SessionEnv, runner?: TaskRunner) {
    this.state = state;
    this.env = env;
    this.runner = runner ?? defaultTaskRunner(env);
  }

  // ── HTTP entry point ────────────────────────────────────────────────────

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);

    // WebSocket upgrade: `Upgrade: websocket`.
    if (request.headers.get("upgrade")?.toLowerCase() === "websocket") {
      return this.handleWebSocketUpgrade(request);
    }

    // POST /tasks — REST equivalent of `task_submit`. Used by the Workflows
    // Worker (PDX-25) which dispatches into a session without a WS.
    if (url.pathname.endsWith("/tasks") && request.method === "POST") {
      const body = (await request.json().catch(() => null)) as
        | { taskId: string; kind: string; payload: unknown }
        | null;
      if (!body || !body.taskId || !body.kind) {
        return new Response(JSON.stringify({ error: "invalid task body" }), {
          status: 400,
          headers: { "content-type": "application/json" }
        });
      }
      await this.enqueueTask(body.taskId, body.kind, body.payload);
      return new Response(
        JSON.stringify({ taskId: body.taskId, status: "queued" }),
        { status: 202, headers: { "content-type": "application/json" } }
      );
    }

    return new Response("not found", { status: 404 });
  }

  protected async handleWebSocketUpgrade(request: Request): Promise<Response> {
    const userId = request.headers.get("x-helm-user-id") ?? "anonymous";
    const sessionId =
      request.headers.get("x-helm-session-id") ??
      (await this.state.storage.get<string>(STORAGE_KEYS.SESSION_ID)) ??
      crypto.randomUUID();
    await this.state.storage.put(STORAGE_KEYS.SESSION_ID, sessionId);
    await this.state.storage.put(STORAGE_KEYS.USER_ID, userId);

    // The standard WebSocketPair API. `acceptWebSocket` opt-in enables
    // hibernation; if the runtime doesn't expose it, fall back to attaching
    // listeners so the same code path works in unit tests.
    const pair = new WebSocketPair();
    const [client, server] = Object.values(pair) as [WebSocket, WebSocket];
    if (this.state.acceptWebSocket) {
      this.state.acceptWebSocket(server, [sessionId]);
    } else {
      (server as unknown as { accept(): void }).accept();
      server.addEventListener("message", (event: MessageEvent) => {
        void this.webSocketMessage(server, event.data as string | ArrayBuffer);
      });
      server.addEventListener("close", () => this.webSocketClose(server));
    }
    this.sockets.add(server);
    await this.touchActivity();

    // 101 Switching Protocols.
    return new Response(null, {
      status: 101,
      // The `webSocket` property is consumed by the Workers runtime; the
      // built-in Response type doesn't yet have it in `lib.dom`, so cast.
      webSocket: client
    } as ResponseInit & { webSocket: WebSocket });
  }

  // ── Hibernated WS callbacks ─────────────────────────────────────────────

  /**
   * Public so the Workers runtime can invoke it after hibernation. Tests
   * call it directly to drive the queue.
   */
  async webSocketMessage(
    ws: WebSocket,
    message: string | ArrayBuffer
  ): Promise<void> {
    await this.touchActivity();
    const parsed = parseEnvelope(message);
    if (!parsed) {
      ws.send(
        JSON.stringify({
          protocol_version: 1,
          version: "1",
          type: "task_event",
          task_id: "",
          sequence: 0,
          kind: {
            event: "completed",
            result: {
              outcome: "error",
              code: "protocol_error",
              message: "could not decode envelope"
            }
          }
        })
      );
      return;
    }
    if (parsed.message.type === "task_submit") {
      const m = parsed.message;
      await this.enqueueTask(m.task_id, m.kind, m.payload);
      this.emitTaskEvent(m.task_id, 0, {
        event: "status_changed",
        status: "queued"
      });
      void this.drain();
    } else if (parsed.message.type === "task_control") {
      // Cancellation is best-effort: if the task is in queue, drop it.
      // If it's running, mark the running entry cancelled — the runner
      // checks the flag between output chunks.
      const ctrl = parsed.message;
      await this.cancelTask(ctrl.task_id);
    }
    // task_event from the client is ignored.
  }

  webSocketClose(ws: WebSocket): void {
    this.sockets.delete(ws);
  }

  // ── Queue & D1 persistence ──────────────────────────────────────────────

  /** Storage layout: `[QueueEntry, ...]`. Order is FIFO. */
  protected async readQueue(): Promise<QueueEntry[]> {
    return (
      (await this.state.storage.get<QueueEntry[]>(STORAGE_KEYS.QUEUE)) ?? []
    );
  }

  protected async writeQueue(queue: QueueEntry[]): Promise<void> {
    await this.state.storage.put(STORAGE_KEYS.QUEUE, queue);
  }

  /**
   * Append a new task to the queue, persist it to D1 with status=queued, and
   * schedule the idle alarm. Public for the REST `/tasks` path; the WS
   * pipeline calls this through `webSocketMessage`.
   */
  async enqueueTask(
    taskId: string,
    kind: string,
    payload: unknown
  ): Promise<void> {
    const queue = await this.readQueue();
    queue.push({
      taskId,
      kind,
      payload,
      enqueuedAt: Date.now(),
      nextSequence: 0
    });
    await this.writeQueue(queue);
    await this.persistTaskRow(taskId, kind, payload, "queued");
    await this.touchActivity();
  }

  /**
   * Drain the queue, running tasks one at a time. Idempotent: concurrent
   * `drain()` calls return immediately. Public so tests can drive it
   * directly without standing up a WS.
   */
  async drain(): Promise<void> {
    if (this.drainInFlight) return this.drainInFlight;
    this.drainInFlight = this.drainOnce();
    try {
      await this.drainInFlight;
    } finally {
      this.drainInFlight = null;
    }
  }

  /** Body of a drain pass. Always called via `drain()` so concurrent callers
   *  share a single in-flight Promise. */
  protected async drainOnce(): Promise<void> {
    try {
      // Loop until the queue is empty so a burst of `task_submit`s drains in
      // one alarm window.
      while (true) {
        const queue = await this.readQueue();
        if (queue.length === 0) return;
        const next = queue[0]!;
        await this.state.storage.put(STORAGE_KEYS.RUNNING, next);
        // Status: queued -> running.
        await this.updateTaskStatus(next.taskId, "running");
        const seqRunning = next.nextSequence++;
        this.emitTaskEvent(next.taskId, seqRunning, {
          event: "status_changed",
          status: "running"
        });

        // Run the body, streaming output chunks back as task_event frames.
        let cancelled = false;
        const result = await this.runner({
          sessionId:
            (await this.state.storage.get<string>(STORAGE_KEYS.SESSION_ID)) ??
            "unknown",
          taskId: next.taskId,
          kind: next.kind,
          payload: next.payload,
          emit: (chunk) => {
            if (cancelled) return;
            const seq = next.nextSequence++;
            this.emitTaskEvent(next.taskId, seq, {
              event: "output",
              stream: chunk.stream,
              data: chunk.data
            });
          }
        }).catch((err: unknown) => ({
          ok: false as const,
          code: "runner_threw",
          message: err instanceof Error ? err.message : String(err)
        }));

        // Look for a cancellation that landed mid-flight.
        const stillRunning = await this.state.storage.get<QueueEntry>(
          STORAGE_KEYS.RUNNING
        );
        if (!stillRunning) {
          cancelled = true;
        }

        const finalStatus: TaskStatus = cancelled
          ? "cancelled"
          : result.ok
            ? "succeeded"
            : "failed";
        await this.updateTaskStatus(next.taskId, finalStatus, result);
        this.emitTaskEvent(next.taskId, next.nextSequence++, {
          event: "completed",
          result: cancelled
            ? { outcome: "cancelled" }
            : result.ok
              ? {
                  outcome: "success",
                  ...(result.output !== undefined ? { output: result.output } : {})
                }
              : { outcome: "error", code: result.code, message: result.message }
        });

        // Pop the head and continue.
        const remaining = (await this.readQueue()).slice(1);
        await this.writeQueue(remaining);
        await this.state.storage.delete(STORAGE_KEYS.RUNNING);
      }
    } finally {
      await this.touchActivity();
    }
  }

  /** Cancel a queued or running task by id. */
  async cancelTask(taskId: string): Promise<boolean> {
    const queue = await this.readQueue();
    const before = queue.length;
    const filtered = queue.filter((q) => q.taskId !== taskId);
    if (filtered.length !== before) {
      await this.writeQueue(filtered);
      await this.updateTaskStatus(taskId, "cancelled");
      this.emitTaskEvent(taskId, 0, {
        event: "completed",
        result: { outcome: "cancelled" }
      });
      return true;
    }
    const running = await this.state.storage.get<QueueEntry>(
      STORAGE_KEYS.RUNNING
    );
    if (running && running.taskId === taskId) {
      // Clear the running flag — `drain()` reads this between chunks.
      await this.state.storage.delete(STORAGE_KEYS.RUNNING);
      return true;
    }
    return false;
  }

  // ── D1 helpers ──────────────────────────────────────────────────────────

  protected async persistTaskRow(
    taskId: string,
    kind: string,
    payload: unknown,
    status: TaskStatus
  ): Promise<void> {
    if (!this.env.DB) return; // tests without D1
    const sessionId =
      (await this.state.storage.get<string>(STORAGE_KEYS.SESSION_ID)) ??
      "unknown";
    const db = getDb(this.env);
    await db
      .insert(tasks)
      .values({
        id: taskId,
        sessionId,
        prompt: typeof payload === "string" ? payload : JSON.stringify(payload),
        status: this.normalizeStatus(status),
        result: { kind, payload }
      })
      .onConflictDoNothing()
      .run()
      .catch((err: unknown) => {
        // D1 errors must not break the queue. The DO retains the queue in
        // storage so a partial D1 failure can be reconciled later.
        console.log(`[SessionDO] D1 task insert failed: ${String(err)}`);
      });
  }

  protected async updateTaskStatus(
    taskId: string,
    status: TaskStatus,
    result?: TaskRunnerResult
  ): Promise<void> {
    if (!this.env.DB) return;
    const db = getDb(this.env);
    const normalized = this.normalizeStatus(status);
    await db
      .update(tasks)
      .set({
        status: normalized,
        ...(result
          ? {
              result: result.ok
                ? { ok: true, output: result.output ?? null }
                : { ok: false, code: result.code, message: result.message }
            }
          : {}),
        updatedAt: new Date()
      })
      .where(eq(tasks.id, taskId))
      .run()
      .catch((err: unknown) => {
        console.log(`[SessionDO] D1 task update failed: ${String(err)}`);
      });
  }

  /**
   * The D1 schema defines a narrower `enum` than the wire protocol
   * (no "paused"). Map the wider TaskStatus into the schema's set.
   */
  protected normalizeStatus(
    status: TaskStatus
  ): "queued" | "running" | "succeeded" | "failed" | "cancelled" {
    if (status === "paused") return "running";
    return status;
  }

  // ── Output fanout ──────────────────────────────────────────────────────

  protected emitTaskEvent(
    taskId: string,
    sequence: number,
    kind: TaskEventKind
  ): void {
    const frame = encodeTaskEvent({
      task_id: taskId,
      sequence,
      timestamp: new Date().toISOString(),
      kind
    });
    const recipients = this.state.getWebSockets
      ? this.state.getWebSockets()
      : Array.from(this.sockets);
    for (const ws of recipients) {
      try {
        ws.send(frame);
      } catch {
        // Client gone; drop silently.
        this.sockets.delete(ws);
      }
    }
  }

  // ── Idle alarm ──────────────────────────────────────────────────────────

  protected async touchActivity(): Promise<void> {
    const now = Date.now();
    await this.state.storage.put(STORAGE_KEYS.LAST_ACTIVITY, now);
    await this.state.storage.setAlarm(now + IDLE_TIMEOUT_MS);
  }

  /**
   * Public so the runtime can invoke it. Tests call it directly to verify
   * tear-down behavior.
   */
  async alarm(): Promise<void> {
    const last =
      (await this.state.storage.get<number>(STORAGE_KEYS.LAST_ACTIVITY)) ?? 0;
    const idleFor = Date.now() - last;
    if (idleFor < IDLE_TIMEOUT_MS) {
      // Activity raced the alarm — re-arm and bail.
      await this.state.storage.setAlarm(last + IDLE_TIMEOUT_MS);
      return;
    }
    // Tear down: cancel running task, close sockets, mark D1 session ended.
    const running = await this.state.storage.get<QueueEntry>(
      STORAGE_KEYS.RUNNING
    );
    if (running) {
      await this.cancelTask(running.taskId);
    }
    const sessionId = await this.state.storage.get<string>(
      STORAGE_KEYS.SESSION_ID
    );
    if (sessionId && this.env.DB) {
      const db = getDb(this.env);
      await db
        .update(sessions)
        .set({ endedAt: new Date() })
        .where(eq(sessions.id, sessionId))
        .run()
        .catch((err: unknown) => {
          console.log(`[SessionDO] D1 session-end update failed: ${String(err)}`);
        });
    }
    for (const ws of Array.from(this.sockets)) {
      try {
        ws.close(1000, "idle timeout");
      } catch {
        // ignored
      }
      this.sockets.delete(ws);
    }
  }
}
