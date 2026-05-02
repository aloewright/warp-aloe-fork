//! GitHub webhook receiver for the Symphony daemon (PDX-26 D3).
//!
//! Implements the `server` extension referenced by Symphony's spec §13.7:
//! a small Axum HTTP server with HMAC-SHA256 signature validation that
//! accepts GitHub webhook deliveries (`pull_request`,
//! `pull_request_review`, `issues`, `push`) and forwards them as structured
//! [`WebhookEvent`]s on a tokio mpsc channel.
//!
//! In addition to GitHub, two adjacent endpoints are exposed for symmetry
//! with the Linear ticket scope:
//!
//! * `POST /webhook/slack`   — Slack-style HMAC (`v0=...`) verification.
//! * `POST /webhook/generic` — generic signed POST → enqueue an arbitrary
//!   payload, useful for in-house automation that shouldn't pretend to be
//!   GitHub or Slack.
//!
//! The receiver intentionally does NOT touch Symphony state directly; it
//! emits events on an mpsc channel and the daemon decides how to fan them
//! out (typically: create-or-update a Linear issue, which Symphony picks up
//! on the next poll tick — no new orchestrator state needed).
//!
//! ## Security
//!
//! * GitHub: validates `X-Hub-Signature-256` using the configured
//!   `webhook_secret`. Constant-time comparison.
//! * Slack: validates `X-Slack-Signature` against the v0 string scheme.
//! * Generic: validates `X-Webhook-Signature` (hex-encoded HMAC-SHA256).
//! * All signatures are verified BEFORE the JSON body is deserialized so
//!   that malformed payloads can never reach event consumers without
//!   passing crypto.
//!
//! ## Non-portable surface
//!
//! Tokio + Axum, so this crate is gated to non-WASM targets via the build
//! graph (it's only a member of the host workspace, not the WASM bundle).

#![deny(missing_docs)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Router;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::mpsc;

/// Canonical event shape emitted by the receiver. The `kind` discriminant
/// drives Symphony's translation into Linear issues / agent dispatches.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebhookEvent {
    /// `pull_request` event from GitHub. Action is one of `opened`,
    /// `closed`, `reopened`, `synchronize`, etc.
    GithubPullRequest {
        /// PR action.
        action: String,
        /// Repository full name (`owner/repo`).
        repo: String,
        /// PR number.
        number: u64,
        /// PR title.
        title: String,
        /// PR HTML URL.
        url: String,
        /// `true` if the PR is now in `merged` state.
        merged: bool,
    },
    /// `pull_request_review` event.
    GithubPullRequestReview {
        /// Review action (`submitted`, `edited`, `dismissed`).
        action: String,
        /// `approved`, `changes_requested`, or `commented`.
        state: String,
        /// Repository full name.
        repo: String,
        /// PR number.
        number: u64,
        /// Review HTML URL.
        url: String,
    },
    /// `issues` event.
    GithubIssue {
        /// Issue action (`opened`, `closed`, `reopened`, etc).
        action: String,
        /// Repository full name.
        repo: String,
        /// Issue number.
        number: u64,
        /// Issue title.
        title: String,
        /// Issue HTML URL.
        url: String,
    },
    /// `push` event.
    GithubPush {
        /// Repository full name.
        repo: String,
        /// Git ref pushed to (e.g. `refs/heads/main`).
        ref_: String,
        /// Number of commits in this push.
        commits: u64,
        /// Pushed-by user login.
        pusher: String,
    },
    /// Slack slash command or event.
    Slack {
        /// `/foo` command name or event subtype.
        command: String,
        /// Channel ID.
        channel: String,
        /// User ID.
        user: String,
        /// Free-form text payload.
        text: String,
    },
    /// Generic HMAC-signed payload — Symphony decides the schema.
    Generic {
        /// Raw JSON body, kept as a value so callers can pull what they want.
        payload: serde_json::Value,
    },
}

/// Wrapper carrying a [`WebhookEvent`] and a UTC timestamp marking when the
/// receiver enqueued it. Useful for audit logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceivedEvent {
    /// Decoded event.
    pub event: WebhookEvent,
    /// When the event was received (server-side clock).
    pub received_at: DateTime<Utc>,
}

/// Receiver configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiverConfig {
    /// Address to bind. Defaults to `127.0.0.1:9278` (next port up from
    /// the existing in-app HttpServer at 9277).
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// HMAC secret for `/webhook/github`. Empty disables the route.
    #[serde(default)]
    pub github_secret: String,
    /// HMAC secret for `/webhook/slack`. Empty disables the route.
    #[serde(default)]
    pub slack_secret: String,
    /// HMAC secret for `/webhook/generic`. Empty disables the route.
    #[serde(default)]
    pub generic_secret: String,
}

impl Default for ReceiverConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            github_secret: String::new(),
            slack_secret: String::new(),
            generic_secret: String::new(),
        }
    }
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], 9278))
}

/// Receiver errors.
#[derive(Debug, Error)]
pub enum ReceiverError {
    /// Failure to bind the TCP listener.
    #[error("bind {addr}: {source}")]
    Bind {
        /// Bound address that failed.
        addr: SocketAddr,
        /// Underlying io error.
        #[source]
        source: std::io::Error,
    },
    /// Axum serve loop exited unexpectedly.
    #[error("serve error: {0}")]
    Serve(#[from] std::io::Error),
}

/// Build an [`axum::Router`] for the configured receiver. Useful when a
/// caller wants to compose this with their own router or with the existing
/// `crates/http_server` entry point.
pub fn router(config: ReceiverConfig, sender: mpsc::Sender<ReceivedEvent>) -> Router {
    let state = Arc::new(AppState { config, sender });
    Router::new()
        .route("/healthz", get(healthz))
        .route("/webhook/github", post(github_handler))
        .route("/webhook/slack", post(slack_handler))
        .route("/webhook/generic", post(generic_handler))
        .with_state(state)
}

/// Spawn the receiver on its configured bind address. Returns the bound
/// address (useful when the caller passed `:0` to pick a port at random)
/// plus a join handle.
pub async fn serve(
    config: ReceiverConfig,
    sender: mpsc::Sender<ReceivedEvent>,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), ReceiverError> {
    let bind = config.bind;
    let app = router(config, sender);
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(|source| ReceiverError::Bind { addr: bind, source })?;
    let local = listener
        .local_addr()
        .map_err(|source| ReceiverError::Bind { addr: bind, source })?;
    let handle = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::warn!(error = %e, "github_webhook_receiver: serve loop exited");
        }
    });
    Ok((local, handle))
}

#[derive(Clone)]
struct AppState {
    config: ReceiverConfig,
    sender: mpsc::Sender<ReceivedEvent>,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn github_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if state.config.github_secret.is_empty() {
        return (StatusCode::FORBIDDEN, "github webhook disabled").into_response();
    }
    let sig = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing signature").into_response(),
    };
    let expected = format!(
        "sha256={}",
        hmac_sha256_hex(state.config.github_secret.as_bytes(), &body)
    );
    if !constant_time_eq(sig.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let event_type = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };
    let event = match parse_github_event(event_type, &parsed) {
        Some(e) => e,
        None => {
            // Unsupported but valid event type — ack with 202 so GitHub
            // doesn't keep retrying, but don't enqueue.
            return (StatusCode::ACCEPTED, "ignored").into_response();
        }
    };
    enqueue(&state.sender, event).await
}

async fn slack_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if state.config.slack_secret.is_empty() {
        return (StatusCode::FORBIDDEN, "slack webhook disabled").into_response();
    }
    let sig = match headers
        .get("x-slack-signature")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing signature").into_response(),
    };
    let ts = match headers
        .get("x-slack-request-timestamp")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing timestamp").into_response(),
    };
    // Slack v0: HMAC-SHA256 over "v0:{timestamp}:{body}" with hex digest,
    // then prefixed with "v0=".
    let mut signing = Vec::with_capacity(4 + ts.len() + 1 + body.len());
    signing.extend_from_slice(b"v0:");
    signing.extend_from_slice(ts.as_bytes());
    signing.push(b':');
    signing.extend_from_slice(&body);
    let expected = format!(
        "v0={}",
        hmac_sha256_hex(state.config.slack_secret.as_bytes(), &signing)
    );
    if !constant_time_eq(sig.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };
    let command = parsed
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let channel = parsed
        .get("channel_id")
        .or_else(|| parsed.get("channel"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let user = parsed
        .get("user_id")
        .or_else(|| parsed.get("user"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let text = parsed
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    enqueue(
        &state.sender,
        WebhookEvent::Slack {
            command,
            channel,
            user,
            text,
        },
    )
    .await
}

async fn generic_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    if state.config.generic_secret.is_empty() {
        return (StatusCode::FORBIDDEN, "generic webhook disabled").into_response();
    }
    let sig = match headers
        .get("x-webhook-signature")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s,
        None => return (StatusCode::UNAUTHORIZED, "missing signature").into_response(),
    };
    let expected = hmac_sha256_hex(state.config.generic_secret.as_bytes(), &body);
    if !constant_time_eq(sig.as_bytes(), expected.as_bytes()) {
        return (StatusCode::UNAUTHORIZED, "bad signature").into_response();
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid json").into_response(),
    };
    enqueue(&state.sender, WebhookEvent::Generic { payload: parsed }).await
}

async fn enqueue(
    sender: &mpsc::Sender<ReceivedEvent>,
    event: WebhookEvent,
) -> axum::response::Response {
    let envelope = ReceivedEvent {
        event,
        received_at: Utc::now(),
    };
    match sender.send(envelope).await {
        Ok(()) => (StatusCode::ACCEPTED, "queued").into_response(),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "consumer disconnected").into_response(),
    }
}

fn parse_github_event(event_type: &str, body: &serde_json::Value) -> Option<WebhookEvent> {
    let repo = body
        .pointer("/repository/full_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let action = body
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    match event_type {
        "pull_request" => {
            let pr = body.get("pull_request")?;
            Some(WebhookEvent::GithubPullRequest {
                action,
                repo,
                number: pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0),
                title: pr
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: pr
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                merged: pr.get("merged").and_then(|v| v.as_bool()).unwrap_or(false),
            })
        }
        "pull_request_review" => {
            let pr = body.get("pull_request")?;
            let review = body.get("review")?;
            Some(WebhookEvent::GithubPullRequestReview {
                action,
                state: review
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                repo,
                number: pr.get("number").and_then(|v| v.as_u64()).unwrap_or(0),
                url: review
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        }
        "issues" => {
            let issue = body.get("issue")?;
            Some(WebhookEvent::GithubIssue {
                action,
                repo,
                number: issue.get("number").and_then(|v| v.as_u64()).unwrap_or(0),
                title: issue
                    .get("title")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                url: issue
                    .get("html_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        }
        "push" => Some(WebhookEvent::GithubPush {
            repo,
            ref_: body
                .get("ref")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            commits: body
                .get("commits")
                .and_then(|v| v.as_array())
                .map(|a| a.len() as u64)
                .unwrap_or(0),
            pusher: body
                .pointer("/pusher/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        _ => None,
    }
}

/// Plain HMAC-SHA256 implemented over `sha2`. We avoid pulling in the
/// `hmac` crate to keep the Symphony dep tree narrow — the construction is
/// well-defined (RFC 2104) and tested against a known vector below.
fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let h = Sha256::digest(key);
        k[..h.len()].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    let out = outer.finalize();
    hex::encode(out)
}

/// Constant-time byte slice equality. Returns `false` if lengths differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// HMAC-SHA256 known-answer test from RFC 4231 test case 1.
    #[test]
    fn hmac_sha256_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let msg = b"Hi There";
        let got = hmac_sha256_hex(&key, msg);
        let want = "b0344c61d8db38535ca8afceaf0bf12b\
                    881dc200c9833da726e9376c2e32cff7";
        assert_eq!(got, want);
    }

    #[test]
    fn constant_time_eq_basic() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }

    #[test]
    fn parse_pull_request_event() {
        let body = serde_json::json!({
            "action": "opened",
            "repository": { "full_name": "octo/repo" },
            "pull_request": {
                "number": 42,
                "title": "Add cool feature",
                "html_url": "https://github.com/octo/repo/pull/42",
                "merged": false,
            }
        });
        let ev = parse_github_event("pull_request", &body).unwrap();
        match ev {
            WebhookEvent::GithubPullRequest {
                action,
                repo,
                number,
                title,
                merged,
                ..
            } => {
                assert_eq!(action, "opened");
                assert_eq!(repo, "octo/repo");
                assert_eq!(number, 42);
                assert_eq!(title, "Add cool feature");
                assert!(!merged);
            }
            _ => panic!("expected GithubPullRequest"),
        }
    }

    #[test]
    fn parse_unknown_event_returns_none() {
        let body = serde_json::json!({});
        assert!(parse_github_event("ping", &body).is_none());
    }

    #[tokio::test]
    async fn github_route_rejects_bad_signature() {
        let (tx, _rx) = mpsc::channel(8);
        let cfg = ReceiverConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            github_secret: "topsecret".into(),
            slack_secret: String::new(),
            generic_secret: String::new(),
        };
        let app = router(cfg, tx);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/webhook/github")
            .header("x-hub-signature-256", "sha256=deadbeef")
            .header("x-github-event", "ping")
            .header("content-type", "application/json")
            .body(axum::body::Body::from("{}".to_string()))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn github_route_disabled_when_secret_empty() {
        let (tx, _rx) = mpsc::channel(8);
        let cfg = ReceiverConfig::default();
        let app = router(cfg, tx);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/webhook/github")
            .body(axum::body::Body::from("{}".to_string()))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn generic_route_accepts_valid_signature() {
        let (tx, mut rx) = mpsc::channel(8);
        let secret = "shh";
        let body = b"{\"hello\":\"world\"}";
        let sig = hmac_sha256_hex(secret.as_bytes(), body);
        let cfg = ReceiverConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            github_secret: String::new(),
            slack_secret: String::new(),
            generic_secret: secret.into(),
        };
        let app = router(cfg, tx);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/webhook/generic")
            .header("x-webhook-signature", sig)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_vec()))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let received = rx.recv().await.expect("event enqueued");
        match received.event {
            WebhookEvent::Generic { payload } => {
                assert_eq!(payload["hello"], "world");
            }
            _ => panic!("expected Generic"),
        }
    }

    #[tokio::test]
    async fn slack_route_validates_v0_signature() {
        let (tx, mut rx) = mpsc::channel(8);
        let secret = "slackshh";
        let body =
            b"{\"command\":\"/deploy\",\"channel_id\":\"C1\",\"user_id\":\"U1\",\"text\":\"now\"}";
        let ts = "1700000000";
        let mut signing = Vec::new();
        signing.extend_from_slice(b"v0:");
        signing.extend_from_slice(ts.as_bytes());
        signing.push(b':');
        signing.extend_from_slice(body);
        let sig = format!("v0={}", hmac_sha256_hex(secret.as_bytes(), &signing));
        let cfg = ReceiverConfig {
            bind: SocketAddr::from(([127, 0, 0, 1], 0)),
            github_secret: String::new(),
            slack_secret: secret.into(),
            generic_secret: String::new(),
        };
        let app = router(cfg, tx);
        let req = axum::http::Request::builder()
            .method("POST")
            .uri("/webhook/slack")
            .header("x-slack-signature", sig)
            .header("x-slack-request-timestamp", ts)
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_vec()))
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let received = rx.recv().await.expect("event enqueued");
        match received.event {
            WebhookEvent::Slack {
                command,
                channel,
                user,
                text,
            } => {
                assert_eq!(command, "/deploy");
                assert_eq!(channel, "C1");
                assert_eq!(user, "U1");
                assert_eq!(text, "now");
            }
            _ => panic!("expected Slack"),
        }
    }

    #[tokio::test]
    async fn healthz_works() {
        let (tx, _rx) = mpsc::channel(8);
        let app = router(ReceiverConfig::default(), tx);
        let req = axum::http::Request::builder()
            .method("GET")
            .uri("/healthz")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
