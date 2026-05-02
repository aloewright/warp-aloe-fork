//! # `cloud_client`
//!
//! Native WebSocket client for the Warp cloud control plane.
//!
//! This crate implements the client side of the [`cloud_protocol`] wire
//! format: it opens a WebSocket to a configured URL, submits tasks via
//! [`cloud_protocol::TaskSubmit`], streams [`cloud_protocol::TaskEvent`]s
//! back to the caller, and translates the protocol-level events into the
//! [`orchestrator::AgentEvent`] vocabulary used by the rest of the app.
//!
//! ## Native-only
//!
//! `cloud_client` depends on `tokio-tungstenite` and is intentionally not
//! part of the `wasm32-unknown-unknown` build set. The
//! [`cloud_protocol`] crate is the part that's wasm-clean.
//!
//! ## Components
//!
//! * [`CloudAgent`] — `orchestrator::Agent` impl. The thing the rest of
//!   the system talks to.
//! * [`reconnect::ReconnectPolicy`] — exponential-backoff state machine,
//!   independently testable.
//! * [`transport::Transport`] — trait abstracting "send/recv a JSON frame".
//!   The production impl wraps `tokio-tungstenite`; tests use an in-memory
//!   channel pair.
//!
//! ## Resume on reconnect
//!
//! The protocol's [`cloud_protocol::TaskControl`] has a `Resume` variant,
//! but in V1 it does not carry a sequence number. Forward-compat would let
//! us add an optional field there, but since this PR cannot modify
//! `cloud_protocol`, we send the resume hint via
//! `TaskControl::Signal { name: "resume", payload: Some({"last_sequence":
//! N}) }` instead. The Worker side (PDX-19) is expected to recognise
//! either form. See [`session::resume_message`].
//!
//! ## Scope
//!
//! Per PDX-18, this PR delivers the standalone crate only. Wiring
//! [`CloudAgent`] into the orchestrator's persistent Router is a future
//! follow-up.

#![cfg(not(target_family = "wasm"))]
#![deny(missing_docs)]

pub mod agent;
pub mod error;
pub mod reconnect;
pub mod session;
pub mod transport;

#[cfg(test)]
mod tests;

pub use agent::{CloudAgent, CloudAgentConfig, CLOUD_CONTEXT_TOKENS};
pub use error::{CloudClientError, ConnectError};
pub use reconnect::{ReconnectDecision, ReconnectPolicy};
pub use session::{ResumeHint, SessionState};
pub use transport::{InMemoryTransport, Transport, TransportError, TungsteniteTransport};
