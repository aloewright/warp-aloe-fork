// SPDX-License-Identifier: AGPL-3.0-only
//
// HTTP + WebSocket client for the helm-cloud control plane.

use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest,
    http::header::{HeaderName, HeaderValue},
    Message,
};

use crate::audit::{record_cloud_env_routed, CloudEnvRoutedDetail};
use crate::auth::{HelmAuth, HelmAuthError};
use crate::event::TaskEvent;

/// Opaque session id returned by `POST /api/sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Args mirroring what the existing Warp-hosted GraphQL request sent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionArgs {
    /// Repository to clone (e.g. `https://github.com/owner/repo`).
    pub repo_url: String,
    /// Optional branch override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Initial agent prompt.
    pub prompt: String,
    /// Environment variables to populate in the agent-runtime container.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Optional helm-side user id; if omitted, the SessionDO derives it
    /// from the helm session JWT.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
}

/// Wire shape returned by `POST /api/sessions`.
#[derive(Debug, Deserialize)]
struct CreateSessionResponse {
    session_id: String,
}

#[derive(Debug, Error)]
pub enum HelmCloudError {
    #[error("auth: {0}")]
    Auth(#[from] HelmAuthError),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("helm-cloud returned non-success status {0}: {1}")]
    Status(u16, String),
    #[error("malformed response: {0}")]
    Decode(String),
    #[error("ws: {0}")]
    WebSocket(String),
    #[error("invalid base_url: {0}")]
    BadUrl(String),
}

/// Exposed for the settings UI status row.
#[derive(Debug, Clone, Default)]
pub struct ClientStatus {
    pub last_create_at: Option<DateTime<Utc>>,
    pub in_flight_sessions: usize,
}

/// Live status the UI polls.
#[derive(Default)]
struct StatusInner {
    last_create_at: Option<DateTime<Utc>>,
    in_flight_sessions: usize,
}

/// Boxed stream of `TaskEvent`s coming off the WebSocket.
pub type SessionStream =
    Pin<Box<dyn Stream<Item = Result<TaskEvent, HelmCloudError>> + Send + 'static>>;

/// Thin client. Native-only.
#[derive(Clone)]
pub struct HelmCloudClient {
    inner: Arc<Inner>,
}

struct Inner {
    base_url: String,
    auth: HelmAuth,
    http: reqwest::Client,
    status: Mutex<StatusInner>,
}

impl HelmCloudClient {
    /// Build a client. The `HelmAuth` is the JWT provider — call
    /// `HelmAuth::set_source` before issuing any request.
    pub fn new(base_url: impl Into<String>, auth: HelmAuth) -> Self {
        Self::with_client(base_url, auth, reqwest::Client::new())
    }

    /// Test seam.
    pub fn with_client(
        base_url: impl Into<String>,
        auth: HelmAuth,
        http: reqwest::Client,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                base_url: base_url.into().trim_end_matches('/').to_string(),
                auth,
                http,
                status: Mutex::new(StatusInner::default()),
            }),
        }
    }

    /// `POST /api/sessions`. Returns the helm-cloud session id.
    pub async fn create_session(
        &self,
        args: CreateSessionArgs,
    ) -> Result<SessionId, HelmCloudError> {
        let jwt = self.inner.auth.session_jwt().await?;
        let url = format!("{}/api/sessions", self.inner.base_url);
        let resp = self
            .inner
            .http
            .post(&url)
            .bearer_auth(&jwt)
            .json(&args)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(HelmCloudError::Status(status.as_u16(), text));
        }
        let parsed: CreateSessionResponse = resp
            .json()
            .await
            .map_err(|e| HelmCloudError::Decode(e.to_string()))?;
        let session_id = SessionId(parsed.session_id.clone());

        // Audit + bookkeeping.
        record_cloud_env_routed(CloudEnvRoutedDetail {
            helm_cloud_base_url: self.inner.base_url.clone(),
            session_id: parsed.session_id.clone(),
        });
        {
            let mut st = self.inner.status.lock().await;
            st.last_create_at = Some(Utc::now());
        }
        Ok(session_id)
    }

    /// Open a WebSocket against `/api/sessions/:id/ws` and return a
    /// stream of `TaskEvent`s. The connection is upgraded with the
    /// helm session JWT in `Authorization`, plus `x-helm-user-id` and
    /// `x-helm-session-id` so the SessionDO's `withAuditAttribution`
    /// middleware tags every emitted event.
    pub async fn connect_session_ws(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionStream, HelmCloudError> {
        let jwt = self.inner.auth.session_jwt().await?;

        // Translate http(s):// -> ws(s)://.
        let mut url = url::Url::parse(&self.inner.base_url)
            .map_err(|e| HelmCloudError::BadUrl(e.to_string()))?;
        let new_scheme = match url.scheme() {
            "http" => "ws",
            "https" => "wss",
            other => return Err(HelmCloudError::BadUrl(format!("unsupported scheme {other}"))),
        };
        url.set_scheme(new_scheme)
            .map_err(|_| HelmCloudError::BadUrl("scheme rewrite failed".into()))?;
        url.path_segments_mut()
            .map_err(|_| HelmCloudError::BadUrl("cannot mutate path".into()))?
            .extend(&["api", "sessions", &session_id.0, "ws"]);

        // Build the upgrade request with our auth + attribution headers.
        let mut req = url
            .as_str()
            .into_client_request()
            .map_err(|e| HelmCloudError::WebSocket(e.to_string()))?;
        let headers = req.headers_mut();
        let auth_value = HeaderValue::from_str(&format!("Bearer {jwt}"))
            .map_err(|e| HelmCloudError::WebSocket(e.to_string()))?;
        headers.insert(HeaderName::from_static("authorization"), auth_value);
        // The user id rides in the JWT; we surface a session-scoped
        // value here so the audit middleware can tag streamed events
        // even before it decodes the JWT.
        headers.insert(
            HeaderName::from_static("x-helm-user-id"),
            HeaderValue::from_static("warp-client"),
        );
        let sess_value = HeaderValue::from_str(&session_id.0)
            .map_err(|e| HelmCloudError::WebSocket(e.to_string()))?;
        headers.insert(HeaderName::from_static("x-helm-session-id"), sess_value);

        let (ws, _resp) = tokio_tungstenite::connect_async(req)
            .await
            .map_err(|e| HelmCloudError::WebSocket(e.to_string()))?;

        // Bookkeeping for status row.
        {
            let mut st = self.inner.status.lock().await;
            st.in_flight_sessions = st.in_flight_sessions.saturating_add(1);
        }
        let status_handle = self.inner.clone();

        let stream = async_stream::stream! {
            tokio::pin!(ws);
            while let Some(msg) = ws.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<TaskEvent>(&text) {
                            Ok(ev) => yield Ok(ev),
                            Err(e) => yield Err(HelmCloudError::Decode(e.to_string())),
                        }
                    }
                    Ok(Message::Binary(_)) | Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                        continue;
                    }
                    Ok(Message::Close(_)) => break,
                    Ok(Message::Frame(_)) => continue,
                    Err(e) => {
                        yield Err(HelmCloudError::WebSocket(e.to_string()));
                        break;
                    }
                }
            }
            // decrement on stream drop
            let mut st = status_handle.status.lock().await;
            st.in_flight_sessions = st.in_flight_sessions.saturating_sub(1);
        };

        Ok(Box::pin(stream))
    }

    /// Snapshot of last-create timestamp + in-flight count, for the
    /// settings UI status row.
    pub async fn status(&self) -> ClientStatus {
        let st = self.inner.status.lock().await;
        ClientStatus {
            last_create_at: st.last_create_at,
            in_flight_sessions: st.in_flight_sessions,
        }
    }

    /// Plumbed through for callers (e.g. settings UI) that want the
    /// configured base URL without re-reading the toml.
    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }
}

