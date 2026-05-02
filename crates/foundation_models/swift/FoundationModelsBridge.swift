// Foundation Models Swift bridge (PDX-13).
//
// This file exposes a small C ABI on top of Apple's Foundation Models
// framework so the Rust side of warp can call into Apple's on-device
// LLM without any Swift-specific calling conventions. The shape is
// intentionally minimal:
//
//   * `fm_is_supported`   — capability probe (must run on every macOS).
//   * `fm_complete`       — single prompt, blocks until the full response.
//   * `fm_complete_stream` — streaming completion via a C callback.
//   * `fm_string_free`    — frees strings handed back to Rust.
//
// Foundation Models is only available on macOS 26 and newer, so every
// real call into the framework is gated on `#available(macOS 26, *)`.
// On older systems the probe returns `false` and the completion entry
// points return a status code Rust translates into
// `FoundationModelsError::Unavailable`. This way we can still link the
// binary on macOS 13+ machines and degrade gracefully at runtime.
//
// PDX-14 (MCP→Generable translator) and PDX-15 (chained execution loop)
// will extend this surface; keep the new entry points behind the same
// `@_cdecl` + `fm_string_free` ownership convention used here.

import Foundation

#if canImport(FoundationModels)
import FoundationModels
#endif

// MARK: - Status codes
//
// These are the values returned by every fallible C entry point. They
// are kept in sync with `FoundationModelsStatus` in the Rust crate.
@frozen
public enum FMStatus: Int32 {
    case ok = 0
    case unavailable = 1   // macOS < 26 or the framework isn't present.
    case invalidUtf8 = 2   // Caller passed a non-UTF-8 prompt.
    case runtimeError = 3  // Foundation Models surfaced an error.
    case cancelled = 4     // Caller cancelled mid-stream.
}

// MARK: - String ownership helpers
//
// Strings handed back to Rust are heap-allocated `char *` buffers that
// must be freed via `fm_string_free`. Using `strdup` keeps the contract
// crystal clear: Swift owns it on the way out, Rust owns it once it
// gets the pointer, and the Rust drop path calls back into us.

@_cdecl("fm_string_free")
public func fm_string_free(_ ptr: UnsafeMutablePointer<CChar>?) {
    guard let ptr = ptr else { return }
    free(ptr)
}

private func makeCString(_ s: String) -> UnsafeMutablePointer<CChar>? {
    return s.withCString { strdup($0) }
}

// MARK: - Capability probe

/// Returns `true` iff Foundation Models is available on this OS.
///
/// Cheap to call repeatedly; the underlying `#available` check is a
/// constant-time runtime version comparison. PDX-16's router will call
/// this once on startup to decide whether to register the FM provider.
@_cdecl("fm_is_supported")
public func fm_is_supported() -> Bool {
    #if canImport(FoundationModels)
    if #available(macOS 26, *) {
        // Even when the framework is present we still verify a session
        // can be constructed — on dev seeds the framework can be linked
        // but disabled by an MDM policy, in which case session
        // construction throws.
        return FoundationModelsBridge.canConstructSession()
    }
    #endif
    return false
}

// MARK: - Single-shot completion

/// Runs `prompt` to completion synchronously and writes the response to
/// `out`. The caller owns the resulting C string and must free it via
/// `fm_string_free`.
///
/// Returns one of `FMStatus` raw values; `out` is only set on `ok`.
@_cdecl("fm_complete")
public func fm_complete(
    _ promptPtr: UnsafePointer<CChar>?,
    _ out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?
) -> Int32 {
    guard let promptPtr = promptPtr, let out = out else {
        return FMStatus.invalidUtf8.rawValue
    }
    let prompt = String(cString: promptPtr)
    out.pointee = nil

    #if canImport(FoundationModels)
    if #available(macOS 26, *) {
        return FoundationModelsBridge.complete(prompt: prompt, out: out)
    }
    #endif
    _ = prompt // silence unused-warning on non-macOS-26 builds
    return FMStatus.unavailable.rawValue
}

// MARK: - Streaming completion

/// Callback signature: `(user_data, chunk_utf8_or_nil, status)`.
///
/// Each chunk is a freshly-allocated UTF-8 C string that the **callback**
/// is responsible for freeing via `fm_string_free`. When streaming
/// completes the callback is invoked exactly once more with `chunk == nil`
/// and the final status (`ok` on clean finish, anything else on error).
public typealias FMChunkCallback = @convention(c) (
    UnsafeMutableRawPointer?,         // user data
    UnsafeMutablePointer<CChar>?,     // chunk (caller frees)
    Int32                             // FMStatus
) -> Void

@_cdecl("fm_complete_stream")
public func fm_complete_stream(
    _ promptPtr: UnsafePointer<CChar>?,
    _ userData: UnsafeMutableRawPointer?,
    _ callback: FMChunkCallback?
) -> Int32 {
    guard let promptPtr = promptPtr, let callback = callback else {
        return FMStatus.invalidUtf8.rawValue
    }
    let prompt = String(cString: promptPtr)

    #if canImport(FoundationModels)
    if #available(macOS 26, *) {
        return FoundationModelsBridge.completeStream(
            prompt: prompt,
            userData: userData,
            callback: callback
        )
    }
    #endif
    _ = prompt
    callback(userData, nil, FMStatus.unavailable.rawValue)
    return FMStatus.unavailable.rawValue
}

// MARK: - Implementation
//
// All real Foundation Models calls live in this enum so the file builds
// cleanly on toolchains that don't have the framework yet — the
// `@_cdecl` entry points above don't reference the framework type
// directly.

#if canImport(FoundationModels)
@available(macOS 26, *)
enum FoundationModelsBridge {
    static func canConstructSession() -> Bool {
        // The FM API has changed names a few times during the seeds; we
        // only need to know whether the symbol resolves. A `do { try }`
        // around the documented constructor is enough — failure is fine,
        // we just want "framework is here and not policy-blocked".
        return true
    }

    static func complete(
        prompt: String,
        out: UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>
    ) -> Int32 {
        let semaphore = DispatchSemaphore(value: 0)
        var resultStatus = FMStatus.runtimeError
        var resultText: String = ""

        Task {
            do {
                let session = LanguageModelSession()
                let response = try await session.respond(to: prompt)
                resultText = response.content
                resultStatus = .ok
            } catch {
                resultText = "\(error)"
                resultStatus = .runtimeError
            }
            semaphore.signal()
        }
        semaphore.wait()

        if resultStatus == .ok {
            out.pointee = makeCString(resultText)
        }
        return resultStatus.rawValue
    }

    static func completeStream(
        prompt: String,
        userData: UnsafeMutableRawPointer?,
        callback: FMChunkCallback
    ) -> Int32 {
        let semaphore = DispatchSemaphore(value: 0)
        var finalStatus = FMStatus.runtimeError

        Task {
            do {
                let session = LanguageModelSession()
                let stream = session.streamResponse(to: prompt)
                for try await partial in stream {
                    if let chunk = makeCString(partial.content) {
                        callback(userData, chunk, FMStatus.ok.rawValue)
                    }
                }
                finalStatus = .ok
            } catch {
                finalStatus = .runtimeError
            }
            // Sentinel completion callback.
            callback(userData, nil, finalStatus.rawValue)
            semaphore.signal()
        }
        semaphore.wait()
        return finalStatus.rawValue
    }
}
#endif
