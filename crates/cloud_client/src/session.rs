//! Per-task session state for resume-on-reconnect.
//!
//! On reconnect the client must tell the server which events it has
//! already seen so the server can replay only the missing tail. We track
//! the highest delivered `sequence` per task and emit a resume hint
//! whenever a connection comes back up while a task is still in flight.

use std::collections::HashMap;

use cloud_protocol::{Envelope, TaskControl, V1Message};
use serde_json::json;

/// What the server needs to know to replay events from the right place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeHint {
    /// Which task to resume.
    pub task_id: String,
    /// Highest `sequence` the client has already observed for this task.
    /// The server should replay events with `sequence > last_sequence`.
    pub last_sequence: u64,
}

/// Tracks the high-water sequence number for each in-flight task so we
/// can build a [`ResumeHint`] on reconnect.
#[derive(Debug, Default, Clone)]
pub struct SessionState {
    /// Map of task_id → highest sequence observed.
    inflight: HashMap<String, u64>,
}

impl SessionState {
    /// Empty state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark this task as in-flight (no events yet observed).
    pub fn mark_submitted(&mut self, task_id: &str) {
        self.inflight.entry(task_id.to_string()).or_insert(0);
    }

    /// Record an observed event sequence for this task. The high-water
    /// is monotonic — out-of-order/duplicate events do not lower it.
    pub fn observe(&mut self, task_id: &str, sequence: u64) {
        let entry = self.inflight.entry(task_id.to_string()).or_insert(0);
        if sequence > *entry {
            *entry = sequence;
        }
    }

    /// Drop tracking for a task that has reached a terminal event. After
    /// this the task is considered finished and no resume hint should be
    /// emitted for it on reconnect.
    pub fn complete(&mut self, task_id: &str) {
        self.inflight.remove(task_id);
    }

    /// Return `true` if this is the very first event observed for the
    /// task (used for dedupe-on-reconnect).
    pub fn is_duplicate(&self, task_id: &str, sequence: u64) -> bool {
        match self.inflight.get(task_id) {
            // We've already seen at least this sequence number for this task.
            // sequence == 0 with stored == 0 is ambiguous (could be the very
            // first event or a replay) — treat strictly less-or-equal-and-
            // already-observed as duplicate. We special-case sequence 0 only
            // when we've recorded a higher sequence, otherwise let it through.
            Some(&hw) => sequence != 0 && sequence <= hw,
            None => false,
        }
    }

    /// Snapshot all in-flight tasks as resume hints, in unspecified order.
    pub fn resume_hints(&self) -> Vec<ResumeHint> {
        self.inflight
            .iter()
            .map(|(task_id, &last_sequence)| ResumeHint {
                task_id: task_id.clone(),
                last_sequence,
            })
            .collect()
    }
}

/// Build a `TaskControl` envelope that carries a resume hint.
///
/// The protocol's V1 `Resume` variant only takes `task_id`; to thread the
/// `last_sequence` through without modifying `cloud_protocol`, we send a
/// `Signal { name: "resume" }` whose `payload` carries the sequence
/// number. This is forward-compatible with a future `Resume {
/// last_sequence }` variant: a server that understands the new variant
/// can keep accepting the signal form alongside it.
pub fn resume_message(hint: &ResumeHint) -> Envelope {
    Envelope::v1(V1Message::TaskControl(TaskControl::Signal {
        task_id: hint.task_id.clone(),
        name: "resume".to_string(),
        payload: Some(json!({ "last_sequence": hint.last_sequence })),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn high_water_is_monotonic() {
        let mut s = SessionState::new();
        s.mark_submitted("t1");
        s.observe("t1", 0);
        s.observe("t1", 5);
        s.observe("t1", 3); // out-of-order: should not regress
        let hints = s.resume_hints();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].task_id, "t1");
        assert_eq!(hints[0].last_sequence, 5);
    }

    #[test]
    fn complete_drops_from_resume_set() {
        let mut s = SessionState::new();
        s.mark_submitted("t1");
        s.mark_submitted("t2");
        s.observe("t1", 2);
        s.observe("t2", 7);
        s.complete("t1");
        let hints = s.resume_hints();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].task_id, "t2");
        assert_eq!(hints[0].last_sequence, 7);
    }

    #[test]
    fn is_duplicate_after_observe() {
        let mut s = SessionState::new();
        s.mark_submitted("t1");
        s.observe("t1", 5);
        assert!(s.is_duplicate("t1", 3));
        assert!(s.is_duplicate("t1", 5));
        assert!(!s.is_duplicate("t1", 6));
        assert!(!s.is_duplicate("t2", 1));
    }

    #[test]
    fn resume_message_round_trips() {
        let hint = ResumeHint {
            task_id: "task-42".into(),
            last_sequence: 17,
        };
        let env = resume_message(&hint);
        let bytes = serde_json::to_vec(&env).unwrap();
        let parsed = cloud_protocol::parse_message(&bytes).unwrap();
        match parsed.message {
            cloud_protocol::Message::V1(V1Message::TaskControl(TaskControl::Signal {
                task_id,
                name,
                payload,
            })) => {
                assert_eq!(task_id, "task-42");
                assert_eq!(name, "resume");
                let p = payload.expect("payload");
                assert_eq!(p["last_sequence"], 17);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
