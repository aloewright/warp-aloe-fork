//! macOS-only implementation of the simulator hooks surface.
//!
//! Spawns `xcrun simctl` for each operation and parses the output. All
//! argv construction is delegated to [`crate::commands`] so the argument
//! contracts can be unit-tested on any host.

#![cfg(target_os = "macos")]

use std::path::Path;
use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_stream::Stream;

use crate::commands;
use crate::error::SimulatorError;
use crate::Result;

/// Opaque iOS/macOS simulator UDID.
///
/// Wrapped to prevent accidental confusion with bundle ids, app paths,
/// and other strings flowing through the surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SimulatorDeviceId(String);

impl SimulatorDeviceId {
    /// Construct from a raw UDID. No validation — `simctl` will reject
    /// obviously-bogus ids and we'll surface its complaint.
    pub fn new(udid: impl Into<String>) -> Self {
        Self(udid.into())
    }

    /// The underlying UDID string. Useful for logging.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SimulatorDeviceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Process id returned by `simctl launch`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId(pub u32);

/// One line of `log stream` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine(pub String);

/// Handle to a specific simulator device. Cheap to clone (`SimulatorDeviceId`
/// is a `String` wrapper).
#[derive(Debug, Clone)]
pub struct Simulator {
    device: SimulatorDeviceId,
}

impl Simulator {
    /// Construct from a known UDID without doing any IO.
    pub fn from_udid(device: SimulatorDeviceId) -> Self {
        Self { device }
    }

    /// The wrapped UDID.
    pub fn device(&self) -> &SimulatorDeviceId {
        &self.device
    }

    /// List all available simulator devices, regardless of state.
    pub async fn list() -> Result<Vec<SimulatorDeviceId>> {
        let argv = commands::list_devices_command_args();
        let out = run_xcrun(&argv).await?;
        parse_device_list(&out)
    }

    /// Find the first simulator whose name matches `name` exactly.
    pub async fn find(name: &str) -> Result<Option<Simulator>> {
        let argv = commands::list_devices_command_args();
        let out = run_xcrun(&argv).await?;
        Ok(parse_device_by_name(&out, name).map(Self::from_udid))
    }

    /// Boot the device. No-op if already booted (simctl returns non-zero
    /// in that case; we surface the error so the agent can decide what
    /// to do — typically just continue).
    pub async fn boot(&self) -> Result<()> {
        let argv = commands::boot_command_args(self.device.as_str());
        run_xcrun(&argv).await.map(|_| ())
    }

    /// Shut the device down.
    pub async fn shutdown(&self) -> Result<()> {
        let argv = commands::shutdown_command_args(self.device.as_str());
        run_xcrun(&argv).await.map(|_| ())
    }

    /// Install an `.app` bundle onto the device. The path must point at
    /// the bundle directory, not a zipped artifact.
    pub async fn install(&self, app_path: &Path) -> Result<()> {
        if !app_path.exists() {
            return Err(SimulatorError::MissingApp(app_path.to_path_buf()));
        }
        let argv = commands::install_command_args(
            self.device.as_str(),
            &app_path.to_string_lossy(),
        );
        run_xcrun(&argv).await.map(|_| ())
    }

    /// Launch an installed app by bundle id. Returns the child process id.
    pub async fn launch(&self, bundle_id: &str) -> Result<ProcessId> {
        let argv = commands::launch_command_args(self.device.as_str(), bundle_id);
        let out = run_xcrun(&argv).await?;
        parse_pid(&out)
    }

    /// Take a screenshot, returning raw PNG bytes.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        let argv = commands::screenshot_command_args(self.device.as_str());
        run_xcrun_bytes(&argv).await
    }

    /// Tap at the given (x, y) point coordinates.
    pub async fn tap(&self, x: f64, y: f64) -> Result<()> {
        let argv = commands::tap_command_args(self.device.as_str(), x, y);
        run_xcrun(&argv).await.map(|_| ())
    }

    /// Type the given string into the focused text field.
    pub async fn type_text(&self, text: &str) -> Result<()> {
        let argv = commands::type_text_command_args(self.device.as_str(), text);
        run_xcrun(&argv).await.map(|_| ())
    }

    /// Stream device logs at debug level. Each item in the stream is one
    /// line of `log stream` output. The stream terminates when the child
    /// `log` process exits (e.g. on simulator shutdown) or when the
    /// caller drops the stream (which kills the child).
    pub fn tail_logs(&self) -> impl Stream<Item = LogLine> {
        let argv = commands::tail_logs_command_args(self.device.as_str());
        async_stream::stream! {
            let mut child = match Command::new("xcrun")
                .args(&argv)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to spawn `xcrun log stream`");
                    return;
                }
            };
            let stdout = match child.stdout.take() {
                Some(s) => s,
                None => return,
            };
            let mut reader = BufReader::new(stdout).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => yield LogLine(line),
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            let _ = child.wait().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Subprocess helpers.
// ---------------------------------------------------------------------------

/// Run `xcrun <argv>` and return captured stdout as UTF-8.
async fn run_xcrun(argv: &[String]) -> Result<String> {
    let output = Command::new("xcrun")
        .args(argv)
        .output()
        .await
        .map_err(|source| SimulatorError::Spawn {
            argv: argv.to_vec(),
            source,
        })?;

    if !output.status.success() {
        return Err(SimulatorError::Simctl {
            argv: argv.to_vec(),
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run `xcrun <argv>` and return captured stdout as raw bytes (used for
/// screenshots).
async fn run_xcrun_bytes(argv: &[String]) -> Result<Vec<u8>> {
    let output = Command::new("xcrun")
        .args(argv)
        .output()
        .await
        .map_err(|source| SimulatorError::Spawn {
            argv: argv.to_vec(),
            source,
        })?;

    if !output.status.success() {
        return Err(SimulatorError::Simctl {
            argv: argv.to_vec(),
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(output.stdout)
}

// ---------------------------------------------------------------------------
// Output parsers.
// ---------------------------------------------------------------------------

/// `simctl list -j devices` JSON shape (subset).
#[derive(Debug, Deserialize)]
struct DeviceList {
    /// Map of runtime identifier -> device entries.
    #[serde(default)]
    devices: std::collections::HashMap<String, Vec<DeviceEntry>>,
}

#[derive(Debug, Deserialize)]
struct DeviceEntry {
    udid: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    #[allow(dead_code)]
    state: String,
    #[serde(default, rename = "isAvailable")]
    is_available: Option<bool>,
}

/// Extract every available UDID from a `simctl list -j devices` payload.
pub(crate) fn parse_device_list(json: &str) -> Result<Vec<SimulatorDeviceId>> {
    let parsed: DeviceList = serde_json::from_str(json)
        .map_err(|e| SimulatorError::ParseList(e.to_string()))?;
    let mut out = Vec::new();
    for entries in parsed.devices.values() {
        for entry in entries {
            if entry.is_available.unwrap_or(true) {
                out.push(SimulatorDeviceId::new(entry.udid.clone()));
            }
        }
    }
    Ok(out)
}

/// Find the first device with a matching `name` in a `simctl list -j
/// devices` payload.
pub(crate) fn parse_device_by_name(json: &str, name: &str) -> Option<SimulatorDeviceId> {
    let parsed: DeviceList = serde_json::from_str(json).ok()?;
    for entries in parsed.devices.values() {
        for entry in entries {
            if entry.name == name && entry.is_available.unwrap_or(true) {
                return Some(SimulatorDeviceId::new(entry.udid.clone()));
            }
        }
    }
    None
}

/// Parse `simctl launch` output. Apple's format is:
///
/// ```text
/// com.example.helloworld: 12345
/// ```
pub(crate) fn parse_pid(stdout: &str) -> Result<ProcessId> {
    let trimmed = stdout.trim();
    if let Some((_, pid)) = trimmed.rsplit_once(':') {
        if let Ok(n) = pid.trim().parse::<u32>() {
            return Ok(ProcessId(n));
        }
    }
    Err(SimulatorError::ParsePid(trimmed.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_LIST: &str = r#"{
        "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-17-2": [
                {
                    "udid": "AAAA-1111",
                    "name": "iPhone 15",
                    "state": "Shutdown",
                    "isAvailable": true
                },
                {
                    "udid": "BBBB-2222",
                    "name": "iPhone 15 Pro",
                    "state": "Shutdown",
                    "isAvailable": true
                },
                {
                    "udid": "CCCC-3333",
                    "name": "iPhone Old",
                    "state": "Shutdown",
                    "isAvailable": false
                }
            ]
        }
    }"#;

    #[test]
    fn parse_device_list_skips_unavailable() {
        let devices = parse_device_list(SAMPLE_LIST).unwrap();
        let udids: Vec<_> = devices.iter().map(|d| d.as_str().to_string()).collect();
        assert!(udids.contains(&"AAAA-1111".to_string()));
        assert!(udids.contains(&"BBBB-2222".to_string()));
        assert!(!udids.contains(&"CCCC-3333".to_string()));
    }

    #[test]
    fn parse_device_by_name_finds_exact_match() {
        let d = parse_device_by_name(SAMPLE_LIST, "iPhone 15 Pro").unwrap();
        assert_eq!(d.as_str(), "BBBB-2222");
    }

    #[test]
    fn parse_device_by_name_skips_unavailable() {
        // "iPhone Old" is unavailable; should not be returned.
        assert!(parse_device_by_name(SAMPLE_LIST, "iPhone Old").is_none());
    }

    #[test]
    fn parse_pid_handles_apple_format() {
        let pid = parse_pid("com.example.app: 12345\n").unwrap();
        assert_eq!(pid, ProcessId(12345));
    }

    #[test]
    fn parse_pid_rejects_garbage() {
        assert!(parse_pid("nope").is_err());
        assert!(parse_pid("").is_err());
    }
}
