//! 5-minute-or-less CI smoke for the soak harness.
//!
//! Compresses the 72h soak into a sub-minute run that still exercises every
//! invariant once. CI executes this via `cargo test -p soak_harness`. The
//! full-duration variants are operator-triggered via the `soak_harness`
//! binary with `--profile thirty-minute-smoke` or `--profile full-weekend`.

#![cfg(not(target_family = "wasm"))]

use std::time::Duration;

use soak_harness::{
    seed_fixtures, BehaviorTag, FaultSchedule, FixtureIssue, HarnessConfig, HarnessRunner,
};

fn smoke_fixtures() -> Vec<FixtureIssue> {
    // Trim to one of every behaviour tag so the smoke covers each
    // invariant surface exactly once. The full catalog has ~50 issues
    // and is reserved for the operator-triggered runs.
    let mut taken = std::collections::HashSet::new();
    let mut out = Vec::new();
    for f in seed_fixtures() {
        if taken.insert(f.tag) {
            out.push(f);
        }
    }
    // We always want at least the eight tags represented.
    assert!(out.len() >= 8, "smoke fixtures must cover every tag");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ci_smoke_completes_under_a_minute() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = HarnessConfig {
        // 25 seconds is enough to fire every fault on the CI-smoke
        // schedule (last offset is 20s) and let in-flight stalls trip
        // the 5s synthetic stall_timeout_ms.
        duration: Duration::from_secs(25),
        tick: Duration::from_millis(250),
        audit_path: dir.path().join("audit.log"),
        metrics_path: Some(dir.path().join("metrics.jsonl")),
        workspace_root: dir.path().join("workspaces"),
        max_concurrent_agents: 3,
        agent_label: "agent:claude".to_string(),
        faults: FaultSchedule::default_ci_smoke(),
        fixtures: smoke_fixtures(),
        // Stall-detection is configured to 5s in the harness's synthetic
        // workflow, so we want the invariant's threshold to be permissive
        // enough to not flag a stalled fixture during normal operation.
        stuck_task_threshold: Duration::from_secs(120),
    };

    let runner = HarnessRunner::new(cfg).expect("ctor");
    let board = runner.board();
    let agent = runner.agent();

    let started = std::time::Instant::now();
    let summary = runner.run(None).await.expect("run");
    let elapsed = started.elapsed();

    // ---- shape assertions ----
    assert!(
        elapsed < Duration::from_secs(60),
        "smoke ran too long: {:?}",
        elapsed
    );
    assert!(summary.ticks > 5, "ticks too low: {}", summary.ticks);
    assert!(
        summary.final_snapshot.ticks_observed >= 1,
        "expected ≥1 audit Tick"
    );
    assert!(
        summary.final_snapshot.tasks_dispatched >= 1,
        "expected ≥1 dispatch"
    );
    assert!(board.poll_count() > 0, "board never polled");
    assert!(agent.invocations() > 0, "agent never invoked");

    // ---- invariant assertions ----
    let breaches: Vec<_> = summary
        .invariant_rows
        .iter()
        .filter(|r| matches!(r.status, soak_harness::invariants::InvariantStatus::Breach))
        .collect();
    assert!(
        breaches.is_empty(),
        "smoke surfaced {} invariant breach(es): {:?}",
        breaches.len(),
        breaches
    );

    // ---- coverage assertion ----
    // Every invariant must have run at least once.
    let names: std::collections::HashSet<&str> = summary
        .invariant_rows
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    for required in [
        "audit_log_append_only",
        "concurrency_cap",
        "no_stuck_tasks",
        "no_test_deletion",
    ] {
        assert!(names.contains(required), "missing invariant {required}");
    }

    // ---- fault assertion ----
    // tracker_outage is the only fault wired on this branch — it must
    // have fired and recovered. The other three are surfaced as Skipped
    // gaps for the runbook.
    let recovered = summary
        .fault_outcomes
        .iter()
        .filter(|f| matches!(f.status, soak_harness::faults::FaultStatus::Recovered))
        .count();
    assert!(recovered >= 1, "no faults recovered");
}

#[tokio::test]
async fn ci_smoke_writes_jsonl_metrics_stream() {
    let dir = tempfile::tempdir().unwrap();
    let metrics = dir.path().join("metrics.jsonl");
    let cfg = HarnessConfig {
        duration: Duration::from_secs(3),
        tick: Duration::from_millis(200),
        audit_path: dir.path().join("audit.log"),
        metrics_path: Some(metrics.clone()),
        workspace_root: dir.path().join("workspaces"),
        max_concurrent_agents: 2,
        agent_label: "agent:claude".to_string(),
        faults: FaultSchedule::empty(),
        fixtures: vec![FixtureIssue {
            seq: 1,
            tag: BehaviorTag::HappyFast,
            title: "x".into(),
        }],
        stuck_task_threshold: Duration::from_secs(60),
    };
    let runner = HarnessRunner::new(cfg).unwrap();
    let _ = runner.run(None).await.unwrap();
    let body = std::fs::read_to_string(&metrics).unwrap();
    let n = body.lines().count();
    assert!(n >= 5, "metrics stream should have at least one line per tick: got {n}");
    // Every line must be valid JSON.
    for line in body.lines() {
        let _: serde_json::Value = serde_json::from_str(line).expect("metrics line is JSON");
    }
}
