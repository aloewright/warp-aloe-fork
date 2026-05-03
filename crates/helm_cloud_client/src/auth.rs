// SPDX-License-Identifier: AGPL-3.0-only
//
// JWT exchange against `POST /api/auth/session` (PDX-23).
//
// Two upstream credential sources are supported:
//
//   1. `Cloudflare` — a Cloudflare Access JWT pulled from the OS keychain.
//      Mirrors how Doppler's auth flow stashes its tokens (see
//      `crates/doppler/src/runner.rs`); the keychain item is tagged
//      `warp.helm_cloud.access_jwt`.
//   2. `Doppler`    — a Doppler service token, used in CI and for build
//      scripts that don't have an interactive Cloudflare Access login.
//
// In either case, we POST `{ access_jwt }` (or `{ doppler_token }`) and
// receive back `{ session_jwt, expires_at }`. The exchanged JWT is cached
// in memory and refreshed when expired (PDX-23 sets a 1h TTL).

use std::sync::Arc;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

/// Where the upstream credential comes from.
#[derive(Debug, Clone)]
pub enum HelmAuthSource {
    /// Cloudflare Access JWT (e.g. retrieved via the OS keychain).
    Cloudflare { access_jwt: String },
    /// Doppler service token (CI, build scripts).
    Doppler { token: String },
}

#[derive(Debug, Error)]
pub enum HelmAuthError {
    #[error("helm-cloud auth exchange failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("helm-cloud auth returned non-success status {0}: {1}")]
    Status(u16, String),
    #[error("helm-cloud auth returned malformed JSON: {0}")]
    Decode(String),
    #[error("no upstream credential configured")]
    NoCredential,
}

#[derive(Debug, Serialize)]
struct ExchangeRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    access_jwt: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    doppler_token: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    session_jwt: String,
    /// RFC3339 timestamp when the helm session JWT expires. Optional —
    /// some early helm-cloud builds omit this and we fall back to a 1h
    /// TTL from `now()` to match PDX-23's documented default.
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
struct CachedSession {
    jwt: String,
    expires_at: DateTime<Utc>,
}

/// In-memory helm session JWT holder with refresh-on-expiry.
///
/// `HelmAuth` is `Clone` and threadsafe so the `HelmCloudClient` can hand
/// it to background tasks (the WS reader) without re-bootstrapping.
#[derive(Clone)]
pub struct HelmAuth {
    inner: Arc<HelmAuthInner>,
}

struct HelmAuthInner {
    base_url: String,
    source: Mutex<Option<HelmAuthSource>>,
    cached: Mutex<Option<CachedSession>>,
    http: reqwest::Client,
}

impl HelmAuth {
    /// Build a new `HelmAuth` rooted at the given base URL.
    pub fn new(base_url: impl Into<String>) -> Self {
        Self::with_client(base_url, reqwest::Client::new())
    }

    /// Test seam: inject a pre-built `reqwest::Client`.
    pub fn with_client(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            inner: Arc::new(HelmAuthInner {
                base_url: base_url.into(),
                source: Mutex::new(None),
                cached: Mutex::new(None),
                http,
            }),
        }
    }

    /// Set the upstream credential source. Resets any cached session.
    pub async fn set_source(&self, source: HelmAuthSource) {
        *self.inner.source.lock().await = Some(source);
        *self.inner.cached.lock().await = None;
    }

    /// Return a valid helm session JWT, exchanging upstream creds if the
    /// cache is empty or expired.
    pub async fn session_jwt(&self) -> Result<String, HelmAuthError> {
        // Fast path: a non-expired cached JWT.
        {
            let cached = self.inner.cached.lock().await;
            if let Some(c) = cached.as_ref() {
                // 30s skew buffer so we don't hand out a JWT that expires
                // mid-flight.
                if c.expires_at > Utc::now() + Duration::seconds(30) {
                    return Ok(c.jwt.clone());
                }
            }
        }
        // Slow path: re-exchange.
        let source = self
            .inner
            .source
            .lock()
            .await
            .clone()
            .ok_or(HelmAuthError::NoCredential)?;
        let body = match &source {
            HelmAuthSource::Cloudflare { access_jwt } => ExchangeRequest {
                access_jwt: Some(access_jwt.as_str()),
                doppler_token: None,
            },
            HelmAuthSource::Doppler { token } => ExchangeRequest {
                access_jwt: None,
                doppler_token: Some(token.as_str()),
            },
        };
        let url = format!("{}/api/auth/session", self.inner.base_url);
        let resp = self.inner.http.post(&url).json(&body).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(HelmAuthError::Status(status.as_u16(), text));
        }
        let parsed: ExchangeResponse = resp
            .json()
            .await
            .map_err(|e| HelmAuthError::Decode(e.to_string()))?;
        let expires_at = parsed
            .expires_at
            .unwrap_or_else(|| Utc::now() + Duration::hours(1));
        let new_cached = CachedSession {
            jwt: parsed.session_jwt.clone(),
            expires_at,
        };
        *self.inner.cached.lock().await = Some(new_cached);
        Ok(parsed.session_jwt)
    }

    /// Inspection helper: peek at the cached expiry, if any.
    pub async fn cached_expiry(&self) -> Option<DateTime<Utc>> {
        self.inner.cached.lock().await.as_ref().map(|c| c.expires_at)
    }
}

// Lib-level unit tests for `HelmAuth` are deliberately empty: building a
// `reqwest::Client` requires the workspace's rustls provider to be
// pre-installed, which only happens in app/integration test contexts.
// The `tests/http_client.rs` integration suite spins up a real HTTP
// server and exercises the JWT-exchange path end-to-end, so this code
// is not untested.
