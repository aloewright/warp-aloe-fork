//! iOS / macOS simulator hooks for Helm (PDX-113).
//!
//! Thin async Rust wrapper around `xcrun simctl` that lets coding agents
//! drive iOS/macOS simulators end-to-end during development: boot, install,
//! launch, screenshot, tap, type, and tail logs.
//!
//! ## Design
//!
//! All operations shell out to `xcrun simctl` (occasionally `xcrun simctl
//! ui` or `xcrun simctl io`) and parse stdout. We deliberately avoid
//! `objc2` / `core-foundation` bindings — `Command::new("xcrun")` is
//! enough for the surface area an agent needs, and keeps the dependency
//! tree minimal and the crate macOS-only-by-policy rather than
//! macOS-only-by-link.
//!
//! ## Platform gate
//!
//! The crate compiles on every host (so the workspace builds on Linux CI)
//! but exposes only an empty stub outside of macOS. All real surface area
//! lives behind `#[cfg(target_os = "macos")]`.
//!
//! ## Surface
//!
//! * [`SimulatorDeviceId`] — opaque UDID newtype.
//! * [`Simulator`] — handle; constructible via [`Simulator::find`] /
//!   [`Simulator::list`] / [`Simulator::from_udid`].
//! * `boot`, `shutdown`, `install`, `launch`, `screenshot`, `tap`,
//!   `type_text`, `tail_logs`.
//! * Pure-helper functions [`commands::*`] that build the argv vectors,
//!   isolated for unit testing without spawning real subprocesses.
//!
//! ## Errors
//!
//! All fallible operations return [`SimulatorError`] (a `thiserror`-derived
//! enum). The `xcrun` failure path captures stdout/stderr so the agent can
//! see exactly what `simctl` complained about and self-correct.

#![deny(missing_docs)]

pub mod commands;
mod error;

pub use error::SimulatorError;

#[cfg(target_os = "macos")]
mod platform;

#[cfg(target_os = "macos")]
pub use platform::{LogLine, ProcessId, Simulator, SimulatorDeviceId};

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, SimulatorError>;
