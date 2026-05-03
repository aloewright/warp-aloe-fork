//! Symphony soak-test harness (PDX-29 [D6] 24/7 weekend test).
//!
//! Builds reproducible infrastructure on top of the live `symphony` crate to
//! validate the orchestrator's "always-on" promise:
//!
//! 1. A [`SyntheticBoard`] mimics Linear's polling surface so we don't pollute
//!    a real Linear project during a 72-hour run.
//! 2. A [`SyntheticAgent`] obeys per-issue [`BehaviorTag`]s — happy path,
//!    fail, stall, refuse, big-diff, attempt-test-deletion — so a single soak
//!    exercises every guardrail surface.
//! 3. A [`HarnessRunner`] drives `Orchestrator::tick`, tails the audit log
//!    for `[WATCHDOG][AUDIT]` markers, classifies events into a JSONL metrics
//!    stream, and asserts invariants every tick.
//! 4. A [`FaultSchedule`] injects faults at configured offsets (kill-claude,
//!    drop-receiver, force-budget-critical) so we can verify the system
//!    recovers within one tick.
//!
//! The harness is **observe-only** with respect to the upstream crates
//! (`symphony`, `orchestrator`, `auto_healing`, `cloud_*`): it consumes the
//! `IssueSource` / `Agent` traits and never reaches into private state.
//!
//! Native-only — soak runs against a local Symphony daemon, not WASM.

#![cfg(not(target_family = "wasm"))]
#![deny(missing_docs)]

pub mod board;
pub mod faults;
pub mod fixtures;
pub mod invariants;
pub mod metrics;
pub mod runner;
pub mod synthetic_agent;

pub use board::SyntheticBoard;
pub use faults::{Fault, FaultSchedule};
pub use fixtures::{seed_fixtures, BehaviorTag, FixtureIssue};
pub use invariants::{InvariantReport, Invariants};
pub use metrics::{MetricsSnapshot, MetricsSink};
pub use runner::{HarnessConfig, HarnessRunner, RunSummary};
pub use synthetic_agent::SyntheticAgent;
