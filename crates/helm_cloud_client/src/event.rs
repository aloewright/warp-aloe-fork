// SPDX-License-Identifier: AGPL-3.0-only
//
// Minimal `TaskEvent` shape for streaming WS messages from helm-cloud's
// SessionDO (PDX-20). The full set of variants lives in
// `crates/cloud_protocol`, but the spec forbids us from modifying that
// crate, and the `cloud_protocol::TaskEvent` type doesn't yet have the
// helm-cloud-shaped tagged JSON envelope. We define a small permissive
// shape here and let consumers map to whatever they already display.

use serde::{Deserialize, Serialize};

/// One event off the WS stream. Variants intentionally cover the
/// envelope the SessionDO emits today; unknown kinds round-trip through
/// `Other` so a server-side schema bump doesn't crash the client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TaskEvent {
    /// Session was successfully created on the SessionDO.
    SessionStarted { session_id: String },
    /// Agent-runtime container produced a chunk of stdout/stderr.
    AgentChunk {
        session_id: String,
        text: String,
        #[serde(default)]
        stream: Option<String>,
    },
    /// Agent invoked a tool.
    ToolCall {
        session_id: String,
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    /// Agent received a tool result.
    ToolResult {
        session_id: String,
        tool: String,
        #[serde(default)]
        result: serde_json::Value,
    },
    /// Agent reported task completion.
    Completed {
        session_id: String,
        #[serde(default)]
        summary: Option<String>,
    },
    /// Agent reported a failure.
    Failed {
        session_id: String,
        error: String,
    },
    /// Catch-all for forward-compatibility.
    #[serde(other)]
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_started() {
        let raw = r#"{"kind":"session_started","session_id":"sess_1"}"#;
        let ev: TaskEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(
            ev,
            TaskEvent::SessionStarted {
                session_id: "sess_1".into()
            }
        );
    }

    #[test]
    fn unknown_kind_decodes_as_other() {
        let raw = r#"{"kind":"future_thing","foo":42}"#;
        let ev: TaskEvent = serde_json::from_str(raw).unwrap();
        assert_eq!(ev, TaskEvent::Other);
    }

    #[test]
    fn agent_chunk_round_trips() {
        let ev = TaskEvent::AgentChunk {
            session_id: "s".into(),
            text: "hello".into(),
            stream: Some("stdout".into()),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: TaskEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }
}
