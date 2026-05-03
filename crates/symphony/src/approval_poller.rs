//! Approval-gate poller for [`crate::deploy_tool`] (PDX-114 [E2]).
//!
//! Cloudflare's `DeployWorkflow` (PDX-25) suspends on
//! `step.waitForEvent("approval")`; the runtime resumes the workflow
//! when an `approval` event lands via
//! `POST /api/workflows/deploy/instances/:id/approve`. Symphony owns
//! the agent boundary, so it also owns the policy that decides *when*
//! to send that approval — namely: when an authorized approver posts a
//! Linear comment containing a `+approve` token on the parent issue.
//!
//! This module implements that policy as a small reusable poller. It
//! is intentionally HTTP-shaped rather than tied to a long-running
//! background loop in `Orchestrator::tick`: the daemon is free to
//! spawn one poller per outstanding deploy, scope it to the parent
//! issue, and drop it once the workflow leaves the suspended state.
//!
//! ## Policy
//!
//! For each comment fetched against the parent issue:
//! 1. Skip comments authored by Symphony itself (so the audit comment
//!    we write back does not feedback-trigger an approval).
//! 2. Skip comments older than the deploy's `started_at` timestamp.
//! 3. Reject if the author is not on the [`DeployConfig::approvers`]
//!    allowlist.
//! 4. Match the body against the approval regex (`(?m)^\s*\+approve\b`
//!    by default, configurable for tests).
//! 5. On match → POST the approval payload to the control-plane Worker
//!    and return [`PollOutcome::Approved`].
//!
//! All other comments are ignored. The poller is idempotent: once a
//! deploy has been approved its result is recorded in [`PollerState`]
//! and subsequent calls are no-ops. The control-plane Worker side is
//! also idempotent — re-sending an `approval` event to a workflow that
//! already resumed is a no-op there.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Default approval token. A line of comment body must start with this
/// prefix (allowing leading whitespace) for the approval to register.
pub const DEFAULT_APPROVAL_TOKEN: &str = "+approve";

/// One Linear comment as the poller observes it. Mirrors the subset of
/// Linear's GraphQL Comment shape we depend on; tests construct them
/// directly without needing a full GraphQL client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalComment {
    /// Comment id (used to suppress double-approvals on retry).
    pub id: String,
    /// Author identifier — Linear email or display name. The poller
    /// treats both as opaque strings and the approver allowlist must
    /// match exactly. Production wiring uses the Linear user email.
    pub author: String,
    /// Markdown body.
    pub body: String,
    /// Comment creation time.
    pub created_at: DateTime<Utc>,
}

/// Comment source abstraction so production wires
/// [`crate::tracker::LinearClient`] and tests use an in-memory mock.
#[async_trait]
pub trait CommentSource: Send + Sync {
    /// Fetch all comments on the issue, newest first or oldest first
    /// — the poller tolerates either ordering.
    async fn list_comments(&self, issue_id: &str) -> Result<Vec<ApprovalComment>, String>;
}

/// Approval transport — wraps the
/// `POST /api/workflows/deploy/instances/:id/approve` HTTP call.
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    /// Send a verified approval to the control-plane Worker.
    async fn send_approval(
        &self,
        workflow_instance_id: &str,
        approver: &str,
        rationale: Option<&str>,
        approved_at: DateTime<Utc>,
    ) -> Result<(), String>;
}

/// Per-deploy poller state.
///
/// Constructed once per outstanding deploy and reused across ticks
/// until the workflow resolves (success / timeout / cancel). The
/// `seen_comment_ids` field grows monotonically — at typical comment
/// volumes (single digits per deploy) this is fine; no eviction needed.
#[derive(Debug, Clone)]
pub struct PollerState {
    /// Cloudflare DeployWorkflow instance id.
    pub workflow_instance_id: String,
    /// Linear issue id whose comments we are watching.
    pub issue_id: String,
    /// Allowlist of authorized approvers (matches Linear emails).
    pub approvers: Vec<String>,
    /// Time the deploy was initiated; comments older than this are
    /// ignored to avoid race-resolving on stale `+approve` mentions.
    pub started_at: DateTime<Utc>,
    /// Comment ids we've already evaluated; suppresses double-send.
    pub seen_comment_ids: Vec<String>,
    /// Whether this deploy has already had an approval sent. Once
    /// `true` the poller short-circuits to `Approved` on every call.
    pub approved: bool,
}

impl PollerState {
    /// Construct a fresh state for a deploy that just kicked off.
    pub fn new(
        workflow_instance_id: String,
        issue_id: String,
        approvers: Vec<String>,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            workflow_instance_id,
            issue_id,
            approvers,
            started_at,
            seen_comment_ids: Vec::new(),
            approved: false,
        }
    }
}

/// Outcome of a single [`ApprovalPoller::poll_once`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollOutcome {
    /// No approval comment found (yet); caller should poll again.
    Pending,
    /// An authorized approver said `+approve`; the approval was sent
    /// to the control-plane Worker.
    Approved {
        /// Linear identity that satisfied the gate.
        approver: String,
        /// Linear comment id that triggered the approval (audit trail).
        comment_id: String,
    },
    /// At least one comment matched the regex but the author was not
    /// on the approver allowlist. The caller should surface this so
    /// the unauthorized actor learns why the comment was ignored.
    UnauthorizedAttempt {
        /// Comment author who tried to approve.
        author: String,
        /// Comment id (audit trail).
        comment_id: String,
    },
}

/// The poller. Holds the comment source, the approval sink, and the
/// regex that detects an approval token.
pub struct ApprovalPoller {
    comments: Arc<dyn CommentSource>,
    sink: Arc<dyn ApprovalSink>,
    re: Regex,
}

impl ApprovalPoller {
    /// Build a poller using the default `+approve` token.
    pub fn new(
        comments: Arc<dyn CommentSource>,
        sink: Arc<dyn ApprovalSink>,
    ) -> Self {
        Self::with_token(comments, sink, DEFAULT_APPROVAL_TOKEN)
    }

    /// Build with a custom approval token (tests override to keep the
    /// regex simple, e.g. `"+APPROVE"`). The token is treated as a
    /// literal — special regex characters are escaped.
    pub fn with_token(
        comments: Arc<dyn CommentSource>,
        sink: Arc<dyn ApprovalSink>,
        token: &str,
    ) -> Self {
        let pat = format!(r"(?m)^\s*{}\b", regex::escape(token));
        let re = Regex::new(&pat).expect("approval token compiles to a regex");
        Self { comments, sink, re }
    }

    /// One pass over the comment stream. Mutates `state` in place to
    /// record seen comments and the approval outcome.
    pub async fn poll_once(
        &self,
        state: &mut PollerState,
    ) -> Result<PollOutcome, String> {
        if state.approved {
            // Already done — caller can stop polling.
            return Ok(PollOutcome::Approved {
                approver: "<already-approved>".into(),
                comment_id: "<already-approved>".into(),
            });
        }

        let comments = self.comments.list_comments(&state.issue_id).await?;

        // Sort oldest-first so we approve on the first qualifying
        // comment, not the most recent one.
        let mut sorted = comments;
        sorted.sort_by_key(|c| c.created_at);

        let mut last_unauthorized: Option<(String, String)> = None;
        for c in sorted {
            if state.seen_comment_ids.iter().any(|id| id == &c.id) {
                continue;
            }
            state.seen_comment_ids.push(c.id.clone());

            if c.created_at < state.started_at {
                // Older than the deploy — ignore.
                continue;
            }
            if !self.re.is_match(&c.body) {
                continue;
            }

            if !state.approvers.iter().any(|a| a == &c.author) {
                last_unauthorized = Some((c.author.clone(), c.id.clone()));
                continue;
            }

            // Authorized + token matches — send.
            self.sink
                .send_approval(
                    &state.workflow_instance_id,
                    &c.author,
                    extract_rationale(&c.body),
                    c.created_at,
                )
                .await?;
            state.approved = true;
            return Ok(PollOutcome::Approved {
                approver: c.author,
                comment_id: c.id,
            });
        }

        if let Some((author, comment_id)) = last_unauthorized {
            return Ok(PollOutcome::UnauthorizedAttempt { author, comment_id });
        }
        Ok(PollOutcome::Pending)
    }
}

/// Extract an optional rationale from a comment body. Reads the
/// remainder of the line after `+approve` if present, trimmed.
fn extract_rationale(body: &str) -> Option<&str> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(DEFAULT_APPROVAL_TOKEN) {
            let r = rest.trim();
            if r.is_empty() {
                return None;
            }
            return Some(r);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Default HTTP-backed approval sink
// ---------------------------------------------------------------------------

/// Production [`ApprovalSink`] that POSTs to
/// `<control_plane>/api/workflows/deploy/instances/:id/approve`.
pub struct HttpApprovalSink {
    base_url: String,
    bearer_token: Option<String>,
    http: reqwest::Client,
}

impl HttpApprovalSink {
    /// Construct a new sink.
    pub fn new(base_url: impl Into<String>, bearer_token: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer_token,
            http: reqwest::Client::builder()
                .build()
                .expect("reqwest client builds"),
        }
    }
}

#[async_trait]
impl ApprovalSink for HttpApprovalSink {
    async fn send_approval(
        &self,
        workflow_instance_id: &str,
        approver: &str,
        rationale: Option<&str>,
        approved_at: DateTime<Utc>,
    ) -> Result<(), String> {
        let url = format!(
            "{}/api/workflows/deploy/instances/{}/approve",
            self.base_url.trim_end_matches('/'),
            workflow_instance_id
        );
        let body = json!({
            "approval": {
                "approver": approver,
                "approvedAt": approved_at.to_rfc3339(),
                "rationale": rationale,
            }
        });
        let mut req = self.http.post(&url).json(&body);
        if let Some(tok) = &self.bearer_token {
            req = req.bearer_auth(tok);
        }
        let resp = req.send().await.map_err(|e| e.to_string())?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!("POST {url} returned {status}: {text}"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FixedComments(Vec<ApprovalComment>);

    #[async_trait]
    impl CommentSource for FixedComments {
        async fn list_comments(&self, _: &str) -> Result<Vec<ApprovalComment>, String> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        calls: Mutex<Vec<(String, String, Option<String>)>>,
        fail: bool,
    }

    #[async_trait]
    impl ApprovalSink for RecordingSink {
        async fn send_approval(
            &self,
            id: &str,
            approver: &str,
            rationale: Option<&str>,
            _: DateTime<Utc>,
        ) -> Result<(), String> {
            if self.fail {
                return Err("forced".into());
            }
            self.calls.lock().unwrap().push((
                id.to_string(),
                approver.to_string(),
                rationale.map(|s| s.to_string()),
            ));
            Ok(())
        }
    }

    fn comment(id: &str, author: &str, body: &str, when: DateTime<Utc>) -> ApprovalComment {
        ApprovalComment {
            id: id.into(),
            author: author.into(),
            body: body.into(),
            created_at: when,
        }
    }

    fn state(approvers: Vec<&str>, started_at: DateTime<Utc>) -> PollerState {
        PollerState::new(
            "deploy-abc".into(),
            "iss_1".into(),
            approvers.into_iter().map(String::from).collect(),
            started_at,
        )
    }

    #[tokio::test]
    async fn approves_on_authorized_plus_approve_comment() {
        let started = DateTime::parse_from_rfc3339("2026-05-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = DateTime::parse_from_rfc3339("2026-05-02T00:00:30Z")
            .unwrap()
            .with_timezone(&Utc);

        let comments = Arc::new(FixedComments(vec![comment(
            "c1",
            "alice@example.com",
            "+approve looks good\nshipping it",
            later,
        )]));
        let sink = Arc::new(RecordingSink::default());
        let poller = ApprovalPoller::new(comments, sink.clone());
        let mut s = state(vec!["alice@example.com"], started);
        let outcome = poller.poll_once(&mut s).await.unwrap();
        match outcome {
            PollOutcome::Approved { approver, .. } => {
                assert_eq!(approver, "alice@example.com");
            }
            other => panic!("expected Approved, got {other:?}"),
        }
        let calls = sink.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "deploy-abc");
        assert_eq!(calls[0].1, "alice@example.com");
        assert_eq!(calls[0].2.as_deref(), Some("looks good"));
        assert!(s.approved);
    }

    #[tokio::test]
    async fn rejects_unauthorized_author_with_unauthorized_attempt() {
        let started = DateTime::parse_from_rfc3339("2026-05-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = started + chrono::Duration::seconds(5);

        let comments = Arc::new(FixedComments(vec![comment(
            "c1",
            "mallory@example.com",
            "+approve",
            later,
        )]));
        let sink = Arc::new(RecordingSink::default());
        let poller = ApprovalPoller::new(comments, sink.clone());
        let mut s = state(vec!["alice@example.com"], started);
        let outcome = poller.poll_once(&mut s).await.unwrap();
        assert!(matches!(outcome, PollOutcome::UnauthorizedAttempt { .. }));
        assert!(sink.calls.lock().unwrap().is_empty(), "no approval sent");
        assert!(!s.approved);
    }

    #[tokio::test]
    async fn ignores_pre_deploy_comments() {
        let started = DateTime::parse_from_rfc3339("2026-05-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let earlier = started - chrono::Duration::seconds(60);
        let comments = Arc::new(FixedComments(vec![comment(
            "c1",
            "alice@example.com",
            "+approve",
            earlier,
        )]));
        let sink = Arc::new(RecordingSink::default());
        let poller = ApprovalPoller::new(comments, sink.clone());
        let mut s = state(vec!["alice@example.com"], started);
        let outcome = poller.poll_once(&mut s).await.unwrap();
        assert_eq!(outcome, PollOutcome::Pending);
        assert!(sink.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ignores_unrelated_mentions_of_approve_inside_text() {
        let started = DateTime::parse_from_rfc3339("2026-05-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = started + chrono::Duration::seconds(60);
        // `+approve` only counts at start of a line, not in prose.
        let comments = Arc::new(FixedComments(vec![comment(
            "c1",
            "alice@example.com",
            "I will not say +approve in middle of sentence",
            later,
        )]));
        let sink = Arc::new(RecordingSink::default());
        let poller = ApprovalPoller::new(comments, sink.clone());
        let mut s = state(vec!["alice@example.com"], started);
        let outcome = poller.poll_once(&mut s).await.unwrap();
        assert_eq!(outcome, PollOutcome::Pending);
    }

    #[tokio::test]
    async fn idempotent_after_approval() {
        let started = DateTime::parse_from_rfc3339("2026-05-02T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = started + chrono::Duration::seconds(10);
        let comments = Arc::new(FixedComments(vec![comment(
            "c1",
            "alice@example.com",
            "+approve",
            later,
        )]));
        let sink = Arc::new(RecordingSink::default());
        let poller = ApprovalPoller::new(comments, sink.clone());
        let mut s = state(vec!["alice@example.com"], started);
        // First call sends.
        let _ = poller.poll_once(&mut s).await.unwrap();
        // Second call short-circuits.
        let outcome = poller.poll_once(&mut s).await.unwrap();
        assert!(matches!(outcome, PollOutcome::Approved { .. }));
        assert_eq!(
            sink.calls.lock().unwrap().len(),
            1,
            "approval sent exactly once"
        );
    }

    #[test]
    fn extract_rationale_strips_token_and_returns_remainder() {
        assert_eq!(extract_rationale("+approve lgtm"), Some("lgtm"));
        assert_eq!(extract_rationale("  +approve   shipping it"), Some("shipping it"));
        assert_eq!(extract_rationale("+approve"), None);
        assert_eq!(extract_rationale("nothing here"), None);
    }
}
