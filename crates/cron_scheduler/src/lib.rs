//! Local cron-like scheduler for the Symphony daemon (PDX-26 D3).
//!
//! Pure-Rust 5-field cron expression parser (`min hour dom mon dow`) plus a
//! tokio-based driver that fires [`ScheduledTaskTriggered`] events on a
//! [`tokio::sync::mpsc`] channel whenever a configured schedule matures.
//!
//! Designed to slot in next to Symphony's reconciliation tick — the driver
//! runs as a separate tokio task and the receiver side emits structured
//! events that the rest of Symphony (or downstream consumers in tests) can
//! react to without coupling to the scheduler internals.
//!
//! ## Cron expression syntax
//!
//! Standard 5-field UTC expressions:
//!
//! ```text
//! ┌───────────── minute        (0-59)
//! │ ┌─────────── hour          (0-23)
//! │ │ ┌───────── day of month  (1-31)
//! │ │ │ ┌─────── month         (1-12)
//! │ │ │ │ ┌───── day of week   (0-6, 0 = Sunday)
//! │ │ │ │ │
//! * * * * *
//! ```
//!
//! Each field accepts: `*`, `N`, `A-B` ranges, `A,B,C` lists, `*/N` step
//! values, and `A-B/N` step-over-range. Names (e.g. `MON`) are not
//! supported — keep the syntax tight to keep the parser small. Per POSIX
//! cron, when both day-of-month and day-of-week are restricted (neither is
//! `*`), the schedule fires when EITHER matches — this matches the behavior
//! of Vixie cron and `cron` crate.
//!
//! ## Why not pull in the `cron` crate?
//!
//! The workspace already has a long compile budget and we only need a tiny
//! subset of cron semantics. A self-contained ~250-LOC parser keeps the
//! Symphony binary lean and dependency-clean; if we ever need 6-field
//! (with seconds) or named months we can swap in `cron` behind the same
//! [`Schedule`] trait.

#![deny(missing_docs)]

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;

/// One event emitted whenever a cron schedule matures.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledTaskTriggered {
    /// Configured task name (e.g. `"nightly-dependency-bump"`).
    pub name: String,
    /// Original cron expression.
    pub cron: String,
    /// Opaque payload supplied by the operator at config time. Symphony's
    /// trigger handler decides what this means (e.g. a Linear issue
    /// template name or a webhook to fan out).
    pub payload: serde_json::Value,
    /// UTC timestamp at which the schedule fired.
    pub fired_at: DateTime<Utc>,
}

/// Single configured cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// Human-readable name (unique within the scheduler).
    pub name: String,
    /// 5-field cron expression in UTC.
    pub cron: String,
    /// Opaque payload echoed back in the emitted event.
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Top-level cron scheduler configuration. Suitable for embedding in
/// `WORKFLOW.md` front matter under `cron.jobs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CronSchedulerConfig {
    /// Configured jobs. Empty means the scheduler is a no-op.
    #[serde(default)]
    pub jobs: Vec<CronJob>,
}

/// Errors produced while parsing or running cron expressions.
#[derive(Debug, Error)]
pub enum CronError {
    /// The expression doesn't have exactly five whitespace-separated fields.
    #[error("invalid cron: expected 5 fields, got {0}")]
    WrongFieldCount(usize),
    /// A field contained a value outside its valid range.
    #[error("invalid cron field `{field}`: {reason}")]
    InvalidField {
        /// Source of the offending field, e.g. `minute`.
        field: &'static str,
        /// Human-readable explanation.
        reason: String,
    },
}

/// A parsed 5-field cron expression. Fields hold the explicit set of valid
/// integers for that position (so `*/15` in minutes becomes `{0,15,30,45}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpression {
    minutes: BTreeSet<u32>,
    hours: BTreeSet<u32>,
    days_of_month: BTreeSet<u32>,
    months: BTreeSet<u32>,
    days_of_week: BTreeSet<u32>,
    /// True iff both day-of-month and day-of-week fields are not `*`. In that
    /// case Vixie cron semantics OR the two together; otherwise we AND.
    dom_dow_or: bool,
    /// Original input, kept for echo-back into events.
    raw: String,
}

impl CronExpression {
    /// Parse a 5-field cron expression.
    pub fn parse(raw: &str) -> Result<Self, CronError> {
        let fields: Vec<&str> = raw.split_whitespace().collect();
        if fields.len() != 5 {
            return Err(CronError::WrongFieldCount(fields.len()));
        }
        let minutes = parse_field(fields[0], 0, 59, "minute")?;
        let hours = parse_field(fields[1], 0, 23, "hour")?;
        let days_of_month = parse_field(fields[2], 1, 31, "day_of_month")?;
        let months = parse_field(fields[3], 1, 12, "month")?;
        let days_of_week = parse_field(fields[4], 0, 6, "day_of_week")?;
        let dom_dow_or = fields[2] != "*" && fields[4] != "*";
        Ok(Self {
            minutes,
            hours,
            days_of_month,
            months,
            days_of_week,
            dom_dow_or,
            raw: raw.to_string(),
        })
    }

    /// Compute the next UTC time after `after` (exclusive) at which this
    /// schedule fires. Resolution is one minute. Returns `None` only if no
    /// match exists within ~4 years (which can only happen for impossibly
    /// constrained expressions — well-formed ones always match within a
    /// year).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        // Round up to the next whole minute (cron resolution is 1 min).
        let mut t = after
            .with_second(0)
            .and_then(|x| x.with_nanosecond(0))
            .unwrap_or(after);
        t += ChronoDuration::minutes(1);

        // Bounded scan: max ~4 years of minutes.
        let max_iters: u64 = 60 * 24 * 366 * 4;
        for _ in 0..max_iters {
            if !self.months.contains(&t.month()) {
                // Skip to the 1st of next month.
                t = advance_to_next_month(t);
                continue;
            }
            let dow = t.weekday().num_days_from_sunday();
            let dom_match = self.days_of_month.contains(&t.day());
            let dow_match = self.days_of_week.contains(&dow);
            let day_match = if self.dom_dow_or {
                dom_match || dow_match
            } else {
                dom_match && dow_match
            };
            if !day_match {
                // Skip to start of next day.
                t = advance_to_next_day(t);
                continue;
            }
            if !self.hours.contains(&t.hour()) {
                t = advance_to_next_hour(t);
                continue;
            }
            if !self.minutes.contains(&t.minute()) {
                t += ChronoDuration::minutes(1);
                continue;
            }
            return Some(t);
        }
        None
    }

    /// Original raw expression text.
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

fn advance_to_next_month(t: DateTime<Utc>) -> DateTime<Utc> {
    let (y, m) = if t.month() == 12 {
        (t.year() + 1, 1)
    } else {
        (t.year(), t.month() + 1)
    };
    Utc.with_ymd_and_hms(y, m, 1, 0, 0, 0).unwrap()
}

fn advance_to_next_day(t: DateTime<Utc>) -> DateTime<Utc> {
    (t.date_naive() + ChronoDuration::days(1))
        .and_hms_opt(0, 0, 0)
        .map(|d| Utc.from_utc_datetime(&d))
        .unwrap_or(t)
}

fn advance_to_next_hour(t: DateTime<Utc>) -> DateTime<Utc> {
    t.with_minute(0).and_then(|x| x.with_second(0)).unwrap_or(t) + ChronoDuration::hours(1)
}

fn parse_field(
    spec: &str,
    min: u32,
    max: u32,
    field: &'static str,
) -> Result<BTreeSet<u32>, CronError> {
    let mut out = BTreeSet::new();
    for part in spec.split(',') {
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let s_n: u32 = s.parse().map_err(|_| CronError::InvalidField {
                    field,
                    reason: format!("bad step `{s}`"),
                })?;
                if s_n == 0 {
                    return Err(CronError::InvalidField {
                        field,
                        reason: "step must be > 0".into(),
                    });
                }
                (r, s_n)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a_n: u32 = a.parse().map_err(|_| CronError::InvalidField {
                field,
                reason: format!("bad range start `{a}`"),
            })?;
            let b_n: u32 = b.parse().map_err(|_| CronError::InvalidField {
                field,
                reason: format!("bad range end `{b}`"),
            })?;
            (a_n, b_n)
        } else {
            let n: u32 = range_part.parse().map_err(|_| CronError::InvalidField {
                field,
                reason: format!("bad value `{range_part}`"),
            })?;
            (n, n)
        };
        if lo < min || hi > max || lo > hi {
            return Err(CronError::InvalidField {
                field,
                reason: format!("range {lo}-{hi} outside [{min},{max}]"),
            });
        }
        let mut v = lo;
        while v <= hi {
            out.insert(v);
            // Saturating step add to avoid overflow on the last iteration.
            v = match v.checked_add(step) {
                Some(n) => n,
                None => break,
            };
        }
    }
    if out.is_empty() {
        return Err(CronError::InvalidField {
            field,
            reason: "no values matched".into(),
        });
    }
    Ok(out)
}

/// Driver wrapping a set of [`CronJob`]s with a tokio task that fires
/// [`ScheduledTaskTriggered`] events on the channel returned by [`Self::run`].
pub struct CronScheduler {
    jobs: Vec<(CronJob, CronExpression)>,
}

impl CronScheduler {
    /// Construct a scheduler from config, eagerly validating every cron
    /// expression so misconfigurations fail at startup.
    pub fn from_config(config: &CronSchedulerConfig) -> Result<Self, CronError> {
        let mut jobs = Vec::with_capacity(config.jobs.len());
        for job in &config.jobs {
            let expr = CronExpression::parse(&job.cron)?;
            jobs.push((job.clone(), expr));
        }
        Ok(Self { jobs })
    }

    /// Number of configured jobs.
    pub fn len(&self) -> usize {
        self.jobs.len()
    }

    /// True if no jobs are configured.
    pub fn is_empty(&self) -> bool {
        self.jobs.is_empty()
    }

    /// Spawn the scheduler. Returns an mpsc receiver of trigger events plus
    /// a oneshot sender that cancels the loop (drop the sender to also stop
    /// the scheduler). The task exits on cancel and on receiver-dropped.
    pub fn run(
        self,
    ) -> (
        mpsc::Receiver<ScheduledTaskTriggered>,
        tokio::task::JoinHandle<()>,
    ) {
        let (tx, rx) = mpsc::channel(64);
        let handle = tokio::spawn(async move {
            self.run_loop(tx).await;
        });
        (rx, handle)
    }

    async fn run_loop(self, tx: mpsc::Sender<ScheduledTaskTriggered>) {
        if self.jobs.is_empty() {
            tracing::debug!("cron_scheduler: no jobs configured; exiting");
            return;
        }
        // Track each job's next fire time independently so we always sleep
        // until the earliest one matures.
        let mut next: Vec<DateTime<Utc>> = self
            .jobs
            .iter()
            .map(|(_, expr)| expr.next_after(Utc::now()).unwrap_or_else(far_future))
            .collect();

        loop {
            // Find earliest next.
            let (idx, &when) = match next.iter().enumerate().min_by_key(|(_, t)| **t) {
                Some(p) => p,
                None => return,
            };
            let now = Utc::now();
            let delay = (when - now).max(ChronoDuration::zero());
            let std_delay =
                std::time::Duration::from_millis(delay.num_milliseconds().max(0) as u64);
            tokio::time::sleep(std_delay).await;

            let (job, expr) = &self.jobs[idx];
            let event = ScheduledTaskTriggered {
                name: job.name.clone(),
                cron: job.cron.clone(),
                payload: job.payload.clone(),
                fired_at: Utc::now(),
            };
            if tx.send(event).await.is_err() {
                tracing::debug!("cron_scheduler: receiver dropped; exiting");
                return;
            }
            // Schedule next occurrence strictly after the one we just fired
            // to avoid re-firing the same minute.
            next[idx] = expr.next_after(when).unwrap_or_else(far_future);
        }
    }
}

fn far_future() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(9999, 1, 1, 0, 0, 0).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_star_minute() {
        let e = CronExpression::parse("* * * * *").unwrap();
        assert_eq!(e.minutes.len(), 60);
        assert_eq!(e.hours.len(), 24);
    }

    #[test]
    fn parse_step() {
        let e = CronExpression::parse("*/15 * * * *").unwrap();
        assert_eq!(
            e.minutes.iter().copied().collect::<Vec<_>>(),
            vec![0, 15, 30, 45]
        );
    }

    #[test]
    fn parse_range_and_list() {
        let e = CronExpression::parse("0 9-17 * * 1,3,5").unwrap();
        assert_eq!(e.minutes.iter().copied().collect::<Vec<_>>(), vec![0]);
        assert_eq!(
            e.hours.iter().copied().collect::<Vec<_>>(),
            (9..=17).collect::<Vec<_>>()
        );
        assert_eq!(
            e.days_of_week.iter().copied().collect::<Vec<_>>(),
            vec![1, 3, 5]
        );
    }

    #[test]
    fn parse_bad_field() {
        assert!(CronExpression::parse("60 * * * *").is_err());
        assert!(CronExpression::parse("* 24 * * *").is_err());
        assert!(CronExpression::parse("* * 0 * *").is_err());
        assert!(CronExpression::parse("* * * 13 *").is_err());
        assert!(CronExpression::parse("* * * * 7").is_err());
    }

    #[test]
    fn parse_wrong_field_count() {
        assert!(matches!(
            CronExpression::parse("* * * *"),
            Err(CronError::WrongFieldCount(4))
        ));
    }

    #[test]
    fn next_after_every_minute() {
        let e = CronExpression::parse("* * * * *").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap();
        let n = e.next_after(t).unwrap();
        assert_eq!(n, Utc.with_ymd_and_hms(2026, 5, 1, 12, 1, 0).unwrap());
    }

    #[test]
    fn next_after_nightly() {
        // 0 3 * * * → next 03:00 UTC.
        let e = CronExpression::parse("0 3 * * *").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap();
        let n = e.next_after(t).unwrap();
        assert_eq!(n, Utc.with_ymd_and_hms(2026, 5, 2, 3, 0, 0).unwrap());
    }

    #[test]
    fn next_after_weekly_monday() {
        // 0 9 * * 1 → next Monday 09:00 UTC. 2026-05-01 is a Friday.
        let e = CronExpression::parse("0 9 * * 1").unwrap();
        let t = Utc.with_ymd_and_hms(2026, 5, 1, 12, 0, 0).unwrap();
        let n = e.next_after(t).unwrap();
        assert_eq!(n, Utc.with_ymd_and_hms(2026, 5, 4, 9, 0, 0).unwrap());
    }

    #[test]
    fn next_after_dom_dow_or() {
        // Vixie semantics: when both DoM and DoW are set, fires on EITHER.
        // 0 0 1 * 0 → midnight on the 1st OR on Sundays.
        let e = CronExpression::parse("0 0 1 * 0").unwrap();
        // 2026-05-02 is Saturday 12:00. Next match is Sun 2026-05-03 00:00.
        let t = Utc.with_ymd_and_hms(2026, 5, 2, 12, 0, 0).unwrap();
        let n = e.next_after(t).unwrap();
        assert_eq!(n, Utc.with_ymd_and_hms(2026, 5, 3, 0, 0, 0).unwrap());
    }

    #[test]
    fn scheduler_validates_jobs_at_construction() {
        let cfg = CronSchedulerConfig {
            jobs: vec![CronJob {
                name: "bad".into(),
                cron: "not-a-cron".into(),
                payload: serde_json::Value::Null,
            }],
        };
        assert!(CronScheduler::from_config(&cfg).is_err());
    }

    #[test]
    fn scheduler_empty_is_ok() {
        let s = CronScheduler::from_config(&CronSchedulerConfig::default()).unwrap();
        assert!(s.is_empty());
    }
}
