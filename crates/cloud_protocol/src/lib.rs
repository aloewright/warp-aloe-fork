//! # `cloud_protocol`
//!
//! JSON-over-WebSocket wire protocol for the Warp cloud control plane.
//!
//! This crate is **transport-agnostic** and **runtime-agnostic**: it defines
//! only the message types and helpers needed to serialize/deserialize them as
//! JSON. It must compile cleanly on both native targets and
//! `wasm32-unknown-unknown` so that:
//!
//! * the native Rust client (`cloud_client`, PDX-18) can speak the protocol
//!   over `tokio_tungstenite`, and
//! * the Cloudflare Worker / Durable Object (`helm-cloud`, PDX-19/20) can
//!   speak the protocol over the standard WebSocket runtime in workerd.
//!
//! No `tokio`, `axum`, or `tokio_tungstenite` dependency is allowed here —
//! callers wrap these messages in whatever transport they like.
//!
//! ## Versioning strategy
//!
//! Every frame on the wire is a JSON object that includes a
//! [`protocol_version`](Envelope::protocol_version) field (a `u32`) and a
//! variant-tagged [`Message`] payload. The current schema corresponds to
//! [`PROTOCOL_VERSION_V1`] (`1`) and uses the [`Message::V1`] variant.
//!
//! Rules:
//!
//! 1. **Adding new fields to a V1 sub-message is backward compatible** as
//!    long as they are `Option<…>` or have a `serde(default)`. Decoders MUST
//!    ignore unknown fields (we do — see the `forward_compat_unknown_fields`
//!    test).
//! 2. **Breaking changes** (renaming, removing, or repurposing fields, or
//!    changing the type of a field) require a new variant on [`Message`]
//!    (e.g. `V2(V2Message)`) and a bumped [`Envelope::protocol_version`].
//! 3. Decoders should treat an unknown `protocol_version` whose payload they
//!    cannot parse as a [`ParseError::UnsupportedVersion`] and disconnect
//!    cleanly — there is no automatic downgrade.
//!
//! ## Message taxonomy
//!
//! Three top-level kinds, all carried inside [`V1Message`]:
//!
//! * [`TaskSubmit`] — client → server. "Please run this task."
//! * [`TaskEvent`] — server → client. Lifecycle / progress / output / result.
//! * [`TaskControl`] — client → server. Cancel / pause / resume / signal a
//!   running task.
//!
//! ## Example
//!
//! ```
//! use cloud_protocol::{
//!     parse_message, Envelope, Message, TaskSubmit, V1Message, PROTOCOL_VERSION_V1,
//! };
//!
//! let env = Envelope::v1(V1Message::TaskSubmit(TaskSubmit {
//!     task_id: "t-1".into(),
//!     kind: "shell".into(),
//!     payload: serde_json::json!({ "cmd": "echo hi" }),
//!     metadata: Default::default(),
//! }));
//! let bytes = serde_json::to_vec(&env).unwrap();
//! let parsed = parse_message(&bytes).unwrap();
//! assert_eq!(parsed.protocol_version, PROTOCOL_VERSION_V1);
//! assert!(matches!(parsed.message, Message::V1(V1Message::TaskSubmit(_))));
//! ```

#![cfg_attr(not(test), deny(unsafe_code))]
#![warn(missing_docs)]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Version number for the V1 schema. Bump and add a new [`Message`] variant
/// for any breaking change.
pub const PROTOCOL_VERSION_V1: u32 = 1;

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// Top-level wire frame: a versioned envelope around a [`Message`].
///
/// Every JSON frame on the wire is an [`Envelope`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Schema version of `message`. See crate-level docs for the rules.
    pub protocol_version: u32,
    /// The actual payload, dispatched by version.
    #[serde(flatten)]
    pub message: Message,
}

impl Envelope {
    /// Construct a V1 envelope.
    pub fn v1(msg: V1Message) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION_V1,
            message: Message::V1(msg),
        }
    }
}

/// Versioned message dispatch. Add a new variant per protocol bump.
///
/// Tagged on the wire by a `version` field so future versions can be added
/// without breaking existing decoders.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "version")]
pub enum Message {
    /// Schema version 1.
    #[serde(rename = "1")]
    V1(V1Message),
}

// ---------------------------------------------------------------------------
// V1 messages
// ---------------------------------------------------------------------------

/// Discriminated union of every V1 message kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum V1Message {
    /// Client → server: submit a new task.
    TaskSubmit(TaskSubmit),
    /// Server → client: a lifecycle / progress / output / result event for a task.
    TaskEvent(TaskEvent),
    /// Client → server: control signal for a running task (cancel/pause/resume/signal).
    TaskControl(TaskControl),
}

/// Client → server. Submit a new task to be run by the cloud worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSubmit {
    /// Client-generated unique identifier for this task. Echoed back in
    /// every [`TaskEvent`] and used as the target of [`TaskControl`].
    pub task_id: String,
    /// Discriminator for the task body shape (e.g. `"shell"`, `"agent"`,
    /// `"mcp"`). Interpreted by the server's task router.
    pub kind: String,
    /// Opaque task body. The shape depends on `kind` and is deliberately
    /// kept as raw JSON here so the protocol crate doesn't need to know
    /// about every task type.
    pub payload: serde_json::Value,
    /// Free-form string metadata (trace ids, user-agent, region hints, etc.).
    /// Decoders MUST tolerate unknown keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// Server → client. Lifecycle / progress / output / result of a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    /// The task this event refers to.
    pub task_id: String,
    /// Monotonic per-task event sequence, starting at 0. Used by clients to
    /// detect dropped events and to deduplicate on reconnect.
    pub sequence: u64,
    /// Wall-clock timestamp (RFC 3339) at which the server emitted this
    /// event. Optional because some test/transport paths may not have a clock.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// What happened.
    pub kind: TaskEventKind,
}

/// Concrete event types carried by [`TaskEvent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TaskEventKind {
    /// Task has been accepted by the scheduler and a status snapshot follows.
    StatusChanged {
        /// New lifecycle status.
        status: TaskStatus,
    },
    /// Incremental output (stdout/stderr/log line/agent token).
    Output {
        /// Which logical stream produced this chunk.
        stream: OutputStream,
        /// Raw output payload as a UTF-8 string. Binary streams should be
        /// base64-encoded by the producer.
        data: String,
    },
    /// Progress update (0.0..=1.0) with an optional human-readable label.
    Progress {
        /// Fraction complete in `[0.0, 1.0]`.
        fraction: f32,
        /// Optional short label describing the current step.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Terminal event — task has ended one way or another.
    Completed {
        /// Final result.
        result: TaskResult,
    },
}

/// Lifecycle states for a task on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Accepted, not yet running.
    Queued,
    /// Currently executing on a worker.
    Running,
    /// Voluntarily paused via [`TaskControl::Pause`].
    Paused,
    /// Finished successfully.
    Succeeded,
    /// Finished with an error.
    Failed,
    /// Cancelled by the client.
    Cancelled,
}

/// Logical output stream for a [`TaskEventKind::Output`] event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// Process stdout-equivalent.
    Stdout,
    /// Process stderr-equivalent.
    Stderr,
    /// Structured log line.
    Log,
    /// Agent token / tool call / other model-emitted stream.
    Agent,
}

/// Final outcome of a task. Carried by [`TaskEventKind::Completed`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TaskResult {
    /// Successful completion. Optional structured output.
    Success {
        /// Optional structured success payload.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<serde_json::Value>,
    },
    /// Task failed. Carries an error code and message.
    Error {
        /// Stable, machine-readable error code (e.g. `"timeout"`,
        /// `"rejected"`, `"internal"`).
        code: String,
        /// Human-readable error message.
        message: String,
        /// Optional extra details, free-form.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<serde_json::Value>,
    },
    /// Task was cancelled by the client.
    Cancelled,
}

/// Client → server. Control signals for an in-flight task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "control", rename_all = "snake_case")]
pub enum TaskControl {
    /// Cancel the task. Server responds with a [`TaskEventKind::Completed`]
    /// carrying [`TaskResult::Cancelled`] when the cancellation has taken effect.
    Cancel {
        /// Target task id.
        task_id: String,
    },
    /// Pause the task. Implementation-defined — no-op for tasks that don't
    /// support pausing.
    Pause {
        /// Target task id.
        task_id: String,
    },
    /// Resume a previously paused task.
    Resume {
        /// Target task id.
        task_id: String,
    },
    /// Send a named signal (e.g. `"interrupt"`, `"reload-config"`) to the task.
    Signal {
        /// Target task id.
        task_id: String,
        /// Signal name.
        name: String,
        /// Optional structured payload for the signal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payload: Option<serde_json::Value>,
    },
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Errors returned by [`parse_message`].
#[derive(Debug, Error)]
pub enum ParseError {
    /// JSON syntax was invalid.
    #[error("invalid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
    /// The wire frame had a `protocol_version` we don't know how to decode.
    /// Clients should treat this as fatal for the connection.
    #[error("unsupported protocol version: {version}")]
    UnsupportedVersion {
        /// The version that was advertised on the wire.
        version: u32,
    },
}

/// Decode a single wire frame into an [`Envelope`].
///
/// This is the canonical entry point for both client and server, so the
/// dispatch logic doesn't get duplicated. Returns
/// [`ParseError::UnsupportedVersion`] when the envelope advertises a
/// `protocol_version` we don't have a [`Message`] variant for.
pub fn parse_message(bytes: &[u8]) -> Result<Envelope, ParseError> {
    // First decode loosely so we can extract the version even if the
    // message body is from a future schema we don't recognize.
    #[derive(Deserialize)]
    struct VersionPeek {
        protocol_version: u32,
    }
    let peek: VersionPeek = serde_json::from_slice(bytes)?;

    // Attempt the strict decode. If the version is one we know but the body
    // fails to parse, propagate the JSON error. If the version is unknown,
    // surface that explicitly even if the body would happen to parse.
    if peek.protocol_version != PROTOCOL_VERSION_V1 {
        return Err(ParseError::UnsupportedVersion {
            version: peek.protocol_version,
        });
    }
    let env: Envelope = serde_json::from_slice(bytes)?;
    Ok(env)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(env: &Envelope) -> Envelope {
        let bytes = serde_json::to_vec(env).expect("serialize");
        parse_message(&bytes).expect("parse")
    }

    #[test]
    fn roundtrip_task_submit() {
        let mut metadata = BTreeMap::new();
        metadata.insert("trace_id".into(), "abc-123".into());
        let env = Envelope::v1(V1Message::TaskSubmit(TaskSubmit {
            task_id: "task-1".into(),
            kind: "shell".into(),
            payload: json!({ "cmd": "echo hi" }),
            metadata,
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_event_status_changed() {
        let env = Envelope::v1(V1Message::TaskEvent(TaskEvent {
            task_id: "task-1".into(),
            sequence: 0,
            timestamp: Some("2026-05-02T00:00:00Z".into()),
            kind: TaskEventKind::StatusChanged {
                status: TaskStatus::Running,
            },
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_event_output() {
        let env = Envelope::v1(V1Message::TaskEvent(TaskEvent {
            task_id: "task-1".into(),
            sequence: 1,
            timestamp: None,
            kind: TaskEventKind::Output {
                stream: OutputStream::Stdout,
                data: "hello\n".into(),
            },
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_event_progress() {
        let env = Envelope::v1(V1Message::TaskEvent(TaskEvent {
            task_id: "task-1".into(),
            sequence: 2,
            timestamp: None,
            kind: TaskEventKind::Progress {
                fraction: 0.42,
                label: Some("compiling".into()),
            },
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_event_completed_success() {
        let env = Envelope::v1(V1Message::TaskEvent(TaskEvent {
            task_id: "task-1".into(),
            sequence: 3,
            timestamp: None,
            kind: TaskEventKind::Completed {
                result: TaskResult::Success {
                    output: Some(json!({ "exit_code": 0 })),
                },
            },
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_event_completed_error() {
        let env = Envelope::v1(V1Message::TaskEvent(TaskEvent {
            task_id: "task-1".into(),
            sequence: 4,
            timestamp: None,
            kind: TaskEventKind::Completed {
                result: TaskResult::Error {
                    code: "timeout".into(),
                    message: "exceeded 30s".into(),
                    details: None,
                },
            },
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_event_completed_cancelled() {
        let env = Envelope::v1(V1Message::TaskEvent(TaskEvent {
            task_id: "task-1".into(),
            sequence: 5,
            timestamp: None,
            kind: TaskEventKind::Completed {
                result: TaskResult::Cancelled,
            },
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_control_cancel() {
        let env = Envelope::v1(V1Message::TaskControl(TaskControl::Cancel {
            task_id: "task-1".into(),
        }));
        assert_eq!(roundtrip(&env), env);
    }

    #[test]
    fn roundtrip_task_control_pause_resume() {
        let pause = Envelope::v1(V1Message::TaskControl(TaskControl::Pause {
            task_id: "task-1".into(),
        }));
        let resume = Envelope::v1(V1Message::TaskControl(TaskControl::Resume {
            task_id: "task-1".into(),
        }));
        assert_eq!(roundtrip(&pause), pause);
        assert_eq!(roundtrip(&resume), resume);
    }

    #[test]
    fn roundtrip_task_control_signal() {
        let env = Envelope::v1(V1Message::TaskControl(TaskControl::Signal {
            task_id: "task-1".into(),
            name: "interrupt".into(),
            payload: Some(json!({ "reason": "user" })),
        }));
        assert_eq!(roundtrip(&env), env);
    }

    /// Forward-compat: a V1 message with extra unknown fields at every
    /// nesting level must still parse cleanly. This is the load-bearing
    /// guarantee that lets us add `Option<…>` fields in V1 without bumping
    /// the protocol version.
    #[test]
    fn forward_compat_unknown_fields() {
        let raw = json!({
            "protocol_version": 1,
            "version": "1",
            "type": "task_submit",
            "task_id": "t-1",
            "kind": "shell",
            "payload": { "cmd": "ls" },
            "metadata": {},
            // Extra unknown top-level field on the envelope.
            "trace_extra": "ignored",
            // Extra unknown nested field inside the V1 message body.
            "future_field": { "anything": [1, 2, 3] }
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let env = parse_message(&bytes).expect("forward-compat parse");
        assert_eq!(env.protocol_version, PROTOCOL_VERSION_V1);
        match env.message {
            Message::V1(V1Message::TaskSubmit(s)) => {
                assert_eq!(s.task_id, "t-1");
                assert_eq!(s.kind, "shell");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn unsupported_version_is_reported() {
        let raw = json!({
            "protocol_version": 9999,
            "version": "9999",
            "type": "whatever"
        });
        let bytes = serde_json::to_vec(&raw).unwrap();
        let err = parse_message(&bytes).unwrap_err();
        match err {
            ParseError::UnsupportedVersion { version } => assert_eq!(version, 9999),
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn invalid_json_is_reported() {
        let err = parse_message(b"not json at all").unwrap_err();
        assert!(matches!(err, ParseError::InvalidJson(_)));
    }

    /// Sanity check that the wire format is the shape we expect — guards
    /// against accidental serde attribute drift that would silently break
    /// the Worker.
    #[test]
    fn wire_shape_is_stable() {
        let env = Envelope::v1(V1Message::TaskSubmit(TaskSubmit {
            task_id: "t".into(),
            kind: "shell".into(),
            payload: json!({}),
            metadata: BTreeMap::new(),
        }));
        let v: serde_json::Value = serde_json::from_slice(&serde_json::to_vec(&env).unwrap()).unwrap();
        assert_eq!(v["protocol_version"], 1);
        assert_eq!(v["version"], "1");
        assert_eq!(v["type"], "task_submit");
        assert_eq!(v["task_id"], "t");
    }
}
