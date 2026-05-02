//! End-to-end integration tests for [`auto_healing::GuardrailSet`].
//!
//! Exercises the public API the way the dispatch site in
//! `crates/symphony/src/orchestrator.rs` and
//! `app/src/ai/agent_sdk/driver/local_orchestrator.rs` will: build a
//! configured [`GuardrailSet`] with a shared on-disk audit log, drive
//! a "commit" through it, then assert both the returned outcome and
//! the resulting audit-log JSONL.

use std::sync::Arc;

use auto_healing::{
    AuditLog, DeployGate, DiffSizeCheck, EvaluationMeta, FileDiff, GuardrailOutcome,
    GuardrailRule, GuardrailSet, TestDeletionCheck,
};

fn standard_set(audit: Arc<AuditLog>) -> GuardrailSet {
    GuardrailSet::new()
        .with_diff_size(DiffSizeCheck::new(500))
        .with_test_deletion(TestDeletionCheck::default())
        .with_deploy_gate(DeployGate::default())
        .with_audit_log(audit)
}

fn read_audit(log: &AuditLog) -> Vec<serde_json::Value> {
    let raw = std::fs::read_to_string(log.path()).unwrap_or_default();
    raw.lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("valid JSON"))
        .collect()
}

#[test]
fn small_diff_passes_and_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(AuditLog::open(dir.path().join("audit.log")));
    let set = standard_set(log.clone());

    let diffs = vec![FileDiff {
        path: "src/foo.rs".into(),
        added_lines: 12,
        removed_lines: 4,
        deleted: false,
    }];
    let outcome = set.evaluate_post_run(&diffs, &EvaluationMeta::new("PDX-28", "claude_code"));
    assert_eq!(outcome, GuardrailOutcome::Allow);

    let entries = read_audit(&log);
    assert!(
        entries.is_empty(),
        "no guardrail tripped, expected empty log; got: {entries:?}"
    );
}

#[test]
fn oversize_diff_blocks_and_appends_audit_entry() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(AuditLog::open(dir.path().join("audit.log")));
    let set = standard_set(log.clone());

    let diffs = vec![FileDiff {
        path: "src/big.rs".into(),
        added_lines: 800,
        removed_lines: 0,
        deleted: false,
    }];
    let outcome =
        set.evaluate_post_run(&diffs, &EvaluationMeta::new("PDX-28", "claude_code"));
    match &outcome {
        GuardrailOutcome::Block { rule, reason } => {
            assert_eq!(*rule, GuardrailRule::DiffSize);
            assert!(reason.contains("800"), "reason: {reason}");
        }
        _ => panic!("expected diff-size block"),
    }

    let entries = read_audit(&log);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["rule"], "diff_size");
    assert_eq!(entries[0]["action"], "blocked");
    assert_eq!(entries[0]["task_id"], "PDX-28");
    assert_eq!(entries[0]["agent_id"], "claude_code");
    assert!(entries[0]["timestamp"].is_string());
}

#[test]
fn test_file_deletion_blocks_with_offending_path() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(AuditLog::open(dir.path().join("audit.log")));
    let set = standard_set(log.clone());

    // Acceptance criterion: an agent deleting
    // `app/src/auth/auth_manager_test.rs` gets caught.
    let diffs = vec![FileDiff {
        path: "app/src/auth/auth_manager_test.rs".into(),
        added_lines: 0,
        removed_lines: 250,
        deleted: true,
    }];
    let outcome = set.evaluate_post_run(
        &diffs,
        &EvaluationMeta::new("PDX-28-acceptance", "codex"),
    );
    match &outcome {
        GuardrailOutcome::Block { rule, reason } => {
            assert_eq!(*rule, GuardrailRule::TestDeletion);
            assert!(reason.contains("auth_manager_test.rs"), "reason: {reason}");
        }
        _ => panic!("expected test-deletion block"),
    }

    let entries = read_audit(&log);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["rule"], "test_deletion");
    assert_eq!(entries[0]["offending_path"], "app/src/auth/auth_manager_test.rs");
}

#[test]
fn deploy_command_blocked_with_audit_entry() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(AuditLog::open(dir.path().join("audit.log")));
    let set = standard_set(log.clone());

    // Acceptance criterion: an agent issuing
    // `wrangler deploy --env production` gets blocked.
    let outcome = set.evaluate_command(
        "wrangler deploy --env production",
        &EvaluationMeta::new("PDX-28-acceptance", "claude_code"),
    );
    match &outcome {
        GuardrailOutcome::Block { rule, .. } => {
            assert_eq!(*rule, GuardrailRule::DeployGate);
        }
        _ => panic!("expected deploy-gate block"),
    }

    let entries = read_audit(&log);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["rule"], "deploy_gate");
}

#[test]
fn multiple_blocks_are_appended_chronologically() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(AuditLog::open(dir.path().join("audit.log")));
    let set = standard_set(log.clone());

    // Three independent trips, each on its own meta.
    let _ = set.evaluate_post_run(
        &[FileDiff {
            path: "src/big.rs".into(),
            added_lines: 1_000,
            removed_lines: 0,
            deleted: false,
        }],
        &EvaluationMeta::new("PDX-A", "agent-1"),
    );
    let _ = set.evaluate_post_run(
        &[FileDiff {
            path: "tests/foo.rs".into(),
            added_lines: 0,
            removed_lines: 50,
            deleted: true,
        }],
        &EvaluationMeta::new("PDX-B", "agent-2"),
    );
    let _ = set.evaluate_command(
        "cargo publish",
        &EvaluationMeta::new("PDX-C", "agent-3"),
    );

    let entries = read_audit(&log);
    assert_eq!(entries.len(), 3);
    let rules: Vec<&str> = entries
        .iter()
        .map(|e| e["rule"].as_str().unwrap())
        .collect();
    assert_eq!(rules, vec!["diff_size", "test_deletion", "deploy_gate"]);
    let task_ids: Vec<&str> = entries
        .iter()
        .map(|e| e["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(task_ids, vec!["PDX-A", "PDX-B", "PDX-C"]);
}

#[test]
fn empty_set_lets_everything_through_with_empty_log() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(AuditLog::open(dir.path().join("audit.log")));
    // Default = lenient; the Arc is wired but no checks fire.
    let set = GuardrailSet::new().with_audit_log(log.clone());

    let outcome = set.evaluate_post_run(
        &[FileDiff {
            path: "anything.rs".into(),
            added_lines: 99_999,
            removed_lines: 99_999,
            deleted: true,
        }],
        &EvaluationMeta::new("PDX-X", "agent-y"),
    );
    assert_eq!(outcome, GuardrailOutcome::Allow);
    assert_eq!(
        set.evaluate_command("wrangler deploy --env production", &EvaluationMeta::default()),
        GuardrailOutcome::Allow
    );
    let entries = read_audit(&log);
    assert!(entries.is_empty(), "no checks → no log entries");
}

#[test]
fn override_disables_deploy_gate_per_call() {
    let dir = tempfile::tempdir().unwrap();
    let log = Arc::new(AuditLog::open(dir.path().join("audit.log")));
    let set = GuardrailSet::new()
        .with_deploy_gate(DeployGate::default().with_override(true))
        .with_audit_log(log.clone());

    let outcome = set.evaluate_command(
        "wrangler deploy --env production",
        &EvaluationMeta::new("PDX-Z", "agent-z"),
    );
    assert_eq!(outcome, GuardrailOutcome::Allow);
    assert!(read_audit(&log).is_empty());
}
