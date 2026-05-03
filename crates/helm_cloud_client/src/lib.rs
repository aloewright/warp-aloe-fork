// SPDX-License-Identifier: AGPL-3.0-only
//
// PDX-119 [E7] — helm-cloud control-plane client.
//
// Thin native HTTP + WebSocket client that talks to the helm-cloud Hono
// Worker shipped by PDX-19/20/21/22/23. The client is the user-facing fix
// for the "Your team has run out of add-on credits" wall: when the
// `route_cloud_env_through_helm` toggle is on, the cloud-environment UI
// calls `HelmCloudClient::create_session` and streams `TaskEvent`s back
// through a `connect_session_ws` WebSocket — completely bypassing the
// Warp-hosted GraphQL surface.
//
// The crate is intentionally self-contained:
//   * `config`  — `~/.warp/helm_cloud.toml` load/store (default toggle ON
//                 when `warp_hosted=false`).
//   * `auth`    — Cloudflare Access JWT / Doppler service token →
//                 helm session JWT exchange via `POST /api/auth/session`,
//                 with in-memory caching.
//   * `client`  — `HelmCloudClient::create_session`, `connect_session_ws`,
//                 plus `status()` for the settings UI status row.
//   * `event`   — `TaskEvent` mirrors the relevant subset of
//                 `cloud_protocol` so we don't pull the whole crate in.
//   * `audit`   — `cloud_env_routed` JSONL row emitter; symphony's
//                 `AuditLog` enum is closed, so this writes a parallel
//                 line with the rule + detail fields the spec requires.
//
// Native-only. The whole crate sits behind `cfg(not(target_family =
// "wasm"))` at every call site — `tokio-tungstenite` and on-disk config
// are not portable to the browser bundle.

#![cfg(not(target_family = "wasm"))]

pub mod audit;
pub mod auth;
pub mod client;
pub mod config;
pub mod event;

pub use audit::{record_cloud_env_routed, CloudEnvRoutedDetail};
pub use auth::{HelmAuth, HelmAuthError, HelmAuthSource};
pub use client::{
    ClientStatus, CreateSessionArgs, HelmCloudClient, HelmCloudError, SessionId, SessionStream,
};
pub use config::{load_helm_cloud_config, save_helm_cloud_config, HelmCloudConfig};
pub use event::TaskEvent;
