// SPDX-License-Identifier: AGPL-3.0-only
//
// End-to-end tests for `HelmCloudClient`. Spins up a tiny `axum` server
// that mimics the helm-cloud control-plane surface (`POST /api/auth/session`
// + `POST /api/sessions` + `GET /api/sessions/:id/ws`) and asserts the
// client builds the correct requests with the correct headers.

#![cfg(not(target_family = "wasm"))]

use std::net::SocketAddr;
use std::sync::{Arc, Once};

static CRYPTO_INIT: Once = Once::new();

fn install_crypto_provider() {
    CRYPTO_INIT.call_once(|| {
        // The workspace's reqwest is built with
        // `rustls-tls-native-roots-no-provider`, so the test process
        // must install a rustls crypto provider before the first
        // `Client::new()`.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

use axum::extract::{Path, State, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use helm_cloud_client::{
    CreateSessionArgs, HelmAuth, HelmAuthSource, HelmCloudClient, TaskEvent,
};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Clone, Default)]
struct CapturedReq {
    auth_header: Option<String>,
    body: Option<serde_json::Value>,
    ws_headers: HeaderMap,
}

#[derive(Clone, Default)]
struct AppState {
    captured: Arc<Mutex<CapturedReq>>,
}

async fn auth_session(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut cap = state.captured.lock().await;
    cap.auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok().map(str::to_owned));
    cap.body = Some(body);
    drop(cap);
    Json(json!({
        "session_jwt": "test.helm.jwt",
        // Far-future expiry so the test never bothers to refresh.
        "expires_at": "2099-01-01T00:00:00Z",
    }))
}

async fn create_session(
    headers: HeaderMap,
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let mut cap = state.captured.lock().await;
    cap.auth_header = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok().map(str::to_owned));
    cap.body = Some(body);
    drop(cap);
    Json(json!({ "session_id": "sess_test_42" }))
}

async fn ws_handler(
    Path(_id): Path<String>,
    headers: HeaderMap,
    State(state): State<AppState>,
    ws: WebSocketUpgrade,
) -> Response {
    {
        let mut cap = state.captured.lock().await;
        cap.ws_headers = headers.clone();
    }
    ws.on_upgrade(|mut socket| async move {
        // Push one event then close.
        let ev = json!({
            "kind": "session_started",
            "session_id": "sess_test_42",
        });
        let _ = socket
            .send(axum::extract::ws::Message::Text(ev.to_string().into()))
            .await;
        let _ = SinkExt::close(&mut socket).await;
    })
}

async fn spawn_server() -> (SocketAddr, AppState) {
    install_crypto_provider();
    let state = AppState::default();
    let app = Router::new()
        .route("/api/auth/session", post(auth_session))
        .route("/api/sessions", post(create_session))
        .route("/api/sessions/{id}/ws", axum::routing::get(ws_handler))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, state)
}

#[tokio::test]
async fn create_session_sends_bearer_and_args() {
    let (addr, captured) = spawn_server().await;
    let base = format!("http://{addr}");
    let auth = HelmAuth::new(&base);
    auth.set_source(HelmAuthSource::Cloudflare {
        access_jwt: "cf-access-jwt".into(),
    })
    .await;
    let client = HelmCloudClient::new(&base, auth);
    let id = client
        .create_session(CreateSessionArgs {
            repo_url: "https://github.com/owner/repo".into(),
            branch: Some("main".into()),
            prompt: "fix the build".into(),
            env: vec![("FOO".into(), "bar".into())],
            user_id: Some("user_1".into()),
        })
        .await
        .unwrap();
    assert_eq!(id.0, "sess_test_42");
    let cap = captured.captured.lock().await;
    assert_eq!(
        cap.auth_header.as_deref(),
        Some("Bearer test.helm.jwt"),
        "create_session must use exchanged helm session JWT"
    );
    let body = cap.body.as_ref().unwrap();
    assert_eq!(body["repo_url"], "https://github.com/owner/repo");
    assert_eq!(body["branch"], "main");
    assert_eq!(body["prompt"], "fix the build");
    assert_eq!(body["user_id"], "user_1");
}

#[tokio::test]
async fn jwt_exchange_round_trip_with_doppler_token() {
    let (addr, captured) = spawn_server().await;
    let base = format!("http://{addr}");
    let auth = HelmAuth::new(&base);
    auth.set_source(HelmAuthSource::Doppler {
        token: "dp.st.fake".into(),
    })
    .await;
    let _ = auth.session_jwt().await.unwrap();
    let cap = captured.captured.lock().await;
    let body = cap.body.as_ref().unwrap();
    assert_eq!(body["doppler_token"], "dp.st.fake");
    assert!(body.get("access_jwt").map(|v| v.is_null()).unwrap_or(true));
}

#[tokio::test]
async fn ws_upgrade_includes_helm_attribution_headers() {
    let (addr, captured) = spawn_server().await;
    let base = format!("http://{addr}");
    let auth = HelmAuth::new(&base);
    auth.set_source(HelmAuthSource::Cloudflare {
        access_jwt: "x".into(),
    })
    .await;
    let client = HelmCloudClient::new(&base, auth);
    let id = client
        .create_session(CreateSessionArgs {
            repo_url: "https://github.com/o/r".into(),
            branch: None,
            prompt: "x".into(),
            env: vec![],
            user_id: None,
        })
        .await
        .unwrap();
    let mut stream = client.connect_session_ws(&id).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert!(matches!(first, TaskEvent::SessionStarted { .. }));
    let cap = captured.captured.lock().await;
    assert_eq!(
        cap.ws_headers
            .get("x-helm-session-id")
            .and_then(|v| v.to_str().ok()),
        Some("sess_test_42"),
        "WS upgrade must carry x-helm-session-id"
    );
    assert_eq!(
        cap.ws_headers
            .get("x-helm-user-id")
            .and_then(|v| v.to_str().ok()),
        Some("warp-client"),
    );
    let auth_h = cap.ws_headers.get("authorization").unwrap().to_str().unwrap();
    assert!(
        auth_h.starts_with("Bearer "),
        "WS upgrade must carry Bearer token, got {auth_h:?}"
    );
}

#[tokio::test]
async fn status_reflects_last_create() {
    let (addr, _) = spawn_server().await;
    let base = format!("http://{addr}");
    let auth = HelmAuth::new(&base);
    auth.set_source(HelmAuthSource::Cloudflare {
        access_jwt: "x".into(),
    })
    .await;
    let client = HelmCloudClient::new(&base, auth);
    assert!(client.status().await.last_create_at.is_none());
    let _ = client
        .create_session(CreateSessionArgs {
            repo_url: "r".into(),
            branch: None,
            prompt: "p".into(),
            env: vec![],
            user_id: None,
        })
        .await
        .unwrap();
    assert!(client.status().await.last_create_at.is_some());
}
