// Build script for the Foundation Models Swift bridge crate (PDX-13).
//
// On macOS, this script compiles `swift/FoundationModelsBridge.swift` into
// a static library `libFoundationModelsBridge.a` and emits the link flags
// the Rust crate needs to call into it via `extern "C"`.
//
// On every other target the script does nothing — `lib.rs` is gated on
// `#[cfg(target_os = "macos")]` and provides a stub implementation that
// always reports the runtime as unavailable.
//
// We use `xcrun swiftc` rather than a `Package.swift` because:
//   * The bridge is one file with no Swift package consumers.
//   * It keeps the build identical to how `crates/warpui/build.rs`
//     compiles the Objective-C runtime — `xcrun` + a static archive.
//   * It avoids pulling SwiftPM into Cargo's incremental build graph.

#![allow(clippy::disallowed_types)]

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    cfg_aliases::cfg_aliases! {
        macos: { target_os = "macos" },
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=swift/FoundationModelsBridge.swift");
    // Declare our internal cfg so check-cfg doesn't warn on stable.
    println!("cargo:rustc-check-cfg=cfg(foundation_models_swift_skipped)");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        // Non-macOS targets get a Rust-only stub; nothing to compile.
        return;
    }

    // Allow CI / contributors without Xcode (for example, Linux runners
    // mistakenly cross-targeting macOS) to opt out of the Swift compile
    // step. The Rust side will still surface `fm_is_supported() == false`
    // because the stub functions live in the Rust crate when the symbol
    // isn't linked. In practice this knob is for local builds where Swift
    // compilation is broken; real macOS builds always go through swiftc.
    if env::var("FOUNDATION_MODELS_SKIP_SWIFT").is_ok() {
        println!("cargo:warning=foundation_models: FOUNDATION_MODELS_SKIP_SWIFT set; skipping Swift compile (capability probe will return false)");
        println!("cargo:rustc-cfg=foundation_models_swift_skipped");
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let swift_src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("swift")
        .join("FoundationModelsBridge.swift");
    let archive_path = out_dir.join("libFoundationModelsBridge.a");

    // We deliberately target macOS 13.0 as a deployment floor so the
    // Swift code compiles on older toolchains; the actual Foundation
    // Models calls inside the bridge are gated with
    // `if #available(macOS 26, *)` so older systems return "unsupported"
    // without the dynamic linker faulting on missing symbols.
    let target = env::var("TARGET").unwrap_or_default();
    let swift_target = if target.starts_with("aarch64") {
        "arm64-apple-macosx13.0"
    } else {
        "x86_64-apple-macosx13.0"
    };

    let status = Command::new("xcrun")
        .args([
            "-sdk",
            "macosx",
            "swiftc",
            "-emit-library",
            "-static",
            "-parse-as-library",
            "-O",
            "-target",
            swift_target,
            "-module-name",
            "FoundationModelsBridge",
            "-o",
        ])
        .arg(&archive_path)
        .arg(&swift_src)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!(
                "cargo:rustc-link-search=native={}",
                archive_path.parent().unwrap().display()
            );
            println!("cargo:rustc-link-lib=static=FoundationModelsBridge");
            // Swift static libraries depend on the Swift runtime. Link
            // both the search path and an rpath entry so the dynamic
            // loader can find `libswift_Concurrency.dylib` etc. at run
            // time. (`/usr/lib/swift` ships with macOS itself.)
            println!("cargo:rustc-link-search=native=/usr/lib/swift");
            println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
            println!("cargo:rustc-link-lib=dylib=swiftCore");
            // Foundation is required for `String`/`NSString` interop.
            println!("cargo:rustc-link-lib=framework=Foundation");
        }
        Ok(s) => {
            println!(
                "cargo:warning=foundation_models: swiftc failed (status={s}); falling back to Rust stub"
            );
            println!("cargo:rustc-cfg=foundation_models_swift_skipped");
        }
        Err(e) => {
            println!(
                "cargo:warning=foundation_models: failed to invoke xcrun swiftc ({e}); falling back to Rust stub"
            );
            println!("cargo:rustc-cfg=foundation_models_swift_skipped");
        }
    }
}
