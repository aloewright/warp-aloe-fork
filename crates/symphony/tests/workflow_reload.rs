//! Tests for live `WORKFLOW.md` reload (PDX-111).
//!
//! These exercise the lower-level `apply_reload` directly against a
//! [`WorkflowHandle`] + tempfile pair, which is the deterministic surface.
//! End-to-end watcher tests would have to wait on filesystem-event timing,
//! which is flaky on busy CI; the watcher's wiring (debouncer + Tokio
//! channel) is shared with `crates/prompts::hot_reload` and exercised there.

use std::path::Path;
use std::sync::Arc;

use symphony::audit::AuditLog;
use symphony::reload::{apply_reload, WorkflowHandle};
use symphony::workflow::WorkflowDefinition;
use tempfile::TempDir;

/// Workflow front-matter template parameterised on a few values that the
/// reload tests poke at. Body is a constant Liquid template.
fn workflow_yaml(interval_ms: u64, max_concurrent: usize, root: &Path) -> String {
    format!(
        r#"---
tracker:
  api_key: literal-key
  project_slug: x
polling:
  interval_ms: {interval_ms}
workspace:
  root: {root_str}
agent:
  max_concurrent_agents: {max_concurrent}
  agent_label_required: "agent:claude"
---
Issue {{{{ issue.identifier }}}}: {{{{ issue.title }}}}
"#,
        interval_ms = interval_ms,
        max_concurrent = max_concurrent,
        root_str = root.display(),
    )
}

fn audit_log(dir: &Path) -> Arc<AuditLog> {
    Arc::new(AuditLog::open(dir.join("audit.log")))
}

fn read_audit_kinds(audit_path: &Path) -> Vec<String> {
    let raw = std::fs::read_to_string(audit_path).unwrap_or_default();
    raw.lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
        .collect()
}

#[test]
fn apply_reload_picks_up_polling_interval_change() {
    let dir = TempDir::new().unwrap();
    let workflow_path = dir.path().join("WORKFLOW.md");
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(&workflow_path, workflow_yaml(30_000, 1, &root)).unwrap();
    let initial = WorkflowDefinition::load(&workflow_path).unwrap();
    let handle = Arc::new(WorkflowHandle::new(initial));
    assert_eq!(handle.load().config.polling.interval_ms, 30_000);

    // Edit the file in place and re-apply.
    std::fs::write(&workflow_path, workflow_yaml(5_000, 1, &root)).unwrap();
    let audit = audit_log(dir.path());
    apply_reload(&workflow_path, &handle, &audit);

    assert_eq!(
        handle.load().config.polling.interval_ms,
        5_000,
        "new interval should be live after a successful reload"
    );

    let kinds = read_audit_kinds(&dir.path().join("audit.log"));
    assert!(
        kinds.iter().any(|k| k == "workflow_reloaded"),
        "expected workflow_reloaded audit event, got {kinds:?}"
    );
}

#[test]
fn apply_reload_keeps_previous_definition_on_yaml_error() {
    let dir = TempDir::new().unwrap();
    let workflow_path = dir.path().join("WORKFLOW.md");
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(&workflow_path, workflow_yaml(30_000, 2, &root)).unwrap();
    let initial = WorkflowDefinition::load(&workflow_path).unwrap();
    let handle = Arc::new(WorkflowHandle::new(initial));

    // Corrupt the file with intentionally broken YAML.
    std::fs::write(
        &workflow_path,
        "---\ntracker:\n  api_key: literal-key\n  project_slug: x\n  active_states: [unterminated\n---\nbody\n",
    )
    .unwrap();

    let audit = audit_log(dir.path());
    apply_reload(&workflow_path, &handle, &audit);

    // Previous definition stays live.
    let now = handle.load();
    assert_eq!(now.config.polling.interval_ms, 30_000);
    assert_eq!(now.config.agent.max_concurrent_agents, 2);

    let kinds = read_audit_kinds(&dir.path().join("audit.log"));
    assert!(
        kinds.iter().any(|k| k == "workflow_reload_failed"),
        "expected workflow_reload_failed audit event, got {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "workflow_reloaded"),
        "must not record a successful reload on parse failure: {kinds:?}"
    );
}

#[test]
fn apply_reload_rejects_workspace_root_change() {
    let dir = TempDir::new().unwrap();
    let workflow_path = dir.path().join("WORKFLOW.md");
    let root_a = dir.path().join("ws_a");
    let root_b = dir.path().join("ws_b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    std::fs::write(&workflow_path, workflow_yaml(30_000, 1, &root_a)).unwrap();
    let initial = WorkflowDefinition::load(&workflow_path).unwrap();
    assert_eq!(initial.config.workspace.root, root_a);
    let handle = Arc::new(WorkflowHandle::new(initial));

    // Re-write with a *different* workspace root. Reload must reject this.
    std::fs::write(&workflow_path, workflow_yaml(15_000, 1, &root_b)).unwrap();
    let audit = audit_log(dir.path());
    apply_reload(&workflow_path, &handle, &audit);

    // Previous root preserved; previous interval preserved (we reject the
    // whole reload, not just the root field).
    let now = handle.load();
    assert_eq!(now.config.workspace.root, root_a, "root must be unchanged");
    assert_eq!(
        now.config.polling.interval_ms, 30_000,
        "rejected reloads do not partially apply"
    );

    let kinds = read_audit_kinds(&dir.path().join("audit.log"));
    assert!(
        kinds.iter().any(|k| k == "workflow_reload_rejected"),
        "expected workflow_reload_rejected audit event, got {kinds:?}"
    );
}

#[test]
fn apply_reload_handles_replace_via_rename() {
    // Simulates an editor's "atomic save": write to a tempfile, then rename
    // over the target. PDX-111 calls out that this is a deliberate "reload
    // gesture" and must trigger a re-parse against the new contents.
    let dir = TempDir::new().unwrap();
    let workflow_path = dir.path().join("WORKFLOW.md");
    let staging = dir.path().join("WORKFLOW.md.tmp");
    let root = dir.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();

    std::fs::write(&workflow_path, workflow_yaml(30_000, 1, &root)).unwrap();
    let initial = WorkflowDefinition::load(&workflow_path).unwrap();
    let handle = Arc::new(WorkflowHandle::new(initial));

    std::fs::write(&staging, workflow_yaml(7_500, 4, &root)).unwrap();
    std::fs::rename(&staging, &workflow_path).unwrap();
    let audit = audit_log(dir.path());
    apply_reload(&workflow_path, &handle, &audit);

    let now = handle.load();
    assert_eq!(now.config.polling.interval_ms, 7_500);
    assert_eq!(now.config.agent.max_concurrent_agents, 4);
}
