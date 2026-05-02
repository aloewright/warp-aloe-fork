import { describe, expect, it } from "vitest";
import {
  PROTOCOL_VERSION_V1,
  encodeTaskEvent,
  parseEnvelope
} from "../../src/workers/durable-objects/protocol.js";

describe("parseEnvelope", () => {
  it("accepts a well-formed task_submit", () => {
    const raw = JSON.stringify({
      protocol_version: PROTOCOL_VERSION_V1,
      version: "1",
      type: "task_submit",
      task_id: "t-1",
      kind: "shell",
      payload: { cmd: "ls" }
    });
    const decoded = parseEnvelope(raw);
    expect(decoded?.message.type).toBe("task_submit");
    if (decoded?.message.type === "task_submit") {
      expect(decoded.message.task_id).toBe("t-1");
      expect(decoded.message.kind).toBe("shell");
    }
  });

  it("ignores unknown fields (forward compatibility)", () => {
    const raw = JSON.stringify({
      protocol_version: PROTOCOL_VERSION_V1,
      version: "1",
      type: "task_submit",
      task_id: "t",
      kind: "shell",
      payload: {},
      future_field: 42
    });
    expect(parseEnvelope(raw)?.message.type).toBe("task_submit");
  });

  it("rejects unsupported protocol versions", () => {
    const raw = JSON.stringify({
      protocol_version: 9999,
      version: "9999",
      type: "task_submit"
    });
    expect(parseEnvelope(raw)).toBeNull();
  });

  it("rejects non-JSON inputs", () => {
    expect(parseEnvelope("not json")).toBeNull();
  });

  it("decodes task_control variants", () => {
    const cancel = JSON.stringify({
      protocol_version: 1,
      version: "1",
      type: "task_control",
      control: "cancel",
      task_id: "t-1"
    });
    const decoded = parseEnvelope(cancel);
    expect(decoded?.message.type).toBe("task_control");
  });
});

describe("encodeTaskEvent", () => {
  it("produces a wire-shape compatible envelope", () => {
    const wire = encodeTaskEvent({
      task_id: "t",
      sequence: 0,
      kind: { event: "status_changed", status: "running" }
    });
    const value = JSON.parse(wire) as Record<string, unknown>;
    expect(value.protocol_version).toBe(1);
    expect(value.version).toBe("1");
    expect(value.type).toBe("task_event");
    expect(value.task_id).toBe("t");
    expect(value.kind).toEqual({ event: "status_changed", status: "running" });
  });
});
