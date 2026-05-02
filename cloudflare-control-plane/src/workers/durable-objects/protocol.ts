/**
 * TypeScript mirror of `crates/cloud_protocol` (PDX-17) — just the V1
 * envelope shapes that the SessionDO needs to encode/decode WebSocket frames.
 *
 * This is intentionally hand-rolled rather than generated. The Rust crate is
 * the source of truth; whenever it changes the corresponding fields here must
 * be updated. The V1 forward-compat rule (decoders ignore unknown fields)
 * means the schema can grow optional fields without bumping
 * `PROTOCOL_VERSION_V1`.
 */

export const PROTOCOL_VERSION_V1 = 1;

export type TaskStatus =
  | "queued"
  | "running"
  | "paused"
  | "succeeded"
  | "failed"
  | "cancelled";

export type OutputStream = "stdout" | "stderr" | "log" | "agent";

export interface TaskSubmit {
  task_id: string;
  kind: string;
  payload: unknown;
  metadata?: Record<string, string>;
}

export type TaskResult =
  | { outcome: "success"; output?: unknown }
  | { outcome: "error"; code: string; message: string; details?: unknown }
  | { outcome: "cancelled" };

export type TaskEventKind =
  | { event: "status_changed"; status: TaskStatus }
  | { event: "output"; stream: OutputStream; data: string }
  | { event: "progress"; fraction: number; label?: string }
  | { event: "completed"; result: TaskResult };

export interface TaskEvent {
  task_id: string;
  sequence: number;
  timestamp?: string;
  kind: TaskEventKind;
}

export type TaskControl =
  | { control: "cancel"; task_id: string }
  | { control: "pause"; task_id: string }
  | { control: "resume"; task_id: string }
  | {
      control: "signal";
      task_id: string;
      name: string;
      payload?: unknown;
    };

export type V1Message =
  | ({ type: "task_submit" } & TaskSubmit)
  | ({ type: "task_event" } & TaskEvent)
  | ({ type: "task_control" } & TaskControl);

export interface Envelope {
  protocol_version: number;
  version: "1";
  // Flattened V1Message via `serde(flatten)` on the Rust side.
  // We model that as a union here.
  // The runtime helpers below are the only blessed path for building one.
  // This `unknown` is narrowed by `parseEnvelope`.
  // (TypeScript can't express `&` over a discriminated union plus a header
  // without making consumers write boilerplate, so the Envelope type stays
  // permissive and `parseEnvelope` returns the narrowed shape.)
  [key: string]: unknown;
}

export interface ParsedEnvelope {
  protocol_version: number;
  message: V1Message;
}

/**
 * Parse a raw WebSocket frame as JSON and validate it against the V1
 * envelope shape. Returns `null` for invalid JSON, an unknown protocol
 * version, or an unknown V1 message type.
 *
 * Mirrors `cloud_protocol::parse_message`. Forward compatibility: extra
 * fields on the envelope or inside the V1 body are silently ignored.
 */
export function parseEnvelope(raw: string | ArrayBuffer): ParsedEnvelope | null {
  let text: string;
  if (typeof raw === "string") {
    text = raw;
  } else {
    text = new TextDecoder().decode(raw);
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object") return null;
  const env = parsed as Record<string, unknown>;
  const protoVersion = env.protocol_version;
  if (typeof protoVersion !== "number") return null;
  if (protoVersion !== PROTOCOL_VERSION_V1) return null;
  if (env.version !== "1") return null;

  const type = env.type;
  if (typeof type !== "string") return null;

  if (type === "task_submit") {
    const taskId = env.task_id;
    const kind = env.kind;
    if (typeof taskId !== "string" || typeof kind !== "string") return null;
    const metadata = (env.metadata && typeof env.metadata === "object"
      ? (env.metadata as Record<string, string>)
      : undefined);
    return {
      protocol_version: protoVersion,
      message: {
        type: "task_submit",
        task_id: taskId,
        kind,
        payload: env.payload,
        ...(metadata ? { metadata } : {})
      }
    };
  }

  if (type === "task_control") {
    const control = env.control;
    const taskId = env.task_id;
    if (typeof control !== "string" || typeof taskId !== "string") return null;
    if (control === "cancel" || control === "pause" || control === "resume") {
      return {
        protocol_version: protoVersion,
        message: { type: "task_control", control, task_id: taskId }
      };
    }
    if (control === "signal") {
      const name = env.name;
      if (typeof name !== "string") return null;
      return {
        protocol_version: protoVersion,
        message: {
          type: "task_control",
          control: "signal",
          task_id: taskId,
          name,
          payload: env.payload
        }
      };
    }
    return null;
  }

  if (type === "task_event") {
    // SessionDO never *receives* TaskEvents, but accept them so the parser
    // round-trips faithfully for tests.
    const taskId = env.task_id;
    const sequence = env.sequence;
    const kind = env.kind;
    if (typeof taskId !== "string" || typeof sequence !== "number") return null;
    if (!kind || typeof kind !== "object") return null;
    return {
      protocol_version: protoVersion,
      message: {
        type: "task_event",
        task_id: taskId,
        sequence,
        timestamp:
          typeof env.timestamp === "string" ? env.timestamp : undefined,
        kind: kind as TaskEventKind
      }
    };
  }

  return null;
}

/**
 * Build a V1 envelope around a `TaskEvent`. The resulting object is JSON-
 * serialized by `JSON.stringify` before being sent over the wire.
 */
export function encodeTaskEvent(event: TaskEvent): string {
  return JSON.stringify({
    protocol_version: PROTOCOL_VERSION_V1,
    version: "1",
    type: "task_event",
    task_id: event.task_id,
    sequence: event.sequence,
    ...(event.timestamp ? { timestamp: event.timestamp } : {}),
    kind: event.kind
  });
}
