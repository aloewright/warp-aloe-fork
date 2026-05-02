import { describe, expect, it } from "vitest";
import {
  SessionDO,
  IDLE_TIMEOUT_MS,
  SESSION_STORAGE_KEYS,
  buildContainerEnvFile,
  type SessionEnv,
  type TaskRunner
} from "../../src/workers/durable-objects/index.js";
import {
  PROTOCOL_VERSION_V1,
  parseEnvelope
} from "../../src/workers/durable-objects/protocol.js";
import { makeInMemoryState } from "./in-memory-state.js";

function makeEnv(overrides: Partial<SessionEnv> = {}): SessionEnv {
  return {
    // No DB by default — tests opt in by injecting a fake DB.
    DB: undefined as unknown as D1Database,
    ...overrides
  };
}

function buildSubmit(taskId: string, payload: unknown = {}): string {
  return JSON.stringify({
    protocol_version: PROTOCOL_VERSION_V1,
    version: "1",
    type: "task_submit",
    task_id: taskId,
    kind: "shell",
    payload
  });
}

class FakeWebSocket {
  readonly sent: string[] = [];
  readonly closed: Array<{ code?: number; reason?: string }> = [];
  send(data: string): void {
    this.sent.push(data);
  }
  close(code?: number, reason?: string): void {
    this.closed.push({ code, reason });
  }
}

describe("SessionDO task queue ordering", () => {
  it("processes tasks FIFO and emits status_changed -> output -> completed per task", async () => {
    const state = makeInMemoryState();
    const env = makeEnv();
    const calls: string[] = [];
    const runner: TaskRunner = async ({ taskId, emit }) => {
      calls.push(taskId);
      emit({ stream: "stdout", data: `hello-${taskId}` });
      return { ok: true, output: { taskId } };
    };
    const session = new SessionDO(state, env, runner);
    const ws = new FakeWebSocket() as unknown as WebSocket;
    session["sockets"].add(ws);

    await session.webSocketMessage(ws, buildSubmit("t-1"));
    await session.webSocketMessage(ws, buildSubmit("t-2"));
    // The webSocketMessage call kicks off `drain()` via void; await it
    // explicitly here so the test isn't racy.
    await session.drain();

    expect(calls).toEqual(["t-1", "t-2"]);
    const fake = ws as unknown as FakeWebSocket;
    const events = fake.sent
      .map((s) => parseEnvelope(s))
      .filter((p) => p && p.message.type === "task_event");
    // Per task we expect: status:queued, status:running, output, completed.
    const t1 = events.filter(
      (e) =>
        e &&
        e.message.type === "task_event" &&
        e.message.task_id === "t-1"
    );
    expect(t1.length).toBeGreaterThanOrEqual(4);

    // Storage drained.
    const queue = (await state.storage.get<unknown[]>(
      SESSION_STORAGE_KEYS.QUEUE
    )) ?? [];
    expect(queue).toEqual([]);
    expect(await state.storage.get(SESSION_STORAGE_KEYS.RUNNING)).toBeUndefined();
  });

  it("emits a protocol error for malformed envelopes without crashing", async () => {
    const state = makeInMemoryState();
    const session = new SessionDO(
      state,
      makeEnv(),
      async () => ({ ok: true })
    );
    const ws = new FakeWebSocket() as unknown as WebSocket;
    session["sockets"].add(ws);
    await session.webSocketMessage(ws, "not-json");
    const fake = ws as unknown as FakeWebSocket;
    expect(fake.sent.length).toBe(1);
    const decoded = parseEnvelope(fake.sent[0]!);
    expect(decoded?.message.type).toBe("task_event");
  });

  it("cancels a running task when control:cancel arrives", async () => {
    const state = makeInMemoryState();
    let resolveRun!: () => void;
    const runner: TaskRunner = () =>
      new Promise<{ ok: true }>((resolve) => {
        resolveRun = () => resolve({ ok: true });
      });
    const session = new SessionDO(state, makeEnv(), runner);
    const ws = new FakeWebSocket() as unknown as WebSocket;
    session["sockets"].add(ws);

    await session.webSocketMessage(ws, buildSubmit("t-cancel"));
    const drainPromise = session.drain();
    // Give the runner a tick to mark RUNNING.
    await new Promise((r) => setTimeout(r, 10));
    await session.cancelTask("t-cancel");
    resolveRun();
    await drainPromise;

    const fake = ws as unknown as FakeWebSocket;
    const completed = fake.sent
      .map((s) => parseEnvelope(s))
      .filter(
        (p) =>
          p?.message.type === "task_event" &&
          p.message.kind &&
          (p.message.kind as { event?: string }).event === "completed"
      );
    expect(completed.length).toBeGreaterThan(0);
  });

  it("schedules an alarm IDLE_TIMEOUT_MS in the future on activity", async () => {
    const state = makeInMemoryState();
    const session = new SessionDO(
      state,
      makeEnv(),
      async () => ({ ok: true })
    );
    const before = Date.now();
    await session.enqueueTask("t-alarm", "shell", {});
    const alarm = await state.storage.getAlarm();
    expect(alarm).not.toBeNull();
    expect((alarm ?? 0) - before).toBeGreaterThanOrEqual(IDLE_TIMEOUT_MS - 1_000);
    expect((alarm ?? 0) - before).toBeLessThanOrEqual(IDLE_TIMEOUT_MS + 1_000);
  });

  it("alarm() closes connected sockets when idle window has elapsed", async () => {
    const state = makeInMemoryState();
    const session = new SessionDO(
      state,
      makeEnv(),
      async () => ({ ok: true })
    );
    const ws = new FakeWebSocket() as unknown as WebSocket;
    session["sockets"].add(ws);
    // Pretend the last activity happened far in the past.
    await state.storage.put(
      SESSION_STORAGE_KEYS.LAST_ACTIVITY,
      Date.now() - IDLE_TIMEOUT_MS - 60_000
    );
    await session.alarm();
    expect((ws as unknown as FakeWebSocket).closed.length).toBe(1);
  });

  it("alarm() re-arms instead of tearing down when activity is recent", async () => {
    const state = makeInMemoryState();
    const session = new SessionDO(
      state,
      makeEnv(),
      async () => ({ ok: true })
    );
    const ws = new FakeWebSocket() as unknown as WebSocket;
    session["sockets"].add(ws);
    await state.storage.put(SESSION_STORAGE_KEYS.LAST_ACTIVITY, Date.now());
    await session.alarm();
    expect((ws as unknown as FakeWebSocket).closed.length).toBe(0);
    expect(await state.storage.getAlarm()).not.toBeNull();
  });

  it("buildContainerEnvFile emits only the configured R2 keys", () => {
    expect(buildContainerEnvFile({} as SessionEnv)).toBe("");
    const lines = buildContainerEnvFile({
      HELM_R2_ACCESS_KEY_ID: "AKIA",
      HELM_R2_SECRET_ACCESS_KEY: "SECRET",
      HELM_R2_BUCKET: "helm-checkpoints",
      HELM_R2_ENDPOINT: "https://example.r2.cloudflarestorage.com"
    } as SessionEnv);
    expect(lines).toContain("HELM_R2_ACCESS_KEY_ID=AKIA");
    expect(lines).toContain("HELM_R2_BUCKET=helm-checkpoints");
    expect(lines.endsWith("\n")).toBe(true);
  });

  it("REST POST /tasks enqueues without a WebSocket", async () => {
    const state = makeInMemoryState();
    const session = new SessionDO(
      state,
      makeEnv(),
      async () => ({ ok: true })
    );
    const res = await session.fetch(
      new Request("https://session/tasks", {
        method: "POST",
        body: JSON.stringify({ taskId: "rest-1", kind: "shell", payload: {} }),
        headers: { "content-type": "application/json" }
      })
    );
    expect(res.status).toBe(202);
    const queue = (await state.storage.get<unknown[]>(
      SESSION_STORAGE_KEYS.QUEUE
    )) ?? [];
    expect(queue.length).toBe(1);
  });
});
