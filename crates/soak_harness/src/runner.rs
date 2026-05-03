//! Driver loop for the soak harness.
//!
//! Boots a real `symphony::Orchestrator` against an in-memory
//! [`crate::SyntheticBoard`] + [`crate::SyntheticAgent`] pair, then ticks it
//! on a fixed cadence while:
//!
//! * tailing the audit log into [`crate::MetricsSink`],
//! * running [`crate::Invariants`] after every tick,
//! * applying faults from the configured [`crate::FaultSchedule`] at the
//!   right offsets.
//!
//! The runner is purely additive: it never touches private state on the
//! upstream crates — only the `IssueSource` / `Agent` traits and the
//! audit-log file format.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use orchestrator::{AgentRegistration, Budget, Cap, Provider, Router};
use serde::{Deserialize, Serialize};
use symphony::audit::AuditLog;
use symphony::orchestrator::{IssueSource, Orchestrator};
use symphony::workflow::WorkflowDefinition;
use symphony::workspace::WorkspaceManager;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncSeekExt, BufReader, SeekFrom};
use tokio::sync::oneshot;

use crate::board::SyntheticBoard;
use crate::faults::{self, FaultOutcome, FaultSchedule, ScheduledFault};
use crate::fixtures::{seed_fixtures, FixtureIssue};
use crate::invariants::Invariants;
use crate::metrics::{MetricsSink, MetricsSnapshot};
use crate::synthetic_agent::SyntheticAgent;

/// Configuration consumed by [`HarnessRunner::run`].
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Total duration of the run.
    pub duration: Duration,
    /// Tick cadence (how often the runner calls `Orchestrator::tick`).
    pub tick: Duration,
    /// Path the harness writes its audit log to. Created if missing.
    pub audit_path: PathBuf,
    /// Optional path for the JSONL metrics stream.
    pub metrics_path: Option<PathBuf>,
    /// Workspace root for per-issue scratch directories.
    pub workspace_root: PathBuf,
    /// Concurrent agent cap forwarded into the synthetic
    /// `WORKFLOW.md` config.
    pub max_concurrent_agents: usize,
    /// Linear label required on synthetic issues. Defaults to
    /// `agent:claude` to match the upstream Symphony default.
    pub agent_label: String,
    /// Fault schedule. Use [`FaultSchedule::empty`] to disable.
    pub faults: FaultSchedule,
    /// Fixture catalog. Use [`seed_fixtures`] for the default 50+ catalog
    /// or pass a smaller subset for fast smoke runs.
    pub fixtures: Vec<FixtureIssue>,
    /// How long an issue may sit in `running` without an audit event
    /// before the stuck-task invariant flags it. Defaults to 30 minutes.
    pub stuck_task_threshold: Duration,
}

impl HarnessConfig {
    /// Reasonable smoke defaults: 5-minute run, 1-second tick, default
    /// fixtures, CI-smoke faults. Audit + metrics paths default into a
    /// freshly-created tempdir which the caller is responsible for
    /// keeping alive.
    pub fn smoke_defaults(scratch: &Path) -> Self {
        Self {
            duration: Duration::from_secs(5 * 60),
            tick: Duration::from_secs(1),
            audit_path: scratch.join("audit.log"),
            metrics_path: Some(scratch.join("metrics.jsonl")),
            workspace_root: scratch.join("workspaces"),
            max_concurrent_agents: 3,
            agent_label: "agent:claude".to_string(),
            faults: FaultSchedule::default_ci_smoke(),
            fixtures: seed_fixtures(),
            stuck_task_threshold: Duration::from_secs(30 * 60),
        }
    }

    /// 30-minute smoke variant designed for the operator-triggered
    /// "smoke soak" used as a CI gate. Slightly longer than the unit-test
    /// smoke so all four faults fire on their default offsets.
    pub fn thirty_minute_smoke(scratch: &Path) -> Self {
        Self {
            duration: Duration::from_secs(30 * 60),
            tick: Duration::from_secs(2),
            audit_path: scratch.join("audit.log"),
            metrics_path: Some(scratch.join("metrics.jsonl")),
            workspace_root: scratch.join("workspaces"),
            max_concurrent_agents: 3,
            agent_label: "agent:claude".to_string(),
            faults: FaultSchedule::default_smoke(),
            fixtures: seed_fixtures(),
            stuck_task_threshold: Duration::from_secs(15 * 60),
        }
    }

    /// Full 72-hour weekend run. Operator-triggered, never CI.
    pub fn full_weekend(scratch: &Path) -> Self {
        Self {
            duration: Duration::from_secs(72 * 60 * 60),
            tick: Duration::from_secs(10),
            audit_path: scratch.join("audit.log"),
            metrics_path: Some(scratch.join("metrics.jsonl")),
            workspace_root: scratch.join("workspaces"),
            max_concurrent_agents: 3,
            agent_label: "agent:claude".to_string(),
            faults: FaultSchedule::default_72h(),
            fixtures: seed_fixtures(),
            stuck_task_threshold: Duration::from_secs(30 * 60),
        }
    }
}

/// Aggregate summary returned at run end.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    /// Final metrics snapshot.
    pub final_snapshot: MetricsSnapshot,
    /// Outcome of every fault the schedule applied.
    pub fault_outcomes: Vec<FaultOutcome>,
    /// All invariant rows recorded during the run, flattened.
    pub invariant_rows: Vec<crate::invariants::InvariantRow>,
    /// Total ticks the runner executed.
    pub ticks: u64,
    /// Total wall-clock the run consumed.
    pub elapsed_seconds: u64,
}

impl RunSummary {
    /// `true` when:
    /// - the orchestrator produced at least one `Tick` audit event,
    /// - no invariant breach was recorded,
    /// - every applicable fault recovered (skipped is OK).
    pub fn passed(&self) -> bool {
        let snap = &self.final_snapshot;
        if snap.ticks_observed == 0 {
            return false;
        }
        if snap.invariant_breaches > 0 {
            return false;
        }
        for f in &self.fault_outcomes {
            if matches!(f.status, crate::faults::FaultStatus::DidNotRecover) {
                return false;
            }
        }
        true
    }
}

/// Errors raised by the runner.
#[derive(Debug, Error)]
pub enum RunnerError {
    /// The synthetic workflow file failed to parse. (Should not happen in
    /// practice — the body is constructed in code below.)
    #[error("workflow build: {0}")]
    Workflow(String),
    /// I/O failure setting up scratch directories.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Driver. Construct via [`HarnessRunner::new`] and call [`Self::run`].
pub struct HarnessRunner {
    config: HarnessConfig,
    sink: Arc<MetricsSink>,
    board: Arc<SyntheticBoard>,
    agent: Arc<SyntheticAgent>,
}

impl HarnessRunner {
    /// Wire up a runner against the supplied config.
    pub fn new(config: HarnessConfig) -> Result<Self, RunnerError> {
        std::fs::create_dir_all(&config.workspace_root)?;
        if let Some(parent) = config.audit_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let sink = MetricsSink::new(config.metrics_path.clone());
        let board = Arc::new(SyntheticBoard::with_fixtures(
            &config.fixtures,
            &config.agent_label,
        ));
        let agent = Arc::new(SyntheticAgent::new());

        Ok(Self {
            config,
            sink,
            board,
            agent,
        })
    }

    /// Synthetic-agent handle (for assertions in tests).
    pub fn agent(&self) -> Arc<SyntheticAgent> {
        Arc::clone(&self.agent)
    }

    /// Synthetic-board handle (for assertions in tests).
    pub fn board(&self) -> Arc<SyntheticBoard> {
        Arc::clone(&self.board)
    }

    /// Metrics sink (for snapshotting outside the run loop).
    pub fn sink(&self) -> Arc<MetricsSink> {
        Arc::clone(&self.sink)
    }

    /// Build the synthetic `WORKFLOW.md` body. Intentionally minimal: the
    /// soak harness only cares about Symphony's tick loop, not the prompt
    /// template's contents.
    fn synthetic_workflow_body(&self) -> String {
        // Front-matter values are chosen to match the harness's
        // expectations: short polling interval (we drive `tick()`
        // directly so this is mostly cosmetic), the configured
        // concurrency cap, and a label that matches our fixtures.
        format!(
            r#"---
tracker:
  api_key: "synthetic-key-not-used"
  project_slug: "soak"
  active_states: ["Todo", "In Progress"]
polling:
  interval_ms: 60000
agent:
  max_concurrent_agents: {cap}
  agent_label_required: "{label}"
  comment_on_completion: true
  handoff_state_on_success: "In Review"
  handoff_state_on_failure: "Backlog"
  stall_timeout_ms: 5000
  max_retry_backoff_ms: 60000
  max_retry_attempts: 2
workspace:
  root: "{root}"
hooks: {{}}
---
SOAK harness synthetic prompt for {{{{ issue.identifier }}}}: {{{{ issue.title }}}}.
"#,
            cap = self.config.max_concurrent_agents,
            label = self.config.agent_label,
            root = self.config.workspace_root.display(),
        )
    }

    /// Run to completion. Returns when the configured duration elapses,
    /// `shutdown` resolves, or all fixtures have completed AND the run
    /// has covered enough wall-clock to apply every scheduled fault.
    pub async fn run(self, mut shutdown: Option<oneshot::Receiver<()>>) -> Result<RunSummary, RunnerError> {
        let started = Instant::now();
        let workflow = WorkflowDefinition::from_str(&self.synthetic_workflow_body())
            .map_err(|e| RunnerError::Workflow(e.to_string()))?;

        // ---- build the orchestrator ----
        let workspaces = Arc::new(WorkspaceManager::new(
            self.config.workspace_root.clone(),
            workflow.config.hooks.clone(),
        ));
        let mut caps = HashMap::new();
        caps.insert(
            Provider::ClaudeCode,
            Cap {
                // Generous caps so the harness itself never trips a budget.
                monthly_micro_dollars: u64::MAX / 4,
                session_micro_dollars: u64::MAX / 4,
            },
        );
        let budget = Arc::new(Budget::new(caps));
        let mut router = Router::new(Arc::clone(&budget));
        router.register(AgentRegistration {
            agent: Arc::clone(&self.agent) as Arc<dyn orchestrator::Agent>,
            provider: Provider::ClaudeCode,
            estimated_micros_per_task: 1,
        });
        let router = Arc::new(router);

        let audit = Arc::new(AuditLog::open(self.config.audit_path.clone()));
        let board = Arc::clone(&self.board);
        let orch = Arc::new(Orchestrator::new(
            workflow,
            board.clone() as Arc<dyn IssueSource>,
            workspaces,
            router,
            audit,
        ));

        // ---- spawn audit-log tailer ----
        let sink_for_tail = Arc::clone(&self.sink);
        let audit_path = self.config.audit_path.clone();
        let (tailer_stop_tx, tailer_stop_rx) = oneshot::channel();
        let tailer = tokio::spawn(audit_tailer(audit_path.clone(), sink_for_tail, tailer_stop_rx));

        // ---- invariants ----
        let invariants = Invariants::new(
            audit_path.clone(),
            self.config.max_concurrent_agents,
            self.config.stuck_task_threshold,
        );

        // ---- main tick loop ----
        let mut pending_faults: Vec<ScheduledFault> = self.config.faults.iter().copied().collect();
        let mut fault_outcomes: Vec<FaultOutcome> = Vec::new();
        let mut invariant_rows: Vec<crate::invariants::InvariantRow> = Vec::new();
        let mut tick_idx: u64 = 0;
        let mut last_tick_at = Instant::now();

        loop {
            let elapsed = started.elapsed();

            // Apply any faults whose offset has matured.
            let mut fired_indices = Vec::new();
            for (idx, sf) in pending_faults.iter().enumerate() {
                if elapsed >= sf.offset {
                    let outcome = faults::apply(sf.fault, &board, &self.sink);
                    tracing::info!(
                        fault = sf.fault.tag(),
                        status = ?outcome.status,
                        "fault injected"
                    );
                    fault_outcomes.push(outcome);
                    fired_indices.push(idx);
                }
            }
            for idx in fired_indices.into_iter().rev() {
                pending_faults.remove(idx);
            }

            // Tick the orchestrator.
            if let Err(e) = orch.tick().await {
                tracing::warn!(error = %e, "harness tick failed; continuing");
            }

            // Allow spawned agent tasks to drain a moment so the audit
            // log catches up before we run invariants.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Run invariants on this tick.
            let snap = self.sink.snapshot(tick_idx);
            let report = invariants
                .check(tick_idx, &orch, &snap, &self.sink)
                .await;
            invariant_rows.extend(report.rows.iter().cloned());

            tick_idx += 1;

            // Check for early termination conditions.
            if elapsed >= self.config.duration {
                tracing::info!(elapsed_secs = elapsed.as_secs(), "duration reached; ending run");
                break;
            }

            if let Some(rx) = shutdown.as_mut() {
                if rx.try_recv().is_ok() {
                    tracing::info!("shutdown signal received; ending run");
                    break;
                }
            }

            // Sleep until the next tick boundary.
            let now = Instant::now();
            let next = last_tick_at + self.config.tick;
            if now < next {
                tokio::time::sleep(next - now).await;
            }
            last_tick_at = Instant::now();
        }

        // Drain any pending agent tasks for a brief moment so the final
        // snapshot reflects in-flight completions.
        let drain_deadline = Instant::now() + Duration::from_millis(500);
        loop {
            let (running, _completed) = orch.state_snapshot().await;
            if running.is_empty() || Instant::now() >= drain_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        // Stop the tailer and produce the final snapshot.
        let _ = tailer_stop_tx.send(());
        // Give it up to 250ms to flush.
        let _ = tokio::time::timeout(Duration::from_millis(250), tailer).await;

        // Final snapshot post-drain so completion counts reflect any
        // tasks that finished after the last tick boundary. If the loop
        // never executed even once this still returns a zero snapshot so
        // callers always get something useful back.
        let final_snapshot = self.sink.snapshot(tick_idx);

        Ok(RunSummary {
            final_snapshot,
            fault_outcomes,
            invariant_rows,
            ticks: tick_idx,
            elapsed_seconds: started.elapsed().as_secs(),
        })
    }
}

/// Tail the audit log line by line until `stop` resolves. Each parsed
/// line is fed into [`MetricsSink::ingest_audit`].
async fn audit_tailer(
    path: PathBuf,
    sink: Arc<MetricsSink>,
    mut stop: oneshot::Receiver<()>,
) {
    // Wait until the file appears (Symphony creates it on first write).
    for _ in 0..200 {
        if path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "tailer failed to open audit log");
            return;
        }
    };
    let _ = f.seek(SeekFrom::Start(0)).await;
    let mut reader = BufReader::new(f);
    let mut line = String::new();
    loop {
        if stop.try_recv().is_ok() {
            break;
        }
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<symphony::audit::AuditEvent>(trimmed) {
                    Ok(ev) => sink.ingest_audit(&ev),
                    Err(e) => {
                        tracing::trace!(error = %e, "audit line parse failed (skipping)");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "audit tailer read error");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{BehaviorTag, FixtureIssue};

    fn tiny_fixtures() -> Vec<FixtureIssue> {
        vec![
            FixtureIssue { seq: 1, tag: BehaviorTag::HappyFast, title: "h1".into() },
            FixtureIssue { seq: 2, tag: BehaviorTag::Failing, title: "f1".into() },
            FixtureIssue { seq: 3, tag: BehaviorTag::RequestTestDeletion, title: "td".into() },
        ]
    }

    #[tokio::test]
    async fn runner_executes_at_least_one_tick_and_writes_metrics() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HarnessConfig {
            duration: Duration::from_millis(800),
            tick: Duration::from_millis(100),
            audit_path: dir.path().join("audit.log"),
            metrics_path: Some(dir.path().join("metrics.jsonl")),
            workspace_root: dir.path().join("workspaces"),
            max_concurrent_agents: 2,
            agent_label: "agent:claude".into(),
            faults: FaultSchedule::empty(),
            fixtures: tiny_fixtures(),
            stuck_task_threshold: Duration::from_secs(30),
        };
        let runner = HarnessRunner::new(cfg).expect("ctor ok");
        let summary = runner.run(None).await.expect("run ok");

        assert!(summary.ticks > 0);
        assert!(summary.final_snapshot.ticks_observed > 0);
        assert!(summary.final_snapshot.tasks_dispatched > 0);
        // The metrics JSONL file should exist with at least one line.
        let body = std::fs::read_to_string(dir.path().join("metrics.jsonl")).unwrap();
        assert!(body.lines().count() > 0);
    }

    #[tokio::test]
    async fn runner_records_fault_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HarnessConfig {
            duration: Duration::from_secs(2),
            tick: Duration::from_millis(150),
            audit_path: dir.path().join("audit.log"),
            metrics_path: None,
            workspace_root: dir.path().join("workspaces"),
            max_concurrent_agents: 2,
            agent_label: "agent:claude".into(),
            faults: FaultSchedule::default_ci_smoke(),
            fixtures: tiny_fixtures(),
            stuck_task_threshold: Duration::from_secs(30),
        };
        let runner = HarnessRunner::new(cfg).expect("ctor ok");
        let summary = runner.run(None).await.expect("run ok");

        // tracker_outage should fire and be marked Recovered; the others
        // are Skipped on this branch (no upstream wiring).
        let tags: Vec<&str> = summary.fault_outcomes.iter().map(|f| f.fault.as_str()).collect();
        assert!(tags.contains(&"tracker_outage"));
        let recovered = summary
            .fault_outcomes
            .iter()
            .filter(|f| matches!(f.status, crate::faults::FaultStatus::Recovered))
            .count();
        assert!(recovered >= 1);
    }

    #[tokio::test]
    async fn runner_observes_test_deletion_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HarnessConfig {
            duration: Duration::from_millis(1500),
            tick: Duration::from_millis(150),
            audit_path: dir.path().join("audit.log"),
            metrics_path: None,
            workspace_root: dir.path().join("workspaces"),
            max_concurrent_agents: 3,
            agent_label: "agent:claude".into(),
            faults: FaultSchedule::empty(),
            fixtures: vec![FixtureIssue {
                seq: 1,
                tag: BehaviorTag::RequestTestDeletion,
                title: "tries to delete tests/".into(),
            }],
            stuck_task_threshold: Duration::from_secs(30),
        };
        let runner = HarnessRunner::new(cfg).expect("ctor ok");
        let agent = runner.agent();
        let summary = runner.run(None).await.expect("run ok");
        assert!(agent.test_deletion_attempts() >= 1);
        assert!(summary.final_snapshot.test_deletion_attempts >= 1);
    }
}
