//! `warp audit` — operator-facing access to the symphony audit log JSONL.
//!
//! The symphony [`AuditLog`](crates/symphony/src/audit.rs) writes a JSON
//! object per line to `~/.warp/symphony/audit.log`. Each line has the shape:
//!
//! ```json
//! { "timestamp": "<RFC3339>", "task_id": "...", "agent_id": "...",
//!   "rule": "diff_size|test_deletion|deploy_gate|budget_exceeded",
//!   "action": "blocked|overridden|allowed", "offending_path": "...",
//!   "detail": "..." }
//! ```
//!
//! This module provides:
//! * Clap argument structures for `warp audit query|follow|summary|sync`
//! * A small, allocation-light JSONL parser that tolerates partial / future
//!   fields without failing
//! * Filter predicates (`--filter rule=budget_exceeded`, `--action ...`,
//!   `--actor ...`, `--since 1h`)
//! * Aggregation helpers used by `summary`
//!
//! The CLI handler is wired in `app/src/ai/agent_sdk/audit.rs`; this crate
//! only owns the surface and the (heavily unit-tested) parsing + filtering
//! primitives.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Duration, Utc};
use clap::{Args, Subcommand};

/// Default location of the symphony audit-log JSONL.
///
/// Resolves to `~/.warp/symphony/audit.log` at runtime; we keep the path
/// suffix as a `&'static str` here so it can be appended to the home dir
/// without pulling in `dirs` from this lib.
pub const DEFAULT_AUDIT_LOG_SUFFIX: &str = ".warp/symphony/audit.log";

/// Subcommands of `warp audit`.
#[derive(Debug, Clone, Subcommand)]
pub enum AuditCommand {
    /// Query the JSONL audit log with optional filters.
    ///
    /// Reads `~/.warp/symphony/audit.log` (override with `--path`) and prints
    /// matching rows to stdout. Use `--since` to restrict to a recent window
    /// (e.g. `--since 1h`, `--since 24h`, `--since 7d`).
    Query(QueryArgs),

    /// Follow new entries as they're appended (`tail -f`-style).
    ///
    /// Useful while running soak tests or reproducing budget-bomb scenarios.
    Follow(FollowArgs),

    /// Summarize counts grouped by action / rule / actor.
    ///
    /// Produces an ASCII-table breakdown matching what the Grafana
    /// `helm-overview.json` dashboard renders.
    Summary(SummaryArgs),

    /// Mirror local audit-log entries to the cloud control plane.
    ///
    /// Pushes batched JSONL rows to the `/api/audit/sync` route exposed by
    /// `cloudflare-control-plane/src/workers/audit_mirror.ts`, which writes
    /// them into the `audit_log` D1 table from PDX-22.
    Sync(SyncArgs),
}

/// Shared filter / path arguments accepted by every subcommand.
#[derive(Debug, Clone, Args)]
pub struct CommonAuditArgs {
    /// Override the audit-log JSONL path. Defaults to
    /// `~/.warp/symphony/audit.log`.
    #[arg(long = "path", value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Only include rows with the given `action` value (e.g. `blocked`,
    /// `overridden`, `allowed`, `http.request`).
    #[arg(long = "action", value_name = "ACTION")]
    pub action: Option<String>,

    /// Only include rows with the given `rule` value (e.g. `budget_exceeded`,
    /// `diff_size`, `test_deletion`, `deploy_gate`).
    #[arg(long = "rule", value_name = "RULE")]
    pub rule: Option<String>,

    /// Only include rows for the given actor (`agent_id` or `task_id`).
    #[arg(long = "actor", value_name = "ID")]
    pub actor: Option<String>,

    /// Restrict to entries whose `timestamp` is within the given window
    /// (e.g. `1h`, `24h`, `7d`).
    #[arg(long = "since", value_name = "DURATION")]
    pub since: Option<humantime::Duration>,

    /// Free-form `key=value` predicates, repeatable.
    ///
    /// Accepted keys: `action`, `rule`, `actor`, `task_id`, `agent_id`,
    /// `offending_path`. Multiple `--filter` flags are AND-ed together.
    #[arg(long = "filter", value_name = "KEY=VALUE")]
    pub filter: Vec<String>,
}

/// Args for `warp audit query`.
#[derive(Debug, Clone, Args)]
pub struct QueryArgs {
    #[clap(flatten)]
    pub common: CommonAuditArgs,

    /// Cap the number of rows printed.
    #[arg(long = "limit", value_name = "N")]
    pub limit: Option<usize>,
}

/// Args for `warp audit follow`.
#[derive(Debug, Clone, Args)]
pub struct FollowArgs {
    #[clap(flatten)]
    pub common: CommonAuditArgs,
}

/// Args for `warp audit summary`.
#[derive(Debug, Clone, Args)]
pub struct SummaryArgs {
    #[clap(flatten)]
    pub common: CommonAuditArgs,
}

/// Args for `warp audit sync`.
#[derive(Debug, Clone, Args)]
pub struct SyncArgs {
    #[clap(flatten)]
    pub common: CommonAuditArgs,

    /// Cloud control-plane base URL (e.g.
    /// `https://helm-control-plane-prod.workers.dev`). The CLI will POST
    /// batches to `<remote>/api/audit/sync`.
    #[arg(long = "remote", value_name = "URL")]
    pub remote: String,

    /// Maximum rows per batch when posting.
    #[arg(long = "batch-size", default_value_t = 500)]
    pub batch_size: usize,

    /// Optional bearer token for the control-plane Access policy.
    #[arg(long = "token", env = "WARP_AUDIT_SYNC_TOKEN")]
    pub token: Option<String>,
}

/// One parsed audit-log entry.
///
/// Unknown fields are preserved verbatim under [`AuditEntry::extra`] so we
/// don't have to bump this struct every time PDX-28 grows the schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditEntry {
    /// RFC 3339 timestamp string. Parsed lazily by the filter layer.
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub rule: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub action: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub offending_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// Anything else the writer may have included. Preserved on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl AuditEntry {
    /// Parse the `timestamp` field into a UTC `DateTime`. Returns `None`
    /// if the string is missing or unparseable; callers treat that as
    /// "always include in unbounded queries".
    pub fn parsed_timestamp(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.timestamp)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    }
}

/// Parse a single line of the audit JSONL.
///
/// Empty / whitespace-only lines yield `Ok(None)` so callers can iterate
/// without filtering. Malformed JSON returns an error annotated with the
/// raw line for operator-friendly debugging.
pub fn parse_line(line: &str) -> Result<Option<AuditEntry>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let entry: AuditEntry = serde_json::from_str(trimmed)
        .with_context(|| format!("malformed audit-log line: {}", trimmed))?;
    Ok(Some(entry))
}

/// Parse a whole JSONL blob.
///
/// Bad lines are surfaced with their 1-based line number to make it easy
/// to point an operator at the offending entry.
pub fn parse_jsonl(blob: &str) -> Result<Vec<AuditEntry>> {
    let mut out = Vec::new();
    for (idx, line) in blob.lines().enumerate() {
        match parse_line(line) {
            Ok(Some(e)) => out.push(e),
            Ok(None) => {}
            Err(e) => return Err(e.context(format!("line {}", idx + 1))),
        }
    }
    Ok(out)
}

/// A compiled set of predicates. Built from [`CommonAuditArgs`] and
/// applied by [`Predicate::matches`].
#[derive(Debug, Default, Clone)]
pub struct Predicate {
    pub action: Option<String>,
    pub rule: Option<String>,
    pub actor: Option<String>,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
    pub offending_path: Option<String>,
    /// Lower bound on `timestamp`. `None` means unbounded.
    pub since: Option<DateTime<Utc>>,
}

impl Predicate {
    /// Build a predicate from CLI args. `now` is the reference time used
    /// for the `--since` window (parameterized so tests are deterministic).
    pub fn from_args(args: &CommonAuditArgs, now: DateTime<Utc>) -> Result<Self> {
        let mut p = Predicate {
            action: args.action.clone(),
            rule: args.rule.clone(),
            actor: args.actor.clone(),
            since: args
                .since
                .map(|d| {
                    let std: std::time::Duration = d.into();
                    let chrono_dur = Duration::from_std(std)
                        .map_err(|e| anyhow!("--since out of range: {e}"))?;
                    Ok::<_, anyhow::Error>(now - chrono_dur)
                })
                .transpose()?,
            ..Predicate::default()
        };

        for raw in &args.filter {
            let (k, v) = raw
                .split_once('=')
                .ok_or_else(|| anyhow!("--filter expects KEY=VALUE, got `{raw}`"))?;
            match k {
                "action" => p.action = Some(v.to_string()),
                "rule" => p.rule = Some(v.to_string()),
                "actor" => p.actor = Some(v.to_string()),
                "task_id" => p.task_id = Some(v.to_string()),
                "agent_id" => p.agent_id = Some(v.to_string()),
                "offending_path" => p.offending_path = Some(v.to_string()),
                other => return Err(anyhow!("unknown --filter key `{other}`")),
            }
        }
        Ok(p)
    }

    /// Returns `true` if `entry` matches every populated predicate field.
    pub fn matches(&self, entry: &AuditEntry) -> bool {
        if let Some(a) = &self.action
            && &entry.action != a
        {
            return false;
        }
        if let Some(r) = &self.rule
            && &entry.rule != r
        {
            return false;
        }
        if let Some(actor) = &self.actor
            && &entry.agent_id != actor
            && &entry.task_id != actor
        {
            return false;
        }
        if let Some(t) = &self.task_id
            && &entry.task_id != t
        {
            return false;
        }
        if let Some(a) = &self.agent_id
            && &entry.agent_id != a
        {
            return false;
        }
        if let Some(p) = &self.offending_path
            && &entry.offending_path != p
        {
            return false;
        }
        if let Some(since) = self.since
            && let Some(ts) = entry.parsed_timestamp()
            && ts < since
        {
            return false;
        }
        true
    }
}

/// Aggregated counts produced by `warp audit summary`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    pub total: usize,
    pub by_action: BTreeMap<String, usize>,
    pub by_rule: BTreeMap<String, usize>,
    pub by_actor: BTreeMap<String, usize>,
}

impl Summary {
    /// Build a summary by streaming entries through `pred`.
    pub fn from_entries<'a>(
        entries: impl IntoIterator<Item = &'a AuditEntry>,
        pred: &Predicate,
    ) -> Self {
        let mut s = Summary::default();
        for e in entries {
            if !pred.matches(e) {
                continue;
            }
            s.total += 1;
            if !e.action.is_empty() {
                *s.by_action.entry(e.action.clone()).or_default() += 1;
            }
            if !e.rule.is_empty() {
                *s.by_rule.entry(e.rule.clone()).or_default() += 1;
            }
            // Prefer agent_id as the actor; fall back to task_id.
            let actor = if !e.agent_id.is_empty() {
                Some(e.agent_id.clone())
            } else if !e.task_id.is_empty() {
                Some(e.task_id.clone())
            } else {
                None
            };
            if let Some(a) = actor {
                *s.by_actor.entry(a).or_default() += 1;
            }
        }
        s
    }

    /// Render the summary as a fixed-width ASCII table suitable for stdout.
    pub fn render_table(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("total: {}\n", self.total));
        out.push_str("\nBy action:\n");
        render_section(&mut out, &self.by_action);
        out.push_str("\nBy rule:\n");
        render_section(&mut out, &self.by_rule);
        out.push_str("\nBy actor:\n");
        render_section(&mut out, &self.by_actor);
        out
    }
}

fn render_section(out: &mut String, m: &BTreeMap<String, usize>) {
    if m.is_empty() {
        out.push_str("  (none)\n");
        return;
    }
    let key_w = m.keys().map(|k| k.len()).max().unwrap_or(0).max(4);
    for (k, v) in m {
        out.push_str(&format!("  {k:<key_w$}  {v}\n", key_w = key_w));
    }
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
