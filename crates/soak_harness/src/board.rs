//! Synthetic Linear-shaped board for the soak harness.
//!
//! Implements [`symphony::orchestrator::IssueSource`] so the real Symphony
//! orchestrator can poll us instead of `crates/symphony/src/tracker.rs`'s
//! Linear GraphQL client. State changes (comments, transitions) are
//! recorded in memory rather than written to a real tracker.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use symphony::orchestrator::IssueSource;
use symphony::tracker::{Issue, TrackerError};

use crate::fixtures::FixtureIssue;

/// Comment recorded against a synthetic issue.
#[derive(Debug, Clone)]
pub struct Comment {
    /// When the comment was recorded.
    pub at: DateTime<Utc>,
    /// Body text.
    pub body: String,
}

/// State transition recorded against a synthetic issue.
#[derive(Debug, Clone)]
pub struct Transition {
    /// When the transition was recorded.
    pub at: DateTime<Utc>,
    /// Target state name (e.g. `"In Review"`).
    pub target: String,
}

#[derive(Debug, Default)]
struct BoardInner {
    issues: HashMap<String, Issue>,
    /// Insertion order of `issue.id`s — preserved so polls have a stable
    /// ordering before Symphony's own sort-by-(priority, created_at).
    order: Vec<String>,
    comments: HashMap<String, Vec<Comment>>,
    transitions: HashMap<String, Vec<Transition>>,
    /// Number of `fetch_candidate_issues` calls observed; used by tests
    /// to assert the tick loop is actually running.
    poll_count: u64,
    /// If `true`, the next poll returns a tracker error. Used by the
    /// fault-injection schedule to exercise transient tracker failures.
    inject_poll_error: bool,
}

/// Synthetic board. Cheap to clone behind an `Arc` — internal state is
/// `Mutex`-guarded.
pub struct SyntheticBoard {
    inner: Mutex<BoardInner>,
}

impl SyntheticBoard {
    /// Construct an empty board.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(BoardInner::default()),
        }
    }

    /// Bulk-load fixtures, applying `label` as the `agent_label_required`.
    pub fn with_fixtures(fixtures: &[FixtureIssue], label: &str) -> Self {
        let board = Self::new();
        let mut inner = board.inner.lock().expect("fresh mutex");
        for f in fixtures {
            let issue = f.to_issue(label);
            inner.order.push(issue.id.clone());
            inner.issues.insert(issue.id.clone(), issue);
        }
        drop(inner);
        board
    }

    /// Number of `fetch_candidate_issues` calls observed since construction.
    pub fn poll_count(&self) -> u64 {
        self.inner.lock().map(|g| g.poll_count).unwrap_or(0)
    }

    /// Total issues currently held by the board.
    pub fn issue_count(&self) -> usize {
        self.inner.lock().map(|g| g.issues.len()).unwrap_or(0)
    }

    /// Add a single issue mid-run (used by fault injection or tests).
    pub fn add_issue(&self, issue: Issue) {
        if let Ok(mut g) = self.inner.lock() {
            if !g.issues.contains_key(&issue.id) {
                g.order.push(issue.id.clone());
            }
            g.issues.insert(issue.id.clone(), issue);
        }
    }

    /// Drop a single issue (e.g. simulating manual cancellation).
    pub fn remove_issue(&self, id: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.issues.remove(id);
            g.order.retain(|x| x != id);
        }
    }

    /// Inject a one-shot tracker error on the *next* poll. Used by fault
    /// injection to verify the orchestrator is resilient to a missed poll.
    pub fn inject_poll_error(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.inject_poll_error = true;
        }
    }

    /// Snapshot of comments for `issue_id`.
    pub fn comments_for(&self, issue_id: &str) -> Vec<Comment> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.comments.get(issue_id).cloned())
            .unwrap_or_default()
    }

    /// Snapshot of state transitions for `issue_id`.
    pub fn transitions_for(&self, issue_id: &str) -> Vec<Transition> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.transitions.get(issue_id).cloned())
            .unwrap_or_default()
    }

    /// Total number of state transitions observed across all issues.
    /// Used by harness invariants to verify the audit log is non-empty.
    pub fn total_transitions(&self) -> usize {
        self.inner
            .lock()
            .map(|g| g.transitions.values().map(|v| v.len()).sum())
            .unwrap_or(0)
    }
}

impl Default for SyntheticBoard {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl IssueSource for SyntheticBoard {
    async fn fetch_candidate_issues(
        &self,
        _active_states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|e| TrackerError::Http(format!("synthetic board mutex poisoned: {e}")))?;
        g.poll_count += 1;
        if g.inject_poll_error {
            g.inject_poll_error = false;
            return Err(TrackerError::Http("injected poll failure".to_string()));
        }
        // Return issues in insertion order; `Orchestrator::tick` re-sorts.
        let mut out = Vec::with_capacity(g.order.len());
        for id in &g.order {
            if let Some(i) = g.issues.get(id) {
                out.push(i.clone());
            }
        }
        Ok(out)
    }

    async fn add_comment(&self, issue_id: &str, body: &str) -> Result<(), TrackerError> {
        if let Ok(mut g) = self.inner.lock() {
            g.comments.entry(issue_id.to_string()).or_default().push(Comment {
                at: Utc::now(),
                body: body.to_string(),
            });
        }
        Ok(())
    }

    async fn transition_issue(
        &self,
        issue_id: &str,
        target_state_name: &str,
    ) -> Result<(), TrackerError> {
        if let Ok(mut g) = self.inner.lock() {
            // Update the canonical state on the issue itself so subsequent
            // polls observe the new state (mirrors Linear's behaviour).
            if let Some(i) = g.issues.get_mut(issue_id) {
                i.state = target_state_name.to_string();
            }
            g.transitions.entry(issue_id.to_string()).or_default().push(Transition {
                at: Utc::now(),
                target: target_state_name.to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{seed_fixtures, BehaviorTag, FixtureIssue};

    #[tokio::test]
    async fn fetch_returns_seeded_issues() {
        let fix = seed_fixtures();
        let n = fix.len();
        let board = SyntheticBoard::with_fixtures(&fix, "agent:claude");
        let got = board
            .fetch_candidate_issues(&["Todo".to_string()])
            .await
            .expect("fetch ok");
        assert_eq!(got.len(), n);
        assert_eq!(board.poll_count(), 1);
    }

    #[tokio::test]
    async fn injected_poll_error_clears_after_one_call() {
        let board = SyntheticBoard::with_fixtures(
            &[FixtureIssue { seq: 1, tag: BehaviorTag::HappyFast, title: "x".into() }],
            "agent:claude",
        );
        board.inject_poll_error();
        assert!(board.fetch_candidate_issues(&[]).await.is_err());
        assert!(board.fetch_candidate_issues(&[]).await.is_ok(), "second call recovers");
    }

    #[tokio::test]
    async fn transition_updates_state() {
        let fix = vec![FixtureIssue { seq: 1, tag: BehaviorTag::HappyFast, title: "x".into() }];
        let board = SyntheticBoard::with_fixtures(&fix, "agent:claude");
        let id = format!("synthetic-soak-{:04}-{}", 1, BehaviorTag::HappyFast.suffix()).to_lowercase();
        board.transition_issue(&id, "In Review").await.unwrap();
        let after = board
            .fetch_candidate_issues(&[])
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(after.state, "In Review");
        assert_eq!(board.transitions_for(&id).len(), 1);
        assert_eq!(board.total_transitions(), 1);
    }

    #[tokio::test]
    async fn add_and_remove_issue_round_trip() {
        let board = SyntheticBoard::new();
        let f = FixtureIssue { seq: 99, tag: BehaviorTag::HappyFast, title: "midrun".into() };
        let issue = f.to_issue("agent:claude");
        let id = issue.id.clone();
        board.add_issue(issue);
        assert_eq!(board.issue_count(), 1);
        board.remove_issue(&id);
        assert_eq!(board.issue_count(), 0);
    }
}
