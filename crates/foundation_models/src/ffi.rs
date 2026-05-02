// Internal FFI shims for the Foundation Models Swift bridge.
//
// Everything in this module is `unsafe` by nature; the public crate
// API lives in `lib.rs` and wraps these calls with the Rust-side
// invariants (UTF-8 validation, ownership of returned C strings,
// cancellation handling for streams).

use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::ptr;

use crate::{FoundationModelsError, Status, StreamControl, StreamSink};

extern "C" {
    /// `bool fm_is_supported(void)`
    pub(crate) fn fm_is_supported() -> bool;

    /// `int32_t fm_complete(const char *prompt, char **out)`
    fn fm_complete(prompt: *const c_char, out: *mut *mut c_char) -> c_int;

    /// `void fm_string_free(char *)`
    fn fm_string_free(ptr: *mut c_char);

    /// `int32_t fm_complete_stream(const char *prompt, void *user_data,
    ///     void (*cb)(void*, char*, int32_t))`
    fn fm_complete_stream(
        prompt: *const c_char,
        user_data: *mut std::ffi::c_void,
        callback: ChunkCallbackC,
    ) -> c_int;
}

/// C-ABI signature of the streaming callback shipped to Swift. Matches
/// `FMChunkCallback` in `FoundationModelsBridge.swift`.
type ChunkCallbackC =
    extern "C" fn(*mut std::ffi::c_void, *mut c_char, c_int);

/// Build a `CString` from a `&str`, surfacing the interior-NUL case as a
/// typed error rather than letting it propagate as a runtime panic.
fn cstring_for(prompt: &str) -> Result<CString, FoundationModelsError> {
    CString::new(prompt).map_err(|_| FoundationModelsError::InvalidPrompt)
}

/// Take ownership of a `*mut c_char` returned by Swift and turn it into
/// an owned `String`. The C buffer is freed before this function
/// returns.
unsafe fn take_swift_string(ptr: *mut c_char) -> Result<String, FoundationModelsError> {
    if ptr.is_null() {
        return Err(FoundationModelsError::Runtime);
    }
    let owned = CStr::from_ptr(ptr).to_string_lossy().into_owned();
    fm_string_free(ptr);
    Ok(owned)
}

pub(crate) fn complete(prompt: &str) -> Result<String, FoundationModelsError> {
    let cprompt = cstring_for(prompt)?;
    let mut out: *mut c_char = ptr::null_mut();

    // SAFETY: `cprompt` outlives the call; `out` is a local that
    // receives an optional Swift-allocated buffer. We hand it back to
    // Swift via `fm_string_free` immediately after copying it.
    let raw = unsafe { fm_complete(cprompt.as_ptr(), &mut out as *mut *mut c_char) };
    Status::from_raw(raw).into_result()?;
    unsafe { take_swift_string(out) }
}

/// Boxed trait-object handle for the streaming callback. We pass the
/// pointer to this box across the FFI boundary as `user_data`; the C
/// shim hands it back unchanged with each chunk.
struct StreamState<'a> {
    sink: &'a mut dyn StreamSink,
    cancelled: bool,
}

extern "C" fn stream_trampoline(
    user_data: *mut std::ffi::c_void,
    chunk: *mut c_char,
    raw_status: c_int,
) {
    // SAFETY: `user_data` is the boxed `StreamState` we handed to
    // Swift; Swift never aliases it across threads concurrently with
    // this callback (it's invoked from a serial DispatchQueue inside
    // the Swift `Task`). Likewise the `chunk` pointer, when non-null,
    // is owned by us and must be freed via `fm_string_free`.
    let state = unsafe {
        debug_assert!(!user_data.is_null());
        &mut *(user_data as *mut StreamState<'_>)
    };

    if state.cancelled {
        if !chunk.is_null() {
            unsafe { fm_string_free(chunk) };
        }
        return;
    }

    if chunk.is_null() {
        // Sentinel "stream finished" callback — nothing to deliver. The
        // outer call site will translate `raw_status` into the right
        // Result on its own.
        let _ = raw_status;
        return;
    }

    // Convert the Swift-allocated buffer into a Rust `&str`, deliver it
    // to the sink, and free the buffer before returning.
    let owned = unsafe { CStr::from_ptr(chunk).to_string_lossy().into_owned() };
    unsafe { fm_string_free(chunk) };

    if state.sink.on_chunk(&owned) == StreamControl::Stop {
        state.cancelled = true;
    }
}

pub(crate) fn complete_stream<S: StreamSink>(
    prompt: &str,
    mut sink: S,
) -> Result<(), FoundationModelsError> {
    let cprompt = cstring_for(prompt)?;
    let mut state = StreamState {
        sink: &mut sink,
        cancelled: false,
    };

    // SAFETY: `state` and `cprompt` live until after the FFI call
    // returns (the Swift bridge runs the entire stream synchronously
    // before returning), so the raw pointers we hand to Swift are
    // valid for the whole call.
    let raw = unsafe {
        fm_complete_stream(
            cprompt.as_ptr(),
            &mut state as *mut StreamState<'_> as *mut std::ffi::c_void,
            stream_trampoline,
        )
    };

    if state.cancelled {
        return Err(FoundationModelsError::Cancelled);
    }
    Status::from_raw(raw).into_result()
}
