# PDX-29 [D6] 24/7 weekend test — operator runbook

This runbook describes how to drive the 72-hour Symphony soak test that
PDX-29 calls for. It also documents the pre-flight smoke variants used
as gates: the 25-second CI smoke (run automatically by `cargo test`) and
the 30-minute "smoke soak" gate that must pass before launching the full
weekend run.

The harness lives in `crates/soak_harness/` and is purely additive on
top of the upstream `symphony`, `orchestrator`, and (when present)
`auto_healing` / `cloud_*` crates — it consumes the published traits
and the audit-log file format only.

---

## TL;DR

```sh
# Gate (≤ 1 min, runs in CI):
cargo test -p soak_harness --test smoke_soak --release

# Operator-triggered 30-minute smoke soak:
cargo run --release --bin soak_harness -- --profile thirty-minute-smoke

# 72-hour weekend test:
nohup cargo run --release --bin soak_harness -- \
    --profile full-weekend \
    --metrics-out ~/.warp/symphony/soak-metrics.jsonl \
    > ~/.warp/symphony/soak.log 2>&1 &

# Tail metrics:
tail -f ~/.warp/symphony/soak-metrics.jsonl | jq -c .
```

---

## What the harness exercises

PDX-29's acceptance criteria translate into invariants the harness
checks every tick:

| PDX-29 acceptance line                         | Harness invariant         | How it's checked                                                                                              |
|------------------------------------------------|---------------------------|---------------------------------------------------------------------------------------------------------------|
| Daemon stays up 60+ hours                      | `audit_log_append_only`   | Audit-log file size is monotonically non-decreasing. A shrink would indicate a rewrite or a daemon restart.   |
| `max_concurrent_agents` respected              | `concurrency_cap`         | `Orchestrator::state_snapshot().0.len() <= max_concurrent_agents` at every tick boundary.                     |
| No diff exceeds `max_diff_lines` (PDX-28)      | derived from audit log    | Counts `DiffGuardExceeded` audit events; the `BigDiff` fixture should produce exactly N events.               |
| No production-deploy command ever executed    | `no_test_deletion`        | `RequestTestDeletion` fixtures emit a `delete_file` ToolCall; expects matching `[AUTO_HEALING][BLOCKED]` log lines. |
| Audit log has zero gaps                        | `audit_log_append_only` + per-tick `Tick` event count | Every tick emits a `Tick` event; harness counts them and asserts the count matches `ticks_observed >= 1`. |
| Sentry shows < 5 crash events                  | (out-of-band)             | Sentry hookups are part of the deploy pipeline, not the harness; confirm via the dashboard post-soak.        |
| Total spend < 50% of monthly cap               | `budget_tier_transitions` | Counts `[BUDGET][TIER]` audit log markers. Harness flags missing markers as a *gap*, not a breach.            |

The harness covers every invariant in CI; gaps for unwired upstream
crates are surfaced explicitly in the run summary so you can decide
whether to abort the soak before launching.

---

## Pre-conditions

1. **Disk**: at least 5 GB free on the partition holding
   `~/.warp/symphony/`. Audit-log growth is roughly 1 KB per tick; a 72h
   run at 10s ticks generates ~25 MB. The metrics file is similar.
2. **Build**: `cargo build --release -p soak_harness` succeeds.
3. **CI smoke is green** locally:
   ```sh
   cargo test -p soak_harness --test smoke_soak --release
   ```
   This must complete inside ~30s with `0 failed`.
4. **Optional Doppler**: not required for the soak harness — the
   synthetic board doesn't talk to Linear. If you also want to drive a
   real Symphony daemon side-by-side, set `LINEAR_API_KEY` per the
   Symphony runbook.
5. **No competing Symphony**: the harness boots its own
   `symphony::Orchestrator` in-process. If a `symphony` daemon is
   already running and writing to the same audit log path, route the
   harness elsewhere with `--audit-log /some/other/path`.

---

## Profiles

| Profile                 | Duration | Tick    | Faults                                                | Use                                                          |
|-------------------------|----------|---------|-------------------------------------------------------|--------------------------------------------------------------|
| `ci-smoke` (default)    | 5 min    | 1 s     | All four at 2/8/14/20s                                | Fast smoke — also exercised by `cargo test`. Operator can run for diagnostics. |
| `thirty-minute-smoke`   | 30 min   | 2 s     | Default smoke schedule (30/90/180/300s)               | Mandatory pre-soak gate. Run before every full weekend.      |
| `full-weekend`          | 72 h     | 10 s    | T+1h tracker outage, T+6h budget critical, T+24h MCP drop, T+48h claude kill | The PDX-29 acceptance run.                |

Override individual flags with `--duration`, `--tick`, `--audit-log`,
`--metrics-out`, `--workspace-root`, `--max-concurrent-agents`.

---

## Launch

### 30-minute pre-soak gate

```sh
cargo run --release --bin soak_harness -- --profile thirty-minute-smoke
```

The binary logs to stderr and writes JSONL metrics to
`$TMPDIR/soak-harness-{pid}/metrics.jsonl`. Exit code `0` means
`passed: true`; exit code `2` means at least one invariant breached or
a fault failed to recover. Investigate before continuing.

### Full 72-hour run

```sh
mkdir -p ~/.warp/symphony
nohup cargo run --release --bin soak_harness -- \
    --profile full-weekend \
    --audit-log ~/.warp/symphony/soak-audit.log \
    --metrics-out ~/.warp/symphony/soak-metrics.jsonl \
    --workspace-root ~/.warp/symphony/soak-workspaces \
    > ~/.warp/symphony/soak.log 2>&1 &
echo $! > ~/.warp/symphony/soak.pid
```

Confirm the process is running:

```sh
ps -p "$(cat ~/.warp/symphony/soak.pid)"
```

---

## Monitoring while the soak runs

```sh
# Live metrics:
tail -f ~/.warp/symphony/soak-metrics.jsonl | jq -c '{tick, completed: .tasks_completed, failed: .tasks_failed, stalled: .tasks_stalled, breaches: .invariant_breaches}'

# Live audit log:
tail -f ~/.warp/symphony/soak-audit.log | jq -c .

# Quick health snapshot (last 5 metrics samples):
tail -5 ~/.warp/symphony/soak-metrics.jsonl | jq -r 'select(.tasks_dispatched != null) | "tick=\(.tick) dispatched=\(.tasks_dispatched) completed=\(.tasks_completed) ratio=\(.tasks_completed * 1.0 / (.tasks_dispatched + 0.0001) | tostring | .[0:5])"'
```

### Healthy thresholds

* `completion_ratio >= 0.85` over any 6h window — anything lower
  suggests an agent-track regression. (The synthetic catalog is ~60%
  happy-path; expect this to drop to ~0.7 in the first ten minutes
  before retries land. Steady-state should converge.)
* `invariant_breaches == 0` — any non-zero count is investigative.
  The most likely cause is the orchestrator's retry-bypass-cap path
  (PDX-25 follow-up): retries can transiently push `running.len()`
  past `max_concurrent_agents`. The harness flags this.
* `tasks_stalled` should equal `4 × n_stalling_fixtures` over 72h
  (each stalling fixture re-enters via retry). If higher, look for
  agent crashes.
* `faults_recovered == faults_injected` for every applicable fault.
  Faults marked `Skipped` are gaps in upstream wiring (auto_healing,
  budget_enforcer, mcp_forwarder, claude-CLI track) — the runbook
  expects these on a branch where those crates haven't merged yet.

---

## Sample metrics output

A 25-second smoke run on this branch produced:

```json
{"at":"2026-05-03T01:47:36.485678Z","tick":51,"ticks_observed":51,"tasks_claimed":43,"tasks_dispatched":43,"tasks_completed":30,"tasks_failed":8,"tasks_stalled":2,"retries_scheduled":10,"retries_dispatched":0,"retries_given_up":0,"diff_guard_exceeded":0,"test_deletion_attempts":0,"test_deletion_blocked":0,"budget_tier_transitions":0,"faults_injected":4,"faults_recovered":1,"invariant_breaches":0}
```

```text
ticks                         : 51
elapsed_seconds               : 25
ticks_observed                : 51
tasks_claimed                 : 43
tasks_dispatched              : 43
tasks_completed               : 30
tasks_failed                  : 8
tasks_stalled                 : 2
retries_scheduled             : 10
retries_dispatched            : 0
retries_given_up              : 0
diff_guard_exceeded           : 0
test_deletion_attempts        : 0
test_deletion_blocked         : 0
budget_tier_transitions       : 0
faults_injected               : 4
faults_recovered              : 1
invariant_breaches            : 0
completion_ratio              : 0.698
passed                        : true
---- fault outcomes ----
  tracker_outage -> Recovered: injected one-shot poll error; recovery is automatic on next tick
  force_budget_critical -> Skipped: budget_enforcer not wired in this branch — gap surfaced for runbook
  drop_mcp_receiver -> Skipped: mcp_forwarder not wired in this branch — gap surfaced for runbook
  kill_claude_subprocess -> Skipped: claude-CLI agent track not wired in this branch — synthetic agent has no subprocess
```

This is the canonical "shape" — when you compare a 72h run, the
*ratios* should look the same; absolute numbers scale linearly with
duration.

---

## Post-soak inspection checklist

After the 72h run finishes (or you stop it):

1. **Daemon stayed up**:
   ```sh
   ps -p "$(cat ~/.warp/symphony/soak.pid)" || echo "daemon exited"
   wc -l ~/.warp/symphony/soak.log
   grep -E "panic|abort|SIGSEGV" ~/.warp/symphony/soak.log
   ```
2. **Final metrics snapshot**:
   ```sh
   tail -1 ~/.warp/symphony/soak-metrics.jsonl | jq .
   ```
3. **Invariant breaches over time**:
   ```sh
   jq -r 'select(.invariant_breaches > 0) | "tick=\(.tick) breaches=\(.invariant_breaches)"' \
       ~/.warp/symphony/soak-metrics.jsonl | head
   ```
4. **Audit log size and gaps**:
   ```sh
   wc -l ~/.warp/symphony/soak-audit.log
   # Audit lines should be monotonic in timestamp:
   jq -r .timestamp ~/.warp/symphony/soak-audit.log | sort -c
   ```
5. **Stall and retry shape**: count by issue identifier:
   ```sh
   jq -r 'select(.kind == "stalled") | .issue_identifier' ~/.warp/symphony/soak-audit.log | sort | uniq -c
   jq -r 'select(.kind == "retry_given_up") | .issue_identifier' ~/.warp/symphony/soak-audit.log | sort | uniq -c
   ```
6. **Test-deletion blocks** (if `auto_healing` is wired):
   ```sh
   grep -c "AUTO_HEALING" ~/.warp/symphony/soak-audit.log
   ```
7. **Cleanup**:
   ```sh
   kill "$(cat ~/.warp/symphony/soak.pid)" 2>/dev/null
   rm -rf ~/.warp/symphony/soak-workspaces
   ```

Acceptance is satisfied when:

* Daemon stayed up for ≥ 60h.
* `tail -1 metrics.jsonl | jq .invariant_breaches` is 0.
* Every fault marked `Recovered` (or `Skipped` with a documented gap).
* `completion_ratio >= 0.85` averaged over the last 12h.
* Audit log timestamps are sorted (no out-of-order writes).

---

## Known failure modes and recovery

| Failure mode                                  | Symptom                                                           | Recovery                                                                                         |
|------------------------------------------------|-------------------------------------------------------------------|--------------------------------------------------------------------------------------------------|
| Concurrency cap transiently exceeded           | `invariant_breaches` increments mid-run, no error in logs.        | Known gap: orchestrator's retry path bypasses the per-tick cap. File a follow-up to PDX-25.      |
| Audit log file disappeared                    | `audit_log_append_only` flagged Breach with size shrink.          | Disk pressure or external `rm`. Stop the soak; the audit log is the primary source of record.   |
| Stuck task                                     | `no_stuck_tasks` flagged Breach.                                  | Stall detection mis-fired — capture the running entry's identifier from the breach detail and audit-log the issue. |
| Faults all `Skipped`                           | Three of four faults show `Skipped` in summary.                   | Branch lacks `auto_healing` / `budget_enforcer` / `mcp_forwarder` / claude-CLI wiring. Re-run after those PRs merge. |
| Tracker outage didn't recover                  | `tracker_outage -> DidNotRecover`.                                | Synthetic-board mutex got poisoned. Soak is invalid; restart from scratch.                       |
| Out of disk                                    | Audit / metrics writes start failing in `tracing::warn!` lines.   | Pre-flight check should have caught this. Free space and restart; the harness is idempotent.    |

---

## Hand-off

When you finish a soak, archive the artifacts:

```sh
SOAK_TS=$(date +%Y%m%d-%H%M%S)
mkdir -p ~/.warp/symphony/soak-archive/$SOAK_TS
cp ~/.warp/symphony/soak-{audit.log,metrics.jsonl,log} \
   ~/.warp/symphony/soak-archive/$SOAK_TS/
echo "archived to ~/.warp/symphony/soak-archive/$SOAK_TS"
```

Attach the archive directory (or its `tar -czf`'d form) to the PDX-29
Linear comment, with the final metrics summary inline.
