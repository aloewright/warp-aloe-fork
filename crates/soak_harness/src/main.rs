//! `soak_harness` binary entry point.
//!
//! The harness boots a real Symphony orchestrator wired to a synthetic
//! Linear-shaped board and a deterministic synthetic agent, then drives
//! the tick loop for the configured duration. The acceptance criteria
//! for PDX-29 are implemented in `crates/soak_harness/src/{invariants,
//! metrics,faults}.rs`.
//!
//! Typical operator invocations:
//!
//! ```text
//! # 5-minute CI smoke (also runs in `cargo test -p soak_harness`).
//! cargo run --bin soak_harness -- --duration 5m --tick 1s
//!
//! # 30-minute "smoke soak" — gate for the full run.
//! cargo run --bin soak_harness -- --profile thirty-minute-smoke
//!
//! # Full 72-hour weekend test (operator-triggered).
//! nohup cargo run --release --bin soak_harness -- \
//!     --profile full-weekend \
//!     --metrics-out ~/.warp/symphony/soak-metrics.jsonl \
//!     > ~/.warp/symphony/soak.log 2>&1 &
//! ```

#![cfg(not(target_family = "wasm"))]

use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use soak_harness::{HarnessConfig, HarnessRunner};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "soak_harness", about = "PDX-29 Symphony soak-test driver")]
struct Cli {
    /// One of the named harness profiles. When supplied, individual
    /// `--duration`/`--tick`/etc flags are ignored.
    #[arg(long, value_enum)]
    profile: Option<Profile>,

    /// Total run duration (e.g. `5m`, `72h`). Default: 5m.
    #[arg(long, default_value = "5m", value_parser = parse_duration)]
    duration: Duration,

    /// Tick cadence (e.g. `1s`, `10s`). Default: 1s.
    #[arg(long, default_value = "1s", value_parser = parse_duration)]
    tick: Duration,

    /// Where the synthetic Symphony audit log goes. Defaults to a fresh
    /// path under `$TMPDIR/soak-harness-{pid}/audit.log`.
    #[arg(long)]
    audit_log: Option<PathBuf>,

    /// Where the JSONL metrics stream goes. Defaults to `metrics.jsonl`
    /// alongside `--audit-log`.
    #[arg(long)]
    metrics_out: Option<PathBuf>,

    /// Workspace root for per-issue scratch directories. Defaults to a
    /// fresh path under `$TMPDIR/soak-harness-{pid}/workspaces`.
    #[arg(long)]
    workspace_root: Option<PathBuf>,

    /// Concurrent agent cap. Default: 3.
    #[arg(long, default_value_t = 3)]
    max_concurrent_agents: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Profile {
    /// 5-minute CI smoke — every applicable fault fires inside the first
    /// minute. This is the variant `cargo test -p soak_harness` runs.
    CiSmoke,
    /// 30-minute smoke soak — gate for the full weekend run.
    ThirtyMinuteSmoke,
    /// 72-hour weekend run. Operator-triggered, never CI.
    FullWeekend,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num_part, unit) = if let Some(p) = s.find(|c: char| c.is_alphabetic()) {
        (&s[..p], &s[p..])
    } else {
        (s, "s")
    };
    let n: u64 = num_part
        .parse()
        .map_err(|e: std::num::ParseIntError| format!("invalid duration number: {e}"))?;
    let secs = match unit {
        "ms" => return Ok(Duration::from_millis(n)),
        "s" => n,
        "m" => n * 60,
        "h" => n * 60 * 60,
        "d" => n * 60 * 60 * 24,
        other => return Err(format!("unknown unit '{other}' (want ms|s|m|h|d)")),
    };
    Ok(Duration::from_secs(secs))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,soak_harness=info,symphony=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    // Build a default scratch dir if the user didn't override individual
    // paths. Using PID keeps multiple invocations on the same host
    // separated.
    let pid = std::process::id();
    let scratch = std::env::temp_dir().join(format!("soak-harness-{pid}"));
    std::fs::create_dir_all(&scratch)?;

    let mut config = match cli.profile {
        Some(Profile::CiSmoke) => HarnessConfig::smoke_defaults(&scratch),
        Some(Profile::ThirtyMinuteSmoke) => HarnessConfig::thirty_minute_smoke(&scratch),
        Some(Profile::FullWeekend) => HarnessConfig::full_weekend(&scratch),
        None => HarnessConfig {
            duration: cli.duration,
            tick: cli.tick,
            audit_path: cli.audit_log.clone().unwrap_or_else(|| scratch.join("audit.log")),
            metrics_path: cli.metrics_out.clone().or_else(|| Some(scratch.join("metrics.jsonl"))),
            workspace_root: cli
                .workspace_root
                .clone()
                .unwrap_or_else(|| scratch.join("workspaces")),
            max_concurrent_agents: cli.max_concurrent_agents,
            agent_label: "agent:claude".to_string(),
            faults: soak_harness::FaultSchedule::default_ci_smoke(),
            fixtures: soak_harness::seed_fixtures(),
            stuck_task_threshold: Duration::from_secs(30 * 60),
        },
    };

    // Apply explicit overrides on top of the chosen profile.
    if cli.profile.is_some() {
        if let Some(p) = cli.audit_log.as_ref() {
            config.audit_path = p.clone();
        }
        if let Some(p) = cli.metrics_out.as_ref() {
            config.metrics_path = Some(p.clone());
        }
        if let Some(p) = cli.workspace_root.as_ref() {
            config.workspace_root = p.clone();
        }
    }

    eprintln!(
        "soak_harness starting: duration={:?} tick={:?} audit={} metrics={}",
        config.duration,
        config.tick,
        config.audit_path.display(),
        config
            .metrics_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".into()),
    );

    let runner = HarnessRunner::new(config)?;
    let summary = runner.run(None).await?;

    eprintln!("---- soak harness summary ----");
    eprintln!("ticks                         : {}", summary.ticks);
    eprintln!("elapsed_seconds               : {}", summary.elapsed_seconds);
    eprintln!("ticks_observed                : {}", summary.final_snapshot.ticks_observed);
    eprintln!("tasks_claimed                 : {}", summary.final_snapshot.tasks_claimed);
    eprintln!("tasks_dispatched              : {}", summary.final_snapshot.tasks_dispatched);
    eprintln!("tasks_completed               : {}", summary.final_snapshot.tasks_completed);
    eprintln!("tasks_failed                  : {}", summary.final_snapshot.tasks_failed);
    eprintln!("tasks_stalled                 : {}", summary.final_snapshot.tasks_stalled);
    eprintln!("retries_scheduled             : {}", summary.final_snapshot.retries_scheduled);
    eprintln!("retries_dispatched            : {}", summary.final_snapshot.retries_dispatched);
    eprintln!("retries_given_up              : {}", summary.final_snapshot.retries_given_up);
    eprintln!("diff_guard_exceeded           : {}", summary.final_snapshot.diff_guard_exceeded);
    eprintln!("test_deletion_attempts        : {}", summary.final_snapshot.test_deletion_attempts);
    eprintln!("test_deletion_blocked         : {}", summary.final_snapshot.test_deletion_blocked);
    eprintln!("budget_tier_transitions       : {}", summary.final_snapshot.budget_tier_transitions);
    eprintln!("faults_injected               : {}", summary.final_snapshot.faults_injected);
    eprintln!("faults_recovered              : {}", summary.final_snapshot.faults_recovered);
    eprintln!("invariant_breaches            : {}", summary.final_snapshot.invariant_breaches);
    eprintln!(
        "completion_ratio              : {:.3}",
        summary.final_snapshot.completion_ratio()
    );
    eprintln!("passed                        : {}", summary.passed());

    // Print per-fault rows so the operator can see which surfaces are
    // wired and which surfaced as gaps.
    eprintln!("---- fault outcomes ----");
    for f in &summary.fault_outcomes {
        eprintln!("  {} -> {:?}: {}", f.fault, f.status, f.detail);
    }

    if !summary.passed() {
        std::process::exit(2);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_duration;
    use std::time::Duration;

    #[test]
    fn parse_duration_handles_units() {
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("5s").unwrap(), Duration::from_secs(5));
        assert_eq!(parse_duration("3m").unwrap(), Duration::from_secs(180));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
    }

    #[test]
    fn parse_duration_rejects_unknown_unit() {
        assert!(parse_duration("5y").is_err());
    }
}
