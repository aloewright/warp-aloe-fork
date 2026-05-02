//! Append-only audit log for guardrail trips.
//!
//! Stores one JSON object per line. Append-only, never truncated by
//! this code (rotation, if any, is the caller's problem). Failures are
//! logged through `tracing` and never propagated, since the audit log
//! is observability rather than load-bearing state.
//!
//! ## Schema
//!
//! Each line is a JSON object with keys:
//!
//! ```json
//! {
//!   "timestamp": "2026-05-02T12:34:56.789Z",
//!   "task_id": "PDX-28",
//!   "agent_id": "claude_code",
//!   "rule": "diff_size",
//!   "action": "blocked",
//!   "offending_path": "tests/foo_test.rs",
//!   "detail": "diff size 1234 lines exceeds configured cap of 500"
//! }
//! ```
//!
//! `task_id`, `agent_id`, `offending_path`, and `detail` are all
//! optional. The schema mirrors the `audit.log` layout already produced
//! by `crates/symphony/src/audit.rs` so a single tooling surface
//! (`grep`, `jq`) covers both. SQLite mirroring (PDX-71 ledger pattern)
//! is left as a follow-up gated behind `cfg(not(target_family =
//! "wasm"))` once a queryable corpus is needed.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Categorical guardrail rule identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailRule {
    /// [`crate::diff_size::DiffSizeCheck`] tripped.
    DiffSize,
    /// [`crate::test_deletion::TestDeletionCheck`] tripped.
    TestDeletion,
    /// [`crate::deploy_gate::DeployGate`] tripped.
    DeployGate,
}

/// Action recorded against a guardrail trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardrailAction {
    /// Operation was blocked.
    Blocked,
    /// Operation was allowed despite the rule firing (override active).
    Overridden,
    /// Operation was allowed because no rule fired (recorded for audit
    /// completeness on opt-in deployments).
    Allowed,
}

/// One line in the audit log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditEntry {
    /// When the trip was recorded.
    pub timestamp: DateTime<Utc>,
    /// Task / issue id, if available.
    pub task_id: Option<String>,
    /// Agent provider, if available.
    pub agent_id: Option<String>,
    /// Which rule fired.
    pub rule: GuardrailRule,
    /// What happened.
    pub action: GuardrailAction,
    /// Offending file path, if applicable (e.g. test-deletion).
    pub offending_path: Option<String>,
    /// Free-form detail (e.g. the human-readable block reason).
    pub detail: Option<String>,
}

/// Append-only JSONL audit log. Thread-safe via an internal `Mutex`.
pub struct AuditLog {
    path: PathBuf,
    file: Mutex<Option<std::fs::File>>,
}

impl AuditLog {
    /// Open (or create) the log file at `path`. Best effort: if the
    /// parent directory cannot be created, the writer falls back to a
    /// no-op state and emits a `tracing::warn!` on the first attempted
    /// write.
    pub fn open(path: PathBuf) -> Self {
        let file = match Self::open_inner(&path) {
            Ok(f) => Some(f),
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "failed to open audit log");
                None
            }
        };
        Self {
            path,
            file: Mutex::new(file),
        }
    }

    fn open_inner(path: &PathBuf) -> std::io::Result<std::fs::File> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    }

    /// Append `entry` as a JSON line. Failures are logged, never
    /// returned.
    pub fn record(&self, entry: AuditEntry) {
        let line = match serde_json::to_string(&entry) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "failed to serialize audit entry");
                return;
            }
        };
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!(error = %e, "audit log mutex poisoned");
                return;
            }
        };
        if guard.is_none() {
            // Try once to reopen; the parent directory may have appeared
            // since `open()`.
            if let Ok(f) = Self::open_inner(&self.path) {
                *guard = Some(f);
            }
        }
        if let Some(f) = guard.as_mut() {
            if let Err(e) = writeln!(f, "{}", line) {
                tracing::warn!(error = %e, "failed to write audit entry");
            }
        }
    }

    /// Path the log writes to. Useful for tests and tooling.
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rule: GuardrailRule, detail: &str) -> AuditEntry {
        AuditEntry {
            timestamp: Utc::now(),
            task_id: Some("PDX-28".into()),
            agent_id: Some("claude_code".into()),
            rule,
            action: GuardrailAction::Blocked,
            offending_path: None,
            detail: Some(detail.into()),
        }
    }

    #[test]
    fn round_trip_entry() {
        let e = entry(GuardrailRule::DiffSize, "too big");
        let s = serde_json::to_string(&e).unwrap();
        let back: AuditEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back, e);
        // Wire format uses snake_case.
        assert!(s.contains("\"rule\":\"diff_size\""));
        assert!(s.contains("\"action\":\"blocked\""));
    }

    #[test]
    fn append_writes_one_line_per_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = AuditLog::open(path.clone());
        log.record(entry(GuardrailRule::DiffSize, "first"));
        log.record(entry(GuardrailRule::TestDeletion, "second"));
        log.record(entry(GuardrailRule::DeployGate, "third"));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.lines().count(), 3);
        assert!(contents.contains("first"));
        assert!(contents.contains("second"));
        assert!(contents.contains("third"));

        // Each line parses back to a valid AuditEntry.
        for line in contents.lines() {
            let _: AuditEntry = serde_json::from_str(line).expect("valid JSON line");
        }
    }

    #[test]
    fn append_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/audit.log");
        let log = AuditLog::open(path.clone());
        log.record(entry(GuardrailRule::DiffSize, "hello"));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("hello"));
    }

    #[test]
    fn record_after_open_failure_is_noop() {
        // Path that can't be created (the parent is a regular file).
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, b"sentinel").unwrap();
        let path = blocker.join("audit.log");
        let log = AuditLog::open(path.clone());
        // Must not panic.
        log.record(entry(GuardrailRule::DiffSize, "should be dropped"));
        // Sentinel file is untouched.
        assert_eq!(std::fs::read(&blocker).unwrap(), b"sentinel");
    }

    #[test]
    fn append_is_strictly_additive() {
        // Two `AuditLog` handles share the on-disk file but each holds
        // its own descriptor in append mode. We never truncate.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");

        let log1 = AuditLog::open(path.clone());
        log1.record(entry(GuardrailRule::DiffSize, "a"));
        drop(log1);

        let log2 = AuditLog::open(path.clone());
        log2.record(entry(GuardrailRule::TestDeletion, "b"));

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"a\""));
        assert!(contents.contains("\"b\""));
        assert_eq!(contents.lines().count(), 2);
    }
}
