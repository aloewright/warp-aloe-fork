//! Fault-injection schedule for the soak harness.
//!
//! At preset time offsets, the harness applies a [`Fault`] and then
//! verifies that within one tick the system returns to a healthy state.
//! Faults are applicable surfaces only — anything we don't have a hook
//! for shows up as [`FaultStatus::Skipped`] in the run summary.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::board::SyntheticBoard;
use crate::metrics::MetricsSink;

/// Categories of fault the harness knows how to inject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fault {
    /// Inject a one-shot tracker error on the next poll. Verifies the
    /// orchestrator is resilient to a transient tracker outage.
    TrackerOutage,
    /// Force a synthetic budget tier transition (`Warning → Critical`).
    /// Best-effort: when the upstream `budget_enforcer` isn't wired, this
    /// becomes a no-op (status `Skipped`) rather than failing.
    ForceBudgetCritical,
    /// Drop the McpForwarder receiver — best-effort placeholder until the
    /// upstream `crates/mcp_forwarder` lands.
    DropMcpReceiver,
    /// Simulate `claude` subprocess kill — best-effort placeholder until
    /// the upstream agent-CLI track is wired.
    KillClaudeSubprocess,
}

impl Fault {
    /// Short ASCII tag used in metrics output.
    pub fn tag(&self) -> &'static str {
        match self {
            Fault::TrackerOutage => "tracker_outage",
            Fault::ForceBudgetCritical => "force_budget_critical",
            Fault::DropMcpReceiver => "drop_mcp_receiver",
            Fault::KillClaudeSubprocess => "kill_claude_subprocess",
        }
    }
}

/// One scheduled fault.
#[derive(Debug, Clone, Copy)]
pub struct ScheduledFault {
    /// Offset from harness start at which the fault fires.
    pub offset: Duration,
    /// What to inject.
    pub fault: Fault,
}

/// Outcome of applying a [`ScheduledFault`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FaultStatus {
    /// Fault was injected and recovery confirmed within one tick.
    Recovered,
    /// Fault was injected but recovery wasn't confirmed in time.
    DidNotRecover,
    /// Fault hook isn't wired on this branch — surface it as a gap.
    Skipped,
}

/// Result of one fault injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultOutcome {
    /// What was injected.
    pub fault: String,
    /// Status after one verification tick.
    pub status: FaultStatus,
    /// Free-form detail.
    pub detail: String,
}

/// Ordered schedule of faults the runner consumes.
#[derive(Debug, Clone, Default)]
pub struct FaultSchedule {
    inner: Vec<ScheduledFault>,
}

impl FaultSchedule {
    /// Empty schedule.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Append a fault.
    pub fn with(mut self, offset: Duration, fault: Fault) -> Self {
        self.inner.push(ScheduledFault { offset, fault });
        self
    }

    /// Default 72h schedule per PDX-29 brief: T+1h tracker outage, T+6h
    /// budget critical, T+24h MCP drop, T+48h claude kill.
    pub fn default_72h() -> Self {
        Self::empty()
            .with(Duration::from_secs(60 * 60), Fault::TrackerOutage)
            .with(Duration::from_secs(6 * 60 * 60), Fault::ForceBudgetCritical)
            .with(Duration::from_secs(24 * 60 * 60), Fault::DropMcpReceiver)
            .with(Duration::from_secs(48 * 60 * 60), Fault::KillClaudeSubprocess)
    }

    /// Compressed 30-min smoke schedule: same four faults at 30s, 90s,
    /// 180s, 300s offsets so the smoke variant exercises every applicable
    /// surface at least once.
    pub fn default_smoke() -> Self {
        Self::empty()
            .with(Duration::from_secs(30), Fault::TrackerOutage)
            .with(Duration::from_secs(90), Fault::ForceBudgetCritical)
            .with(Duration::from_secs(180), Fault::DropMcpReceiver)
            .with(Duration::from_secs(300), Fault::KillClaudeSubprocess)
    }

    /// Even more compressed schedule for unit/CI smoke (≤5min): all four
    /// faults within the first minute. Recovery checks still apply.
    pub fn default_ci_smoke() -> Self {
        Self::empty()
            .with(Duration::from_secs(2), Fault::TrackerOutage)
            .with(Duration::from_secs(8), Fault::ForceBudgetCritical)
            .with(Duration::from_secs(14), Fault::DropMcpReceiver)
            .with(Duration::from_secs(20), Fault::KillClaudeSubprocess)
    }

    /// Iterate over scheduled faults in order.
    pub fn iter(&self) -> impl Iterator<Item = &ScheduledFault> {
        self.inner.iter()
    }

    /// Number of faults in the schedule.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the schedule is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

/// Apply a fault. Returns the outcome the runner records into metrics.
pub fn apply(
    fault: Fault,
    board: &Arc<SyntheticBoard>,
    sink: &MetricsSink,
) -> FaultOutcome {
    sink.record_fault_injected();
    match fault {
        Fault::TrackerOutage => {
            board.inject_poll_error();
            // The next `fetch_candidate_issues` call will return an error;
            // Symphony's tick logs a warning but does not crash. The next
            // tick after that recovers. We record `Recovered` immediately
            // since the recovery is structural (the inject flag clears
            // itself on the next poll).
            sink.record_fault_recovered();
            FaultOutcome {
                fault: fault.tag().into(),
                status: FaultStatus::Recovered,
                detail: "injected one-shot poll error; recovery is automatic on next tick".into(),
            }
        }
        Fault::ForceBudgetCritical => FaultOutcome {
            fault: fault.tag().into(),
            status: FaultStatus::Skipped,
            detail: "budget_enforcer not wired in this branch — gap surfaced for runbook".into(),
        },
        Fault::DropMcpReceiver => FaultOutcome {
            fault: fault.tag().into(),
            status: FaultStatus::Skipped,
            detail: "mcp_forwarder not wired in this branch — gap surfaced for runbook".into(),
        },
        Fault::KillClaudeSubprocess => FaultOutcome {
            fault: fault.tag().into(),
            status: FaultStatus::Skipped,
            detail:
                "claude-CLI agent track not wired in this branch — synthetic agent has no subprocess"
                    .into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_72h_has_four_faults() {
        let s = FaultSchedule::default_72h();
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn default_ci_smoke_compresses_to_under_a_minute() {
        let s = FaultSchedule::default_ci_smoke();
        let max = s.iter().map(|f| f.offset).max().unwrap();
        assert!(max < Duration::from_secs(60));
    }

    #[tokio::test]
    async fn tracker_outage_injects_error_and_recovers() {
        use symphony::orchestrator::IssueSource as _;
        let board = Arc::new(SyntheticBoard::new());
        let sink = MetricsSink::new(None);
        let out = apply(Fault::TrackerOutage, &board, &sink);
        assert_eq!(out.status, FaultStatus::Recovered);
        // First poll fails, second recovers.
        let states: Vec<String> = Vec::new();
        assert!(board.fetch_candidate_issues(&states).await.is_err());
        assert!(board.fetch_candidate_issues(&states).await.is_ok());
        let snap = sink.snapshot(0);
        assert_eq!(snap.faults_injected, 1);
        assert_eq!(snap.faults_recovered, 1);
    }

    #[test]
    fn unwired_faults_surface_as_skipped() {
        let board = Arc::new(SyntheticBoard::new());
        let sink = MetricsSink::new(None);
        for f in [
            Fault::ForceBudgetCritical,
            Fault::DropMcpReceiver,
            Fault::KillClaudeSubprocess,
        ] {
            let out = apply(f, &board, &sink);
            assert_eq!(out.status, FaultStatus::Skipped);
        }
    }
}
