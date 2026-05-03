//! Error type for the simulator_hooks crate.

use std::path::PathBuf;

use thiserror::Error;

/// All errors surfaced by the simulator_hooks crate.
///
/// The `Simctl` variant intentionally captures the entire `stdout` /
/// `stderr` so an agent reading the error has enough context to fix its
/// invocation (e.g. wrong UDID, missing `.app` bundle, simulator already
/// booted, etc.).
#[derive(Debug, Error)]
pub enum SimulatorError {
    /// Failed to spawn `xcrun` at all (binary missing, permissions, etc.).
    #[error("failed to spawn `xcrun {argv:?}`: {source}")]
    Spawn {
        /// Argv passed to xcrun, for debugging.
        argv: Vec<String>,
        /// Underlying IO error.
        #[source]
        source: std::io::Error,
    },

    /// `xcrun simctl` ran to completion but exited non-zero.
    #[error("`xcrun {argv:?}` exited with status {status:?}\nstdout: {stdout}\nstderr: {stderr}")]
    Simctl {
        /// Argv passed to xcrun.
        argv: Vec<String>,
        /// Exit status, if available.
        status: Option<i32>,
        /// Captured stdout.
        stdout: String,
        /// Captured stderr.
        stderr: String,
    },

    /// `simctl list -j devices` returned JSON we couldn't parse.
    #[error("failed to parse `simctl list` output: {0}")]
    ParseList(String),

    /// Caller asked for a device by name and we couldn't find it.
    #[error("no simulator device found matching {0:?}")]
    DeviceNotFound(String),

    /// Caller passed an `.app` path that doesn't exist on disk.
    #[error("app bundle not found: {0}")]
    MissingApp(PathBuf),

    /// Failed to parse a process id from `simctl launch` stdout.
    #[error("failed to parse pid from `simctl launch` output: {0:?}")]
    ParsePid(String),
}
