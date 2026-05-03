//! Metrics aggregation for the soak harness.
//!
//! Writes a single JSONL stream of [`MetricsSnapshot`]s, one per harness
//! tick. The same snapshot is also returned at run end as part of
//! [`crate::RunSummary`]. Atomic counters are bumped from the audit-log
//! tailer; emitting a snapshot is a copy-out, not a reset.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use symphony::audit::{AuditEvent, AuditEventKind};

/// Snapshot of harness counters at one tick boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsSnapshot {
    /// Wall-clock when the snapshot was emitted.
    pub at: DateTime<Utc>,
    /// Tick sequence number (starts at 0).
    pub tick: u64,
    /// Total `Tick` audit events observed since start.
    pub ticks_observed: u64,
    /// Total `Claimed` audit events observed.
    pub tasks_claimed: u64,
    /// Total `Dispatched` audit events observed.
    pub tasks_dispatched: u64,
    /// Total `Completed` audit events observed.
    pub tasks_completed: u64,
    /// Total `Failed` audit events observed.
    pub tasks_failed: u64,
    /// Total `Stalled` audit events observed.
    pub tasks_stalled: u64,
    /// Total `RetryScheduled` audit events observed.
    pub retries_scheduled: u64,
    /// Total `RetryDispatched` audit events observed.
    pub retries_dispatched: u64,
    /// Total `RetryGivenUp` audit events observed.
    pub retries_given_up: u64,
    /// Total `DiffGuardExceeded` audit events observed.
    pub diff_guard_exceeded: u64,
    /// Test-deletion attempts observed via the `delete_file` ToolCall.
    pub test_deletion_attempts: u64,
    /// Test-deletion blocks observed via the harness's auto-healing
    /// marker. When the upstream `auto_healing` crate isn't present, this
    /// stays at 0 and the harness reports the gap rather than failing CI.
    pub test_deletion_blocked: u64,
    /// Budget tier transitions observed (`Normal → Warning → Critical`).
    /// When the upstream `budget_enforcer` isn't wired, stays 0.
    pub budget_tier_transitions: u64,
    /// Faults injected so far.
    pub faults_injected: u64,
    /// Faults the harness verified the system recovered from.
    pub faults_recovered: u64,
    /// Total invariant breaches observed across the run.
    pub invariant_breaches: u64,
}

impl MetricsSnapshot {
    /// `completed / dispatched`, or `0.0` when no dispatches occurred.
    pub fn completion_ratio(&self) -> f32 {
        if self.tasks_dispatched == 0 {
            return 0.0;
        }
        self.tasks_completed as f32 / self.tasks_dispatched as f32
    }
}

/// Atomic-counter metrics sink. Cheap to clone behind an `Arc`.
pub struct MetricsSink {
    ticks_observed: AtomicU64,
    tasks_claimed: AtomicU64,
    tasks_dispatched: AtomicU64,
    tasks_completed: AtomicU64,
    tasks_failed: AtomicU64,
    tasks_stalled: AtomicU64,
    retries_scheduled: AtomicU64,
    retries_dispatched: AtomicU64,
    retries_given_up: AtomicU64,
    diff_guard_exceeded: AtomicU64,
    test_deletion_attempts: AtomicU64,
    test_deletion_blocked: AtomicU64,
    budget_tier_transitions: AtomicU64,
    faults_injected: AtomicU64,
    faults_recovered: AtomicU64,
    invariant_breaches: AtomicU64,
    out_path: Option<PathBuf>,
    out_file: Mutex<Option<std::fs::File>>,
}

impl MetricsSink {
    /// Construct a new sink. If `out_path` is `Some`, snapshots are also
    /// appended as JSONL to that file. I/O failures are logged through
    /// `tracing` and never panic — observability is non-load-bearing.
    pub fn new(out_path: Option<PathBuf>) -> Arc<Self> {
        let file = out_path.as_ref().and_then(|p| {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .map_err(|e| {
                    tracing::warn!(error = %e, path = %p.display(), "failed to open metrics file");
                    e
                })
                .ok()
        });
        Arc::new(Self {
            ticks_observed: AtomicU64::new(0),
            tasks_claimed: AtomicU64::new(0),
            tasks_dispatched: AtomicU64::new(0),
            tasks_completed: AtomicU64::new(0),
            tasks_failed: AtomicU64::new(0),
            tasks_stalled: AtomicU64::new(0),
            retries_scheduled: AtomicU64::new(0),
            retries_dispatched: AtomicU64::new(0),
            retries_given_up: AtomicU64::new(0),
            diff_guard_exceeded: AtomicU64::new(0),
            test_deletion_attempts: AtomicU64::new(0),
            test_deletion_blocked: AtomicU64::new(0),
            budget_tier_transitions: AtomicU64::new(0),
            faults_injected: AtomicU64::new(0),
            faults_recovered: AtomicU64::new(0),
            invariant_breaches: AtomicU64::new(0),
            out_path,
            out_file: Mutex::new(file),
        })
    }

    /// Classify an audit event and bump the matching counter(s).
    pub fn ingest_audit(&self, event: &AuditEvent) {
        match event.kind {
            AuditEventKind::Tick => {
                self.ticks_observed.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::Claimed => {
                self.tasks_claimed.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::Dispatched => {
                self.tasks_dispatched.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::Completed => {
                self.tasks_completed.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::Failed => {
                self.tasks_failed.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::Stalled => {
                self.tasks_stalled.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::RetryScheduled => {
                self.retries_scheduled.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::RetryDispatched => {
                self.retries_dispatched.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::RetryGivenUp => {
                self.retries_given_up.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::DiffGuardExceeded => {
                self.diff_guard_exceeded.fetch_add(1, Ordering::Relaxed);
            }
            AuditEventKind::ToolCall => {
                if event.message.as_deref() == Some("delete_file") {
                    self.test_deletion_attempts.fetch_add(1, Ordering::Relaxed);
                }
            }
            AuditEventKind::Chunk
            | AuditEventKind::ToolResult => {
                // Scan for upstream auto_healing/budget markers. When the
                // crates aren't present, no message will match; counters
                // stay at 0 and the runbook calls the gap out as expected.
                if let Some(msg) = &event.message {
                    if msg.contains("[AUTO_HEALING][BLOCKED]") {
                        self.test_deletion_blocked.fetch_add(1, Ordering::Relaxed);
                    }
                    if msg.contains("[BUDGET][TIER]") {
                        self.budget_tier_transitions.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    /// Bump the fault-injection counter.
    pub fn record_fault_injected(&self) {
        self.faults_injected.fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the fault-recovery counter.
    pub fn record_fault_recovered(&self) {
        self.faults_recovered.fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the invariant-breach counter.
    pub fn record_invariant_breach(&self) {
        self.invariant_breaches.fetch_add(1, Ordering::Relaxed);
    }

    /// Snapshot the counters and write a JSONL line to the configured
    /// output file (if any).
    pub fn snapshot(&self, tick: u64) -> MetricsSnapshot {
        let snap = MetricsSnapshot {
            at: Utc::now(),
            tick,
            ticks_observed: self.ticks_observed.load(Ordering::Relaxed),
            tasks_claimed: self.tasks_claimed.load(Ordering::Relaxed),
            tasks_dispatched: self.tasks_dispatched.load(Ordering::Relaxed),
            tasks_completed: self.tasks_completed.load(Ordering::Relaxed),
            tasks_failed: self.tasks_failed.load(Ordering::Relaxed),
            tasks_stalled: self.tasks_stalled.load(Ordering::Relaxed),
            retries_scheduled: self.retries_scheduled.load(Ordering::Relaxed),
            retries_dispatched: self.retries_dispatched.load(Ordering::Relaxed),
            retries_given_up: self.retries_given_up.load(Ordering::Relaxed),
            diff_guard_exceeded: self.diff_guard_exceeded.load(Ordering::Relaxed),
            test_deletion_attempts: self.test_deletion_attempts.load(Ordering::Relaxed),
            test_deletion_blocked: self.test_deletion_blocked.load(Ordering::Relaxed),
            budget_tier_transitions: self.budget_tier_transitions.load(Ordering::Relaxed),
            faults_injected: self.faults_injected.load(Ordering::Relaxed),
            faults_recovered: self.faults_recovered.load(Ordering::Relaxed),
            invariant_breaches: self.invariant_breaches.load(Ordering::Relaxed),
        };

        // Best-effort JSONL write — never panic.
        if let Ok(line) = serde_json::to_string(&snap) {
            if let Ok(mut g) = self.out_file.lock() {
                if let Some(f) = g.as_mut() {
                    use std::io::Write;
                    if let Err(e) = writeln!(f, "{}", line) {
                        tracing::warn!(error = %e, "failed to write metrics line");
                    }
                }
            }
        }

        snap
    }

    /// Path the sink is writing to, if any.
    pub fn out_path(&self) -> Option<&Path> {
        self.out_path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: AuditEventKind) -> AuditEvent {
        AuditEvent::new(kind)
    }

    #[test]
    fn ingest_classifies_basic_kinds() {
        let sink = MetricsSink::new(None);
        sink.ingest_audit(&ev(AuditEventKind::Tick));
        sink.ingest_audit(&ev(AuditEventKind::Claimed));
        sink.ingest_audit(&ev(AuditEventKind::Dispatched));
        sink.ingest_audit(&ev(AuditEventKind::Completed));
        sink.ingest_audit(&ev(AuditEventKind::Failed));
        sink.ingest_audit(&ev(AuditEventKind::Stalled));
        sink.ingest_audit(&ev(AuditEventKind::DiffGuardExceeded));
        let s = sink.snapshot(0);
        assert_eq!(s.ticks_observed, 1);
        assert_eq!(s.tasks_claimed, 1);
        assert_eq!(s.tasks_dispatched, 1);
        assert_eq!(s.tasks_completed, 1);
        assert_eq!(s.tasks_failed, 1);
        assert_eq!(s.tasks_stalled, 1);
        assert_eq!(s.diff_guard_exceeded, 1);
    }

    #[test]
    fn tool_call_classifies_test_deletion() {
        let sink = MetricsSink::new(None);
        let mut e = ev(AuditEventKind::ToolCall);
        e.message = Some("delete_file".into());
        sink.ingest_audit(&e);
        let s = sink.snapshot(0);
        assert_eq!(s.test_deletion_attempts, 1);
        assert_eq!(s.test_deletion_blocked, 0);
    }

    #[test]
    fn auto_healing_marker_classifies_block() {
        let sink = MetricsSink::new(None);
        let mut e = ev(AuditEventKind::Chunk);
        e.message = Some("[AUTO_HEALING][BLOCKED] deleted tests/foo.rs".into());
        sink.ingest_audit(&e);
        let s = sink.snapshot(0);
        assert_eq!(s.test_deletion_blocked, 1);
    }

    #[test]
    fn budget_tier_marker_increments() {
        let sink = MetricsSink::new(None);
        let mut e = ev(AuditEventKind::Chunk);
        e.message = Some("[BUDGET][TIER] Warning -> Critical".into());
        sink.ingest_audit(&e);
        let s = sink.snapshot(0);
        assert_eq!(s.budget_tier_transitions, 1);
    }

    #[test]
    fn completion_ratio_handles_no_dispatch() {
        let s = MetricsSnapshot {
            at: Utc::now(),
            tick: 0,
            ticks_observed: 0,
            tasks_claimed: 0,
            tasks_dispatched: 0,
            tasks_completed: 0,
            tasks_failed: 0,
            tasks_stalled: 0,
            retries_scheduled: 0,
            retries_dispatched: 0,
            retries_given_up: 0,
            diff_guard_exceeded: 0,
            test_deletion_attempts: 0,
            test_deletion_blocked: 0,
            budget_tier_transitions: 0,
            faults_injected: 0,
            faults_recovered: 0,
            invariant_breaches: 0,
        };
        assert_eq!(s.completion_ratio(), 0.0);
    }

    #[test]
    fn snapshot_writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.jsonl");
        let sink = MetricsSink::new(Some(p.clone()));
        sink.ingest_audit(&ev(AuditEventKind::Tick));
        sink.snapshot(0);
        sink.snapshot(1);
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let _: MetricsSnapshot = serde_json::from_str(line).unwrap();
        }
    }
}
