//! End-to-end integration test (PDX-113 acceptance).
//!
//! Boots a real simulator, takes a screenshot, asserts the result is a
//! non-empty PNG, and shuts the simulator back down.
//!
//! Gated `#[ignore]` because:
//!
//! 1. CI runs on Linux where this crate compiles to a stub.
//! 2. Even on macOS CI, no simulator runtime is provisioned.
//! 3. The test takes 30-60s of real wall time (boot + screenshot).
//!
//! To run locally on a developer macOS box:
//!
//! ```bash
//! cargo test -p simulator_hooks --tests -- --ignored end_to_end
//! ```
//!
//! Optionally pin to a specific device by setting
//! `SIMULATOR_HOOKS_TEST_DEVICE` to a device name (e.g. "iPhone 15"); we
//! default to the first available device returned by `simctl list`.

#![cfg(target_os = "macos")]

use simulator_hooks::Simulator;

#[tokio::test]
#[ignore = "requires macOS host with Xcode + simulator runtime; takes ~60s"]
async fn end_to_end_boot_screenshot_shutdown() {
    // Pick a device — env override or first available.
    let sim = if let Ok(name) = std::env::var("SIMULATOR_HOOKS_TEST_DEVICE") {
        Simulator::find(&name)
            .await
            .expect("list simulators")
            .unwrap_or_else(|| panic!("no simulator named {name:?}"))
    } else {
        let all = Simulator::list().await.expect("list simulators");
        let first = all.into_iter().next().expect("at least one simulator");
        Simulator::from_udid(first)
    };

    // Boot. simctl returns non-zero if already booted; that's fine for our
    // purposes, so we ignore an error here. The screenshot call below will
    // fail loudly if the device truly isn't running.
    let _ = sim.boot().await;

    // Screenshot — this is the load-bearing assertion.
    let png = sim.screenshot().await.expect("screenshot");
    assert!(!png.is_empty(), "screenshot returned empty bytes");
    // PNG signature: 89 50 4E 47 0D 0A 1A 0A.
    assert_eq!(
        &png[..8],
        &[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
        "screenshot output is not a PNG"
    );

    // Shutdown. Best-effort — if it was already shut down, that's fine.
    let _ = sim.shutdown().await;
}
