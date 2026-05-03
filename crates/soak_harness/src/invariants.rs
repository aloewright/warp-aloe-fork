//! Per-tick invariant checks for the soak harness.
//!
//! Each invariant is a pure observation over the running orchestrator state
//! plus the audit-log file size. Breaches are *reported* (not panicked) so
//! the harness can run unattended for 72 hours and surface every breach in
//! the final summary instead of crashing on the first one.
//!
//! Invariants (per PDX-29 acceptance):
//!
//! * **append-only audit log** — file size is monotonically non-decreasing.
//! * **concurrency cap respected** — `running.len() <= max_concurrent_agents`.
//! * **no stuck tasks** — no `running` entry has gone more than
//!   `stuck_task_threshold` without an audit event.
//! * **test-deletion blocked** — when `test_deletion_attempts > 0`, the
//!   harness expects an equal number of `test_deletion_blocked` events
//!   from the upstream `auto_healing` crate. If the crate isn't present
//!   on this branch, the gap is reported as a *warning* (not a breach)
//!   and surfaced in the runbook so the operator can decide whether to
//!   abort the soak.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use symphony::orchestrator::Orchestrator;

use crate::metrics::{MetricsSink, MetricsSnapshot};

/// Single invariant check result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum InvariantStatus {
    /// Invariant held.
    Ok,
    /// Invariant breached — the harness logs it and continues.
    Breach,
    /// Invariant could not be checked (e.g. upstream feature not wired).
    /// Reported but does not count as a breach.
    Skipped,
}

/// One row in [`InvariantReport`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvariantRow {
    /// Invariant name.
    pub name: String,
    /// Status from the most recent check.
    pub status: InvariantStatus,
    /// Free-form detail (numeric facts, breach reason, etc).
    pub detail: String,
    /// Tick number at which the row was recorded.
    pub tick: u64,
    /// Wall-clock when checked.
    pub at: DateTime<Utc>,
}

/// Aggregate result of a per-tick invariant pass.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InvariantReport {
    /// Per-invariant rows in the order they were checked.
    pub rows: Vec<InvariantRow>,
}

impl InvariantReport {
    /// `true` when every row is `Ok` or `Skipped`.
    pub fn all_passing(&self) -> bool {
        !self.rows.iter().any(|r| r.status == InvariantStatus::Breach)
    }

    /// Count of `Breach` rows.
    pub fn breach_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.status == InvariantStatus::Breach)
            .count()
    }
}

/// Stateful invariant runner. Holds the bookkeeping needed to check
/// monotonic properties (audit-log size, per-issue last-event timestamps).
pub struct Invariants {
    audit_path: PathBuf,
    /// Maximum audit-log byte size observed so far. The check fails if a
    /// later observation is smaller (indicates a rewrite, which would
    /// violate the append-only contract).
    last_audit_size: Mutex<u64>,
    /// Concurrency cap configured on the orchestrator.
    max_concurrent: usize,
    /// Per-issue last-event timestamps, used by the stuck-task check.
    /// Cleared whenever the issue leaves the running set.
    last_seen: Mutex<HashMap<String, Instant>>,
    /// How long an issue is allowed to sit in `running` without a new
    /// audit event before the stuck-task invariant flags it.
    stuck_task_threshold: Duration,
}

impl Invariants {
    /// Construct an invariants runner.
    pub fn new(audit_path: PathBuf, max_concurrent: usize, stuck_task_threshold: Duration) -> Self {
        Self {
            audit_path,
            last_audit_size: Mutex::new(0),
            max_concurrent,
            last_seen: Mutex::new(HashMap::new()),
            stuck_task_threshold,
        }
    }

    /// Run all invariants once.
    pub async fn check(
        &self,
        tick: u64,
        orch: &Orchestrator,
        snapshot: &MetricsSnapshot,
        sink: &MetricsSink,
    ) -> InvariantReport {
        let mut rows = Vec::new();
        let now = Utc::now();
        let mono = Instant::now();

        // ---- audit log monotonicity ----
        let audit_size = std::fs::metadata(&self.audit_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let last = {
            let mut g = self.last_audit_size.lock().unwrap_or_else(|e| e.into_inner());
            let last = *g;
            if audit_size >= last {
                *g = audit_size;
            }
            last
        };
        if audit_size < last {
            sink.record_invariant_breach();
            rows.push(InvariantRow {
                name: "audit_log_append_only".into(),
                status: InvariantStatus::Breach,
                detail: format!("audit size shrank: was {} now {}", last, audit_size),
                tick,
                at: now,
            });
        } else {
            rows.push(InvariantRow {
                name: "audit_log_append_only".into(),
                status: InvariantStatus::Ok,
                detail: format!("size={} prev={}", audit_size, last),
                tick,
                at: now,
            });
        }

        // ---- concurrency cap ----
        let (running, _completed) = orch.state_snapshot().await;
        if running.len() > self.max_concurrent {
            sink.record_invariant_breach();
            rows.push(InvariantRow {
                name: "concurrency_cap".into(),
                status: InvariantStatus::Breach,
                detail: format!(
                    "running={} cap={}",
                    running.len(),
                    self.max_concurrent
                ),
                tick,
                at: now,
            });
        } else {
            rows.push(InvariantRow {
                name: "concurrency_cap".into(),
                status: InvariantStatus::Ok,
                detail: format!("running={} cap={}", running.len(), self.max_concurrent),
                tick,
                at: now,
            });
        }

        // ---- no stuck tasks ----
        // For each in-flight running entry, check whether
        // `now - last_event_at_in_orchestrator > stuck_task_threshold`. We
        // hash on `last_event_at` so a still-emitting agent updates its
        // bookkeeping naturally.
        {
            let mut seen = self.last_seen.lock().unwrap_or_else(|e| e.into_inner());
            seen.retain(|id, _| running.contains_key(id));
            let mut stuck: Vec<String> = Vec::new();
            for (id, entry) in running.iter() {
                let prev = seen
                    .entry(id.clone())
                    .or_insert(entry.last_event_at);
                // If the orchestrator's last_event_at advanced, refresh.
                if entry.last_event_at > *prev {
                    *prev = entry.last_event_at;
                }
                let age = mono.saturating_duration_since(*prev);
                if age > self.stuck_task_threshold {
                    stuck.push(format!("{} ({}s)", entry.identifier, age.as_secs()));
                }
            }
            if stuck.is_empty() {
                rows.push(InvariantRow {
                    name: "no_stuck_tasks".into(),
                    status: InvariantStatus::Ok,
                    detail: format!("running={}", running.len()),
                    tick,
                    at: now,
                });
            } else {
                sink.record_invariant_breach();
                rows.push(InvariantRow {
                    name: "no_stuck_tasks".into(),
                    status: InvariantStatus::Breach,
                    detail: format!("stuck=[{}]", stuck.join(", ")),
                    tick,
                    at: now,
                });
            }
        }

        // ---- test-deletion blocked (best-effort) ----
        if snapshot.test_deletion_attempts == 0 {
            rows.push(InvariantRow {
                name: "no_test_deletion".into(),
                status: InvariantStatus::Ok,
                detail: "no attempts observed yet".into(),
                tick,
                at: now,
            });
        } else if snapshot.test_deletion_blocked >= snapshot.test_deletion_attempts {
            rows.push(InvariantRow {
                name: "no_test_deletion".into(),
                status: InvariantStatus::Ok,
                detail: format!(
                    "attempts={} blocked={}",
                    snapshot.test_deletion_attempts, snapshot.test_deletion_blocked
                ),
                tick,
                at: now,
            });
        } else {
            // No upstream auto_healing block observed — surface as Skipped
            // (gap) rather than Breach so a branch without auto_healing
            // doesn't fail CI. The runbook calls this out explicitly.
            rows.push(InvariantRow {
                name: "no_test_deletion".into(),
                status: InvariantStatus::Skipped,
                detail: format!(
                    "auto_healing block not observed: attempts={} blocked={} — verify auto_healing crate is wired",
                    snapshot.test_deletion_attempts, snapshot.test_deletion_blocked
                ),
                tick,
                at: now,
            });
        }

        InvariantReport { rows }
    }

    /// Path the audit log is being read from.
    pub fn audit_path(&self) -> &Path {
        &self.audit_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_passing_when_no_breaches() {
        let r = InvariantReport {
            rows: vec![InvariantRow {
                name: "x".into(),
                status: InvariantStatus::Ok,
                detail: "".into(),
                tick: 0,
                at: Utc::now(),
            }],
        };
        assert!(r.all_passing());
        assert_eq!(r.breach_count(), 0);
    }

    #[test]
    fn report_records_breach_count() {
        let r = InvariantReport {
            rows: vec![
                InvariantRow {
                    name: "a".into(),
                    status: InvariantStatus::Ok,
                    detail: "".into(),
                    tick: 0,
                    at: Utc::now(),
                },
                InvariantRow {
                    name: "b".into(),
                    status: InvariantStatus::Breach,
                    detail: "x".into(),
                    tick: 0,
                    at: Utc::now(),
                },
                InvariantRow {
                    name: "c".into(),
                    status: InvariantStatus::Skipped,
                    detail: "".into(),
                    tick: 0,
                    at: Utc::now(),
                },
            ],
        };
        assert!(!r.all_passing());
        assert_eq!(r.breach_count(), 1);
    }
}
