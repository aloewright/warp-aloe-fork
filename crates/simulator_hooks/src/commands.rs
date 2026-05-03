//! Pure helpers that build the `xcrun simctl` argv vectors.
//!
//! Split out of the platform module so we can unit-test the argument
//! construction on any host (including Linux CI) without spawning a real
//! subprocess. The actual `Command` execution lives in
//! [`crate::platform`].

/// Build the argv for `xcrun simctl boot <udid>`.
pub fn boot_command_args(udid: &str) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "boot".to_string(),
        udid.to_string(),
    ]
}

/// Build the argv for `xcrun simctl shutdown <udid>`.
pub fn shutdown_command_args(udid: &str) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "shutdown".to_string(),
        udid.to_string(),
    ]
}

/// Build the argv for `xcrun simctl install <udid> <app_path>`.
pub fn install_command_args(udid: &str, app_path: &str) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "install".to_string(),
        udid.to_string(),
        app_path.to_string(),
    ]
}

/// Build the argv for `xcrun simctl launch <udid> <bundle_id>`.
///
/// We don't use `--console-pty` here because we want a process id back on
/// stdout in the form `<bundle>: <pid>`. `--console-pty` would attach the
/// child's console to ours and never return. Agents that need log output
/// should use [`tail_logs_command_args`] instead.
pub fn launch_command_args(udid: &str, bundle_id: &str) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "launch".to_string(),
        udid.to_string(),
        bundle_id.to_string(),
    ]
}

/// Build the argv for `xcrun simctl io <udid> screenshot --type png -`.
///
/// The trailing `-` writes the PNG bytes to stdout; we capture them as a
/// `Vec<u8>` upstream.
pub fn screenshot_command_args(udid: &str) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "io".to_string(),
        udid.to_string(),
        "screenshot".to_string(),
        "--type".to_string(),
        "png".to_string(),
        "-".to_string(),
    ]
}

/// Build the argv for `xcrun simctl ui <udid> tap <x> <y>`.
///
/// Coordinates are in points (the same coordinate space `simctl ui` uses
/// natively); callers that need to convert from pixels should divide by
/// the device's display scale.
pub fn tap_command_args(udid: &str, x: f64, y: f64) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "ui".to_string(),
        udid.to_string(),
        "tap".to_string(),
        format!("{x}"),
        format!("{y}"),
    ]
}

/// Build the argv for `xcrun simctl ui <udid> type <text>`.
///
/// A single argv slot for the text — `simctl ui type` accepts the full
/// string verbatim (no shell interpretation).
pub fn type_text_command_args(udid: &str, text: &str) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "ui".to_string(),
        udid.to_string(),
        "type".to_string(),
        text.to_string(),
    ]
}

/// Build the argv for streaming logs:
/// `xcrun simctl spawn <udid> log stream --level=debug`.
pub fn tail_logs_command_args(udid: &str) -> Vec<String> {
    vec![
        "simctl".to_string(),
        "spawn".to_string(),
        udid.to_string(),
        "log".to_string(),
        "stream".to_string(),
        "--level=debug".to_string(),
    ]
}

/// Build the argv for `xcrun simctl list -j devices`.
pub fn list_devices_command_args() -> Vec<String> {
    vec![
        "simctl".to_string(),
        "list".to_string(),
        "-j".to_string(),
        "devices".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_args_match_simctl_signature() {
        assert_eq!(
            boot_command_args("ABC-123"),
            vec!["simctl", "boot", "ABC-123"]
        );
    }

    #[test]
    fn shutdown_args_match_simctl_signature() {
        assert_eq!(
            shutdown_command_args("XYZ-9"),
            vec!["simctl", "shutdown", "XYZ-9"]
        );
    }

    #[test]
    fn install_args_match_simctl_signature() {
        assert_eq!(
            install_command_args("UDID", "/tmp/Hello.app"),
            vec!["simctl", "install", "UDID", "/tmp/Hello.app"]
        );
    }

    #[test]
    fn launch_args_omit_console_pty_so_pid_is_returned() {
        let args = launch_command_args("UDID", "com.warp.helloworld");
        assert_eq!(
            args,
            vec!["simctl", "launch", "UDID", "com.warp.helloworld"]
        );
        // We deliberately do *not* pass --console-pty (would block forever).
        assert!(!args.iter().any(|a| a == "--console-pty"));
    }

    #[test]
    fn screenshot_args_pipe_png_to_stdout() {
        let args = screenshot_command_args("UDID");
        assert_eq!(
            args,
            vec![
                "simctl",
                "io",
                "UDID",
                "screenshot",
                "--type",
                "png",
                "-"
            ]
        );
    }

    #[test]
    fn tap_args_use_decimal_coordinates() {
        // f64 round-trip: integers should render without scientific notation.
        let args = tap_command_args("UDID", 100.0, 200.0);
        assert_eq!(args, vec!["simctl", "ui", "UDID", "tap", "100", "200"]);

        // Sub-pixel coordinates are preserved.
        let args = tap_command_args("UDID", 12.5, 33.25);
        assert_eq!(args, vec!["simctl", "ui", "UDID", "tap", "12.5", "33.25"]);
    }

    #[test]
    fn type_text_args_pass_string_as_single_argv_slot() {
        let args = type_text_command_args("UDID", "hello world!");
        assert_eq!(args, vec!["simctl", "ui", "UDID", "type", "hello world!"]);
        // The text occupies exactly one argv slot — no shell splitting.
        assert_eq!(args.len(), 5);
    }

    #[test]
    fn tail_logs_args_use_log_stream_with_debug_level() {
        let args = tail_logs_command_args("UDID");
        assert_eq!(
            args,
            vec![
                "simctl",
                "spawn",
                "UDID",
                "log",
                "stream",
                "--level=debug"
            ]
        );
    }

    #[test]
    fn list_devices_args_request_json_output() {
        let args = list_devices_command_args();
        assert_eq!(args, vec!["simctl", "list", "-j", "devices"]);
        // -j is critical: without it we'd get human-readable text.
        assert!(args.iter().any(|a| a == "-j"));
    }
}
