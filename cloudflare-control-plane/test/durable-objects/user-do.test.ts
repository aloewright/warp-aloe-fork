import { describe, expect, it } from "vitest";
import {
  UserDO,
  USER_STORAGE_KEYS,
  DEFAULT_MONTHLY_CAP_MICRODOLLARS,
  type AuditLogReader,
  type UserDOEnv
} from "../../src/workers/durable-objects/index.js";
import { makeInMemoryState } from "./in-memory-state.js";

function makeEnv(overrides: Partial<UserDOEnv> = {}): UserDOEnv {
  return { DB: undefined as unknown as D1Database, ...overrides };
}

const constantReader = (total: number): AuditLogReader => ({
  async totalSpendMicros() {
    return total;
  }
});

class FakeWS {
  readonly sent: string[] = [];
  send(d: string): void {
    this.sent.push(d);
  }
}

describe("UserDO sequence assignment", () => {
  it("assigns monotonic sequences serially", async () => {
    const user = new UserDO(makeInMemoryState(), makeEnv(), constantReader(0));
    const a = await user.assignSequence();
    const b = await user.assignSequence();
    const c = await user.assignSequence();
    expect([a, b, c]).toEqual([1, 2, 3]);
  });

  it("assigns monotonic sequences under concurrent fan-in", async () => {
    const user = new UserDO(makeInMemoryState(), makeEnv(), constantReader(0));
    const N = 100;
    const results = await Promise.all(
      Array.from({ length: N }, () => user.assignSequence())
    );
    // Sorted, the results must be 1..N exactly. The lock chain in
    // `assignSequence` is what guarantees this.
    const sorted = [...results].sort((a, b) => a - b);
    for (let i = 0; i < N; i++) {
      expect(sorted[i]).toBe(i + 1);
    }
    // Persisted high-water mark must equal N.
    const stored = await (
      user as unknown as { state: { storage: { get<T>(k: string): Promise<T | undefined> } } }
    ).state.storage.get<number>(USER_STORAGE_KEYS.SEQUENCE);
    expect(stored).toBe(N);
  });
});

describe("UserDO budget", () => {
  it("hydrates from audit_log on first read and caches the total", async () => {
    let calls = 0;
    const reader: AuditLogReader = {
      async totalSpendMicros() {
        calls++;
        return 1_234_567;
      }
    };
    const user = new UserDO(makeInMemoryState(), makeEnv(), reader);
    const first = await user.getSpend("u-1");
    const second = await user.getSpend("u-1");
    expect(first.total).toBe(1_234_567);
    expect(second.total).toBe(1_234_567);
    expect(calls).toBe(1);
    expect(first.cap).toBe(DEFAULT_MONTHLY_CAP_MICRODOLLARS);
  });

  it("recordSpend accumulates and flips overBudget at the cap", async () => {
    const user = new UserDO(
      makeInMemoryState(),
      makeEnv({ HELM_USER_MONTHLY_CAP_MICRODOLLARS: "1000" }),
      constantReader(0)
    );
    const r1 = await user.recordSpend({
      userId: "u-1",
      micros: 600,
      targetKind: "task"
    });
    expect(r1.overBudget).toBe(false);
    expect(r1.total).toBe(600);
    const r2 = await user.recordSpend({
      userId: "u-1",
      micros: 500,
      targetKind: "task"
    });
    expect(r2.total).toBe(1100);
    expect(r2.overBudget).toBe(true);
  });

  it("rejects non-positive spend deltas", async () => {
    const user = new UserDO(
      makeInMemoryState(),
      makeEnv(),
      constantReader(0)
    );
    await expect(
      user.recordSpend({ userId: "u-1", micros: 0, targetKind: "task" })
    ).rejects.toThrow();
    await expect(
      user.recordSpend({ userId: "u-1", micros: -5, targetKind: "task" })
    ).rejects.toThrow();
  });
});

describe("UserDO active session registry", () => {
  it("registers and unregisters sessions idempotently", async () => {
    const user = new UserDO(makeInMemoryState(), makeEnv(), constantReader(0));
    await user.registerSession("s1");
    await user.registerSession("s1"); // dedup
    await user.registerSession("s2");
    expect(await user.listSessions()).toEqual(["s1", "s2"]);
    await user.unregisterSession("s1");
    expect(await user.listSessions()).toEqual(["s2"]);
  });
});

describe("UserDO broadcast", () => {
  it("assigns a sequence and fans out the frame to subscribers", async () => {
    const user = new UserDO(makeInMemoryState(), makeEnv(), constantReader(0));
    const ws1 = new FakeWS();
    const ws2 = new FakeWS();
    user.addSubscriber(ws1 as unknown as WebSocket, "s-a");
    user.addSubscriber(ws2 as unknown as WebSocket, "s-b");
    const result = await user.broadcast({
      userId: "u-1",
      event: { kind: "created", resourceId: "r-1", payload: { id: "r-1" } }
    });
    expect(result.sequence).toBe(1);
    expect(ws1.sent.length).toBe(1);
    expect(ws2.sent.length).toBe(1);
    const parsed = JSON.parse(ws1.sent[0]!);
    expect(parsed).toMatchObject({
      type: "sync_event",
      sequence: 1,
      kind: "created",
      resourceId: "r-1"
    });
  });

  it("removeSubscriber stops further fan-out", async () => {
    const user = new UserDO(makeInMemoryState(), makeEnv(), constantReader(0));
    const ws = new FakeWS();
    user.addSubscriber(ws as unknown as WebSocket);
    user.removeSubscriber(ws as unknown as WebSocket);
    await user.broadcast({
      userId: "u-1",
      event: { kind: "updated", resourceId: "r-2" }
    });
    expect(ws.sent.length).toBe(0);
  });
});

describe("UserDO HTTP", () => {
  it("POST /broadcast returns the assigned sequence", async () => {
    const user = new UserDO(makeInMemoryState(), makeEnv(), constantReader(0));
    const res = await user.fetch(
      new Request("https://user/broadcast", {
        method: "POST",
        body: JSON.stringify({
          userId: "u-1",
          event: { kind: "created", resourceId: "r-1" }
        }),
        headers: { "content-type": "application/json" }
      })
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as { sequence: number };
    expect(body.sequence).toBe(1);
  });

  it("POST /broadcast 400s on missing fields", async () => {
    const user = new UserDO(makeInMemoryState(), makeEnv(), constantReader(0));
    const res = await user.fetch(
      new Request("https://user/broadcast", {
        method: "POST",
        body: JSON.stringify({}),
        headers: { "content-type": "application/json" }
      })
    );
    expect(res.status).toBe(400);
  });

  it("GET /spend returns cached total + cap", async () => {
    const user = new UserDO(
      makeInMemoryState(),
      makeEnv(),
      constantReader(42)
    );
    const res = await user.fetch(
      new Request("https://user/spend?userId=u-1")
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as { total: number; cap: number };
    expect(body.total).toBe(42);
    expect(body.cap).toBe(DEFAULT_MONTHLY_CAP_MICRODOLLARS);
  });
});
