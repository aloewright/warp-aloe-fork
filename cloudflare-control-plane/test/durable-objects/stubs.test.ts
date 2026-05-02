import { describe, expect, it } from "vitest";
import {
  SwarmDO,
  RepoDO
} from "../../src/workers/durable-objects/index.js";
import { makeInMemoryState } from "./in-memory-state.js";

describe("SwarmDO stub", () => {
  it("returns 501 with a TODO marker on every call", async () => {
    const state = makeInMemoryState();
    const swarm = new SwarmDO(state);
    const res = await swarm.fetch(new Request("https://swarm/anything"));
    expect(res.status).toBe(501);
    const body = (await res.json()) as { error: string; message: string };
    expect(body.error).toBe("not_implemented");
    expect(body.message).toMatch(/SwarmDO/);
  });
});

describe("RepoDO stub", () => {
  it("returns 501 with a TODO marker on every call", async () => {
    const state = makeInMemoryState();
    const repo = new RepoDO(state);
    const res = await repo.fetch(new Request("https://repo/anything"));
    expect(res.status).toBe(501);
    const body = (await res.json()) as { error: string; message: string };
    expect(body.error).toBe("not_implemented");
    expect(body.message).toMatch(/RepoDO/);
  });
});
