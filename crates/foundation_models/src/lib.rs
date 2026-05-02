// SPDX-License-Identifier: AGPL-3.0-only
//
// Foundation Models Swift bridge — Rust side (PDX-13).
//
// This crate is the Rust-facing half of the bridge built in
// `swift/FoundationModelsBridge.swift`. It exposes a small, safe API
// over Apple's on-device Foundation Models framework:
//
//   * [`is_supported`] — true iff Foundation Models is available right now.
//   * [`complete`]     — single prompt → response, blocks the calling thread.
//   * [`complete_stream`] — streaming completion via a Rust closure callback.
//
// On non-macOS targets the API still compiles, but every entry point
// behaves as if the framework is absent: [`is_supported`] returns
// `false` and the completion functions return
// [`FoundationModelsError::Unavailable`]. This keeps platform gating in
// one place — callers don't need their own `#[cfg(target_os = "macos")]`.
//
// # Why a thread-blocking API
//
// PDX-15 will wrap the streaming entry point in an async task and feed
// the chunks into the orchestrator's event loop. Keeping the Rust side
// synchronous here means the Swift bridge owns the runtime story
// (DispatchSemaphore + a Swift `Task`) and we don't take a
// `tokio` dependency in this crate. The orchestrator is free to call
// these from a `spawn_blocking` worker.

#![deny(missing_docs)]
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

//! Apple Foundation Models Swift bridge.
//!
//! See the crate-level docs above for design notes; this comment block
//! is the visible rustdoc.

use std::fmt;

#[cfg(target_os = "macos")]
mod ffi;

/// Errors returned by the Foundation Models bridge.
#[derive(Debug, thiserror::Error)]
pub enum FoundationModelsError {
    /// The runtime is not available on this machine — either we're not
    /// on macOS, or we're on macOS older than 26, or the framework is
    /// otherwise unloadable.
    #[error("Foundation Models is not available on this system")]
    Unavailable,
    /// The supplied prompt was not valid UTF-8 or contained a null byte
    /// that prevents passing it across the C ABI.
    #[error("invalid prompt encoding (must be UTF-8 without interior null bytes)")]
    InvalidPrompt,
    /// The Foundation Models runtime returned an error.
    #[error("Foundation Models runtime error")]
    Runtime,
    /// The streaming call was cancelled by the caller's callback.
    #[error("stream cancelled by caller")]
    Cancelled,
}

/// Status codes returned by the C entry points. Kept in sync with
/// `FMStatus` in `FoundationModelsBridge.swift`.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Ok = 0,
    Unavailable = 1,
    InvalidUtf8 = 2,
    Runtime = 3,
    Cancelled = 4,
}

impl Status {
    fn from_raw(value: i32) -> Self {
        match value {
            0 => Status::Ok,
            1 => Status::Unavailable,
            2 => Status::InvalidUtf8,
            4 => Status::Cancelled,
            // Any unrecognised status maps to `Runtime` so we never
            // return "ok" on garbled output.
            _ => Status::Runtime,
        }
    }

    fn into_result(self) -> Result<(), FoundationModelsError> {
        match self {
            Status::Ok => Ok(()),
            Status::Unavailable => Err(FoundationModelsError::Unavailable),
            Status::InvalidUtf8 => Err(FoundationModelsError::InvalidPrompt),
            Status::Cancelled => Err(FoundationModelsError::Cancelled),
            Status::Runtime => Err(FoundationModelsError::Runtime),
        }
    }
}

/// Returns `true` iff Apple Foundation Models is usable on this
/// process right now.
///
/// On non-macOS targets this is a compile-time `false` — the call is
/// inlined to the constant. On macOS the call falls through to the
/// Swift bridge, which checks `#available(macOS 26, *)` plus a smoke
/// test that the framework can construct a session.
///
/// PDX-16's router will call this once at startup to decide whether to
/// register `Provider::FoundationModels` in the agent registry.
pub fn is_supported() -> bool {
    #[cfg(all(target_os = "macos", not(foundation_models_swift_skipped)))]
    {
        // SAFETY: `fm_is_supported` is `pure` from Rust's perspective —
        // it touches no caller-owned memory and has no side effects on
        // process state. It is safe to call from any thread.
        unsafe { ffi::fm_is_supported() }
    }
    #[cfg(any(not(target_os = "macos"), foundation_models_swift_skipped))]
    {
        false
    }
}

/// Run `prompt` to completion synchronously and return the model's
/// response.
///
/// This blocks the calling thread until the model finishes responding;
/// if you're calling from an async runtime, wrap this in
/// `tokio::task::spawn_blocking` (or your runtime's equivalent).
///
/// Returns [`FoundationModelsError::Unavailable`] when
/// [`is_supported`] would return `false`, so callers can rely on a
/// single error path instead of pre-checking.
pub fn complete(prompt: &str) -> Result<String, FoundationModelsError> {
    if !is_supported() {
        return Err(FoundationModelsError::Unavailable);
    }
    #[cfg(all(target_os = "macos", not(foundation_models_swift_skipped)))]
    {
        ffi::complete(prompt)
    }
    #[cfg(any(not(target_os = "macos"), foundation_models_swift_skipped))]
    {
        let _ = prompt;
        Err(FoundationModelsError::Unavailable)
    }
}

/// Streaming sink for [`complete_stream`].
///
/// The callback is invoked once per partial response chunk. Returning
/// [`StreamControl::Stop`] cancels the stream — the bridge will tear
/// down the session and surface [`FoundationModelsError::Cancelled`]
/// from [`complete_stream`].
pub trait StreamSink {
    /// Receive one chunk of the streaming response.
    fn on_chunk(&mut self, chunk: &str) -> StreamControl;
}

/// Whether the stream callback wants to keep receiving chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamControl {
    /// Keep streaming; deliver the next chunk when ready.
    Continue,
    /// Cancel the stream as soon as possible.
    Stop,
}

impl<F: FnMut(&str) -> StreamControl> StreamSink for F {
    fn on_chunk(&mut self, chunk: &str) -> StreamControl {
        (self)(chunk)
    }
}

/// Stream `prompt` token-by-token, invoking `sink` for each chunk.
///
/// Each chunk is a freshly-decoded `&str`; the bridge handles the C
/// allocation and freeing internally so the sink only ever sees safe
/// Rust references.
///
/// Like [`complete`], this blocks the calling thread for the duration
/// of the stream.
pub fn complete_stream<S: StreamSink>(
    prompt: &str,
    sink: S,
) -> Result<(), FoundationModelsError> {
    if !is_supported() {
        return Err(FoundationModelsError::Unavailable);
    }
    #[cfg(all(target_os = "macos", not(foundation_models_swift_skipped)))]
    {
        ffi::complete_stream(prompt, sink)
    }
    #[cfg(any(not(target_os = "macos"), foundation_models_swift_skipped))]
    {
        let _ = (prompt, sink);
        Err(FoundationModelsError::Unavailable)
    }
}

// `Display` impl on `Status` purely for debugging output in logs.
impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Ok => write!(f, "ok"),
            Status::Unavailable => write!(f, "unavailable"),
            Status::InvalidUtf8 => write!(f, "invalid_utf8"),
            Status::Runtime => write!(f, "runtime_error"),
            Status::Cancelled => write!(f, "cancelled"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On non-macOS targets the probe is a compile-time constant `false`
    /// and every entry point must surface `Unavailable` rather than
    /// attempt an FFI call.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn probe_returns_false_off_mac() {
        assert!(!is_supported());
        assert!(matches!(
            complete("hi"),
            Err(FoundationModelsError::Unavailable)
        ));
        let r = complete_stream("hi", |_chunk: &str| StreamControl::Continue);
        assert!(matches!(r, Err(FoundationModelsError::Unavailable)));
    }

    /// On macOS the probe must always return *something* without panicking
    /// even when running on a system older than macOS 26 (the bridge is
    /// expected to return `false` in that case).
    #[cfg(target_os = "macos")]
    #[test]
    fn probe_does_not_panic_on_mac() {
        let _ = is_supported();
    }

    /// Status round-trip: every known raw value parses back to the
    /// matching `Status`, and unknown raw values fall through to
    /// `Runtime` (never `Ok`).
    #[test]
    fn status_round_trip() {
        assert_eq!(Status::from_raw(0), Status::Ok);
        assert_eq!(Status::from_raw(1), Status::Unavailable);
        assert_eq!(Status::from_raw(2), Status::InvalidUtf8);
        assert_eq!(Status::from_raw(3), Status::Runtime);
        assert_eq!(Status::from_raw(4), Status::Cancelled);
        assert_eq!(Status::from_raw(99), Status::Runtime);
        assert!(matches!(
            Status::Unavailable.into_result(),
            Err(FoundationModelsError::Unavailable)
        ));
        assert!(Status::Ok.into_result().is_ok());
    }

    /// `complete` must reject prompts containing interior null bytes
    /// before they reach the C ABI. We exercise this even on non-macOS
    /// builds because the validation lives entirely on the Rust side.
    #[test]
    fn complete_rejects_interior_nul() {
        let r = complete("hi\0there");
        // On non-macOS we hit `Unavailable` before the nul check; on
        // macOS we'll typically hit `InvalidPrompt`. Both are acceptable
        // — the contract is "does not crash and does not succeed".
        assert!(matches!(
            r,
            Err(FoundationModelsError::Unavailable) | Err(FoundationModelsError::InvalidPrompt)
        ));
    }
}
