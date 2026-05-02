//! PDX-28 [D5] auto-healing bounds.
//!
//! Helm-side guardrails layered on top of Symphony's retry/backoff/stall
//! detection. This crate exposes three orthogonal checks (`diff_size`,
//! `test_deletion`, `deploy_gate`) plus an aggregating [`GuardrailSet`]
//! and an append-only [`audit_log::AuditLog`] used to record every
//! guardrail trip.
//!
//! ## Defaults are lenient
//!
//! [`GuardrailSet::default`] returns an empty set: no checks are wired,
//! no commits are blocked. Configuration is opt-in. This makes it safe
//! to merge while the consuming sites are still being wired.
//!
//! ## Wiring sites
//!
//! * Symphony's post-run handler in
//!   `crates/symphony/src/orchestrator.rs::run_post_steps` calls
//!   [`crate::test_deletion`] (and the existing
//!   `symphony::diff_guard::DiffGuard` covers the diff-size case via
//!   `git diff --shortstat`). [`crate::diff_size`] is the
//!   pre-parsed-input alternative used by callers that already have a
//!   numstat parse on hand.
//! * [`crate::deploy_gate`] is library-only at the moment and is meant
//!   to plug into the agent-dispatch path. The current Warp dispatch
//!   layer (`app/src/ai/agent_sdk/driver/local_orchestrator.rs`) wraps
//!   a driver that does not expose a discrete pre-tool-call hook; a
//!   dedicated wiring pass at the MCP forwarder or driver level is
//!   tracked separately so this PR can land independent of those
//!   surfaces.
//!
//! Sites use the same audit-log surface so a single `audit.log`
//! reflects every guardrail trip regardless of dispatcher.
//!
//! ## Cross-platform
//!
//! All modules are macOS-first. Audit-log backed by SQLite (planned
//! follow-up) is gated behind `cfg(not(target_family = "wasm"))`; the
//! current JSONL backend is portable Rust and works in any environment
//! with a writable filesystem.

#![deny(missing_docs)]

pub mod audit_log;
pub mod deploy_gate;
pub mod diff_size;
pub mod test_deletion;

use std::sync::Arc;

pub use audit_log::{AuditEntry, AuditLog, GuardrailRule, GuardrailAction};
pub use deploy_gate::{DeployGate, DeployGateDecision};
pub use diff_size::{DiffSizeCheck, DiffSizeDecision, FileDiff};
pub use test_deletion::{TestDeletionCheck, TestDeletionDecision};

/// Outcome of a [`GuardrailSet`] evaluation. `Allow` means every wired
/// check passed; `Block` means at least one check rejected the input
/// and execution should be halted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardrailOutcome {
    /// Allow the operation to proceed.
    Allow,
    /// Block the operation. The reason is suitable for human display
    /// (Linear comment, log line, etc.).
    Block {
        /// Which rule fired.
        rule: GuardrailRule,
        /// Human-readable explanation.
        reason: String,
    },
}

impl GuardrailOutcome {
    /// `true` if this outcome blocks the operation.
    pub fn is_block(&self) -> bool {
        matches!(self, GuardrailOutcome::Block { .. })
    }
}

/// Composed guardrail set evaluated as a single unit.
///
/// Each member is optional; an empty set is the lenient default and
/// allows everything. The audit log is also optional — if `None`, trips
/// are still returned as `Block` outcomes but no append happens.
#[derive(Default, Clone)]
pub struct GuardrailSet {
    /// Diff-size check (post-run).
    pub diff_size: Option<DiffSizeCheck>,
    /// Test-deletion check (post-run).
    pub test_deletion: Option<TestDeletionCheck>,
    /// Production-deploy command gate (pre-tool-call).
    pub deploy_gate: Option<DeployGate>,
    /// Audit-log sink. Wrapped in `Arc` so callers can share one log
    /// across multiple guardrail sets without rebuilding.
    pub audit: Option<Arc<AuditLog>>,
}

impl GuardrailSet {
    /// Convenience constructor for an empty (allow-all) set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: attach a diff-size check.
    pub fn with_diff_size(mut self, c: DiffSizeCheck) -> Self {
        self.diff_size = Some(c);
        self
    }

    /// Builder: attach a test-deletion check.
    pub fn with_test_deletion(mut self, c: TestDeletionCheck) -> Self {
        self.test_deletion = Some(c);
        self
    }

    /// Builder: attach a deploy gate.
    pub fn with_deploy_gate(mut self, g: DeployGate) -> Self {
        self.deploy_gate = Some(g);
        self
    }

    /// Builder: attach a shared audit log.
    pub fn with_audit_log(mut self, log: Arc<AuditLog>) -> Self {
        self.audit = Some(log);
        self
    }

    /// Evaluate the post-run checks (diff size + test deletion) against
    /// a list of per-file diffs. Returns the *first* `Block` outcome,
    /// or `Allow` if every wired check passed.
    ///
    /// `meta` is associated with any audit-log entry written.
    pub fn evaluate_post_run(
        &self,
        diffs: &[FileDiff],
        meta: &EvaluationMeta,
    ) -> GuardrailOutcome {
        if let Some(check) = &self.diff_size {
            if let DiffSizeDecision::Block { reason } = check.evaluate(diffs) {
                self.record(GuardrailRule::DiffSize, &reason, meta, None);
                return GuardrailOutcome::Block {
                    rule: GuardrailRule::DiffSize,
                    reason,
                };
            }
        }
        if let Some(check) = &self.test_deletion {
            if let TestDeletionDecision::Block {
                reason,
                offending_path,
            } = check.evaluate(diffs)
            {
                self.record(
                    GuardrailRule::TestDeletion,
                    &reason,
                    meta,
                    Some(offending_path.clone()),
                );
                return GuardrailOutcome::Block {
                    rule: GuardrailRule::TestDeletion,
                    reason,
                };
            }
        }
        GuardrailOutcome::Allow
    }

    /// Evaluate the deploy gate against a candidate command line. The
    /// command is the literal shell invocation the agent intends to run
    /// (e.g. `"wrangler deploy --env production"`). Returns `Allow` if
    /// no gate is wired or the gate accepts the command.
    pub fn evaluate_command(&self, cmd: &str, meta: &EvaluationMeta) -> GuardrailOutcome {
        if let Some(gate) = &self.deploy_gate {
            if let DeployGateDecision::Block { reason } = gate.evaluate(cmd) {
                self.record(GuardrailRule::DeployGate, &reason, meta, None);
                return GuardrailOutcome::Block {
                    rule: GuardrailRule::DeployGate,
                    reason,
                };
            }
        }
        GuardrailOutcome::Allow
    }

    fn record(
        &self,
        rule: GuardrailRule,
        reason: &str,
        meta: &EvaluationMeta,
        offending_path: Option<String>,
    ) {
        if let Some(log) = &self.audit {
            let entry = AuditEntry {
                timestamp: chrono::Utc::now(),
                task_id: meta.task_id.clone(),
                agent_id: meta.agent_id.clone(),
                rule,
                action: GuardrailAction::Blocked,
                offending_path,
                detail: Some(reason.to_string()),
            };
            log.record(entry);
        }
    }
}

/// Per-evaluation context attached to audit-log entries.
#[derive(Debug, Clone, Default)]
pub struct EvaluationMeta {
    /// Task / issue id (Symphony issue id, Linear identifier, or local task id).
    pub task_id: Option<String>,
    /// Agent provider tag (e.g. `"claude_code"`, `"codex"`).
    pub agent_id: Option<String>,
}

impl EvaluationMeta {
    /// Convenience builder.
    pub fn new(task_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        Self {
            task_id: Some(task_id.into()),
            agent_id: Some(agent_id.into()),
        }
    }
}

#[cfg(test)]
mod set_tests {
    use super::*;

    #[test]
    fn default_set_allows_everything() {
        let set = GuardrailSet::new();
        let outcome = set.evaluate_post_run(
            &[FileDiff {
                path: "anything.rs".into(),
                added_lines: 99_999,
                removed_lines: 99_999,
                deleted: true,
            }],
            &EvaluationMeta::default(),
        );
        assert_eq!(outcome, GuardrailOutcome::Allow);
    }

    #[test]
    fn default_set_allows_any_command() {
        let set = GuardrailSet::new();
        let outcome = set.evaluate_command(
            "wrangler deploy --env production",
            &EvaluationMeta::default(),
        );
        assert_eq!(outcome, GuardrailOutcome::Allow);
    }

    #[test]
    fn diff_size_check_blocks_when_wired() {
        let set = GuardrailSet::new().with_diff_size(DiffSizeCheck::new(500));
        let diffs = vec![FileDiff {
            path: "src/big.rs".into(),
            added_lines: 600,
            removed_lines: 0,
            deleted: false,
        }];
        let outcome = set.evaluate_post_run(&diffs, &EvaluationMeta::default());
        assert!(outcome.is_block());
        match outcome {
            GuardrailOutcome::Block { rule, .. } => {
                assert_eq!(rule, GuardrailRule::DiffSize);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_deletion_blocks_when_wired() {
        let set = GuardrailSet::new().with_test_deletion(TestDeletionCheck::default());
        let diffs = vec![FileDiff {
            path: "crates/foo/tests/integration.rs".into(),
            added_lines: 0,
            removed_lines: 80,
            deleted: true,
        }];
        let outcome = set.evaluate_post_run(&diffs, &EvaluationMeta::default());
        assert!(outcome.is_block());
        match outcome {
            GuardrailOutcome::Block { rule, .. } => {
                assert_eq!(rule, GuardrailRule::TestDeletion);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn deploy_gate_blocks_when_wired() {
        let set = GuardrailSet::new().with_deploy_gate(DeployGate::default());
        let outcome = set.evaluate_command(
            "wrangler deploy --env production",
            &EvaluationMeta::default(),
        );
        assert!(outcome.is_block());
        match outcome {
            GuardrailOutcome::Block { rule, .. } => assert_eq!(rule, GuardrailRule::DeployGate),
            _ => unreachable!(),
        }
    }

    #[test]
    fn diff_size_fires_before_test_deletion() {
        // Both rules would trip; diff_size is checked first.
        let set = GuardrailSet::new()
            .with_diff_size(DiffSizeCheck::new(10))
            .with_test_deletion(TestDeletionCheck::default());
        let diffs = vec![FileDiff {
            path: "tests/foo_test.rs".into(),
            added_lines: 0,
            removed_lines: 100,
            deleted: true,
        }];
        let outcome = set.evaluate_post_run(&diffs, &EvaluationMeta::default());
        match outcome {
            GuardrailOutcome::Block { rule, .. } => assert_eq!(rule, GuardrailRule::DiffSize),
            _ => panic!("expected block"),
        }
    }

    #[test]
    fn audit_log_records_block() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.log");
        let log = Arc::new(AuditLog::open(path.clone()));
        let set = GuardrailSet::new()
            .with_diff_size(DiffSizeCheck::new(10))
            .with_audit_log(log.clone());

        let outcome = set.evaluate_post_run(
            &[FileDiff {
                path: "x.rs".into(),
                added_lines: 50,
                removed_lines: 0,
                deleted: false,
            }],
            &EvaluationMeta::new("PDX-28", "claude_code"),
        );
        assert!(outcome.is_block());

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("\"diff_size\""), "log: {contents}");
        assert!(contents.contains("\"PDX-28\""));
        assert!(contents.contains("\"claude_code\""));
    }
}
