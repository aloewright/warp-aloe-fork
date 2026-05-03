//! `simulator` daemon-mediated tool (PDX-113).
//!
//! Exposes the `simulator_hooks` surface to the agent without spawning
//! `xcrun simctl` from inside the agent's subprocess sandbox. Mirrors the
//! [`crate::linear_graphql`] pattern from PDX-112 §10.5: the agent emits a
//! `tool_use` event, Symphony intercepts it from the daemon, executes the
//! corresponding `simctl` command, and feeds the result back as a
//! `tool_result` event.
//!
//! ## Tool surface
//!
//! Single argument: a JSON object `{ op, ... }` where `op` selects the
//! operation. Supported ops:
//!
//! | `op`         | Required fields            | Returns                  |
//! |--------------|----------------------------|--------------------------|
//! | `list`       | —                          | `{ devices: [udid, …] }` |
//! | `find`       | `name: string`             | `{ udid: string \| null }`|
//! | `boot`       | `udid: string`             | `{ ok: true }`           |
//! | `shutdown`   | `udid: string`             | `{ ok: true }`           |
//! | `install`    | `udid: string, app: path`  | `{ ok: true }`           |
//! | `launch`     | `udid: string, bundle: id` | `{ pid: u32 }`           |
//! | `screenshot` | `udid: string`             | `{ png_base64: string }` |
//! | `tap`        | `udid, x: f64, y: f64`     | `{ ok: true }`           |
//! | `type_text`  | `udid: string, text: str`  | `{ ok: true }`           |
//!
//! Errors are returned as `{ error: { kind, message } }`. The daemon does
//! NOT inject any iOS / macOS credentials into the subprocess environment;
//! the agent only ever sees JSON results.
//!
//! ## Platform
//!
//! On macOS hosts a real [`SimulatorExecutor`] is available; on other
//! hosts the tool is still callable but every op returns
//! `{ error: { kind: "unsupported_platform", … } }` so agents running on
//! Linux dev hosts get a structured failure instead of a panic.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

/// Tool name advertised on the agent boundary. Constant so that both the
/// agent registration side and the interception side agree on the spelling.
pub const TOOL_NAME: &str = "simulator";

/// Daemon-side handle to the simulator executor. Cheaply cloneable.
#[derive(Clone)]
pub struct SimulatorTool {
    executor: Arc<dyn SimulatorExecutor>,
}

/// Tracker-agnostic executor trait so tests can supply a mock without
/// shelling out to `xcrun`.
#[async_trait]
pub trait SimulatorExecutor: Send + Sync {
    /// Execute a single tool call and return the structured response. The
    /// implementation must NEVER panic — error paths must surface as
    /// `Err(SimulatorToolError)` so [`SimulatorTool::execute`] can shape
    /// them into the canonical `{ error: { kind, message } }` envelope.
    async fn dispatch(&self, op: SimulatorOp) -> Result<Value, SimulatorToolError>;
}

/// Parsed tool argument, ready for the executor to act on. Lifted out so
/// argument validation can happen up front (and so mock executors can
/// match exhaustively on the variant).
#[derive(Debug, Clone, PartialEq)]
pub enum SimulatorOp {
    /// `simctl list -j devices` → list of UDIDs.
    List,
    /// `simctl list` filtered by exact device name.
    Find {
        /// Device name (e.g. "iPhone 15").
        name: String,
    },
    /// `simctl boot <udid>`.
    Boot {
        /// Target UDID.
        udid: String,
    },
    /// `simctl shutdown <udid>`.
    Shutdown {
        /// Target UDID.
        udid: String,
    },
    /// `simctl install <udid> <app_path>`.
    Install {
        /// Target UDID.
        udid: String,
        /// Path to a `.app` bundle.
        app: String,
    },
    /// `simctl launch <udid> <bundle_id>` → `pid`.
    Launch {
        /// Target UDID.
        udid: String,
        /// Bundle id (e.g. `com.example.helloworld`).
        bundle: String,
    },
    /// `simctl io <udid> screenshot --type png -` → base64-encoded PNG.
    Screenshot {
        /// Target UDID.
        udid: String,
    },
    /// `simctl ui <udid> tap <x> <y>`.
    Tap {
        /// Target UDID.
        udid: String,
        /// X coordinate (points).
        x: f64,
        /// Y coordinate (points).
        y: f64,
    },
    /// `simctl ui <udid> type <text>`.
    TypeText {
        /// Target UDID.
        udid: String,
        /// Text to type into the focused field.
        text: String,
    },
}

/// Error returned by an executor. Wrapped in a structured envelope by
/// [`SimulatorTool::execute`] before being returned to the agent.
#[derive(Debug, thiserror::Error)]
pub enum SimulatorToolError {
    /// The host can't run simulators (non-macOS).
    #[error("unsupported platform: simulator hooks require macOS")]
    UnsupportedPlatform,
    /// `simctl` itself failed.
    #[error("simctl failed: {0}")]
    Simctl(String),
    /// Argument was missing or malformed.
    #[error("invalid argument: {0}")]
    Argument(String),
}

impl SimulatorToolError {
    fn kind(&self) -> &'static str {
        match self {
            Self::UnsupportedPlatform => "unsupported_platform",
            Self::Simctl(_) => "simctl",
            Self::Argument(_) => "argument_validation",
        }
    }
}

impl SimulatorTool {
    /// Construct from any executor.
    pub fn new(executor: Arc<dyn SimulatorExecutor>) -> Self {
        Self { executor }
    }

    /// Execute one tool call. Always returns a JSON value — argument
    /// validation, executor errors, and rate-limit failures are surfaced
    /// as `{ error: { kind, message } }` rather than `Err(_)` so the
    /// agent can read them and self-correct.
    pub async fn execute(&self, args: &Value) -> Value {
        let op = match parse_op(args) {
            Ok(op) => op,
            Err(e) => return error_envelope(&e),
        };
        match self.executor.dispatch(op).await {
            Ok(v) => v,
            Err(e) => error_envelope(&e),
        }
    }
}

/// Parse the JSON args object into a [`SimulatorOp`].
fn parse_op(args: &Value) -> Result<SimulatorOp, SimulatorToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| SimulatorToolError::Argument("args must be an object".into()))?;
    let op = obj
        .get("op")
        .and_then(|v| v.as_str())
        .ok_or_else(|| SimulatorToolError::Argument("missing `op` field".into()))?;

    let str_field = |k: &str| -> Result<String, SimulatorToolError> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| SimulatorToolError::Argument(format!("missing string field `{k}`")))
    };
    let f64_field = |k: &str| -> Result<f64, SimulatorToolError> {
        obj.get(k)
            .and_then(|v| v.as_f64())
            .ok_or_else(|| SimulatorToolError::Argument(format!("missing number field `{k}`")))
    };

    match op {
        "list" => Ok(SimulatorOp::List),
        "find" => Ok(SimulatorOp::Find { name: str_field("name")? }),
        "boot" => Ok(SimulatorOp::Boot { udid: str_field("udid")? }),
        "shutdown" => Ok(SimulatorOp::Shutdown { udid: str_field("udid")? }),
        "install" => Ok(SimulatorOp::Install {
            udid: str_field("udid")?,
            app: str_field("app")?,
        }),
        "launch" => Ok(SimulatorOp::Launch {
            udid: str_field("udid")?,
            bundle: str_field("bundle")?,
        }),
        "screenshot" => Ok(SimulatorOp::Screenshot { udid: str_field("udid")? }),
        "tap" => Ok(SimulatorOp::Tap {
            udid: str_field("udid")?,
            x: f64_field("x")?,
            y: f64_field("y")?,
        }),
        "type_text" => Ok(SimulatorOp::TypeText {
            udid: str_field("udid")?,
            text: str_field("text")?,
        }),
        other => Err(SimulatorToolError::Argument(format!(
            "unknown op `{other}`"
        ))),
    }
}

/// Build the canonical `{ error: { kind, message } }` envelope.
fn error_envelope(e: &SimulatorToolError) -> Value {
    json!({
        "error": {
            "kind": e.kind(),
            "message": e.to_string(),
        }
    })
}

// ---------------------------------------------------------------------------
// Default macOS-backed executor.
// ---------------------------------------------------------------------------

/// Production executor that shells out via `simulator_hooks` on macOS. On
/// other hosts every dispatch returns
/// [`SimulatorToolError::UnsupportedPlatform`].
pub struct XcrunSimulatorExecutor;

impl XcrunSimulatorExecutor {
    /// Construct the production executor.
    pub fn new() -> Self {
        Self
    }
}

impl Default for XcrunSimulatorExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SimulatorExecutor for XcrunSimulatorExecutor {
    async fn dispatch(&self, op: SimulatorOp) -> Result<Value, SimulatorToolError> {
        #[cfg(target_os = "macos")]
        {
            use simulator_hooks::{Simulator, SimulatorDeviceId};

            let map_err = |e: simulator_hooks::SimulatorError| {
                SimulatorToolError::Simctl(e.to_string())
            };

            match op {
                SimulatorOp::List => {
                    let devices = Simulator::list().await.map_err(map_err)?;
                    Ok(json!({
                        "devices": devices.iter().map(|d| d.as_str()).collect::<Vec<_>>(),
                    }))
                }
                SimulatorOp::Find { name } => {
                    let found = Simulator::find(&name).await.map_err(map_err)?;
                    Ok(json!({
                        "udid": found.map(|s| s.device().as_str().to_string()),
                    }))
                }
                SimulatorOp::Boot { udid } => {
                    Simulator::from_udid(SimulatorDeviceId::new(udid))
                        .boot()
                        .await
                        .map_err(map_err)?;
                    Ok(json!({ "ok": true }))
                }
                SimulatorOp::Shutdown { udid } => {
                    Simulator::from_udid(SimulatorDeviceId::new(udid))
                        .shutdown()
                        .await
                        .map_err(map_err)?;
                    Ok(json!({ "ok": true }))
                }
                SimulatorOp::Install { udid, app } => {
                    Simulator::from_udid(SimulatorDeviceId::new(udid))
                        .install(std::path::Path::new(&app))
                        .await
                        .map_err(map_err)?;
                    Ok(json!({ "ok": true }))
                }
                SimulatorOp::Launch { udid, bundle } => {
                    let pid = Simulator::from_udid(SimulatorDeviceId::new(udid))
                        .launch(&bundle)
                        .await
                        .map_err(map_err)?;
                    Ok(json!({ "pid": pid.0 }))
                }
                SimulatorOp::Screenshot { udid } => {
                    let png = Simulator::from_udid(SimulatorDeviceId::new(udid))
                        .screenshot()
                        .await
                        .map_err(map_err)?;
                    // Inline base64 to avoid pulling in another workspace dep
                    // for one call site.
                    Ok(json!({ "png_base64": base64_encode(&png) }))
                }
                SimulatorOp::Tap { udid, x, y } => {
                    Simulator::from_udid(SimulatorDeviceId::new(udid))
                        .tap(x, y)
                        .await
                        .map_err(map_err)?;
                    Ok(json!({ "ok": true }))
                }
                SimulatorOp::TypeText { udid, text } => {
                    Simulator::from_udid(SimulatorDeviceId::new(udid))
                        .type_text(&text)
                        .await
                        .map_err(map_err)?;
                    Ok(json!({ "ok": true }))
                }
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = op;
            Err(SimulatorToolError::UnsupportedPlatform)
        }
    }
}

#[cfg(target_os = "macos")]
fn base64_encode(bytes: &[u8]) -> String {
    // Minimal, std-only base64 encoder. We only need this for the screenshot
    // payload; pulling in the `base64` crate just for one call site would
    // bloat the dep tree (PDX-113 hard constraint).
    const TABLE: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(((bytes.len() + 2) / 3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in chunks.by_ref() {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk[1]) << 8)
            | u32::from(chunk[2]);
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
        out.push(TABLE[(n & 0x3F) as usize] as char);
    }
    let rem = chunks.remainder();
    match rem.len() {
        0 => {}
        1 => {
            let n = u32::from(rem[0]) << 16;
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push('=');
            out.push('=');
        }
        2 => {
            let n = (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8);
            out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        }
        _ => unreachable!(),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mock that records the last op and returns a configured response.
    struct MockExec {
        last: Mutex<Option<SimulatorOp>>,
        response: Mutex<Result<Value, SimulatorToolError>>,
    }

    impl MockExec {
        fn ok(value: Value) -> Self {
            Self {
                last: Mutex::new(None),
                response: Mutex::new(Ok(value)),
            }
        }
        fn err(e: SimulatorToolError) -> Self {
            Self {
                last: Mutex::new(None),
                response: Mutex::new(Err(e)),
            }
        }
    }

    #[async_trait]
    impl SimulatorExecutor for MockExec {
        async fn dispatch(&self, op: SimulatorOp) -> Result<Value, SimulatorToolError> {
            *self.last.lock().unwrap() = Some(op);
            // Replace the inner response with a sentinel so we can hand back
            // the original. We only need single-shot service for tests.
            let mut g = self.response.lock().unwrap();
            std::mem::replace(
                &mut *g,
                Err(SimulatorToolError::Argument("consumed".into())),
            )
        }
    }

    #[tokio::test]
    async fn parse_op_list() {
        let op = parse_op(&json!({ "op": "list" })).unwrap();
        assert_eq!(op, SimulatorOp::List);
    }

    #[tokio::test]
    async fn parse_op_find_requires_name() {
        let err = parse_op(&json!({ "op": "find" })).unwrap_err();
        assert!(matches!(err, SimulatorToolError::Argument(_)));
    }

    #[tokio::test]
    async fn parse_op_tap_requires_numeric_coords() {
        let err = parse_op(&json!({ "op": "tap", "udid": "U", "x": "100", "y": 200 }))
            .unwrap_err();
        assert!(matches!(err, SimulatorToolError::Argument(_)));
    }

    #[tokio::test]
    async fn execute_dispatches_to_executor_with_parsed_op() {
        let mock = Arc::new(MockExec::ok(json!({ "pid": 42 })));
        let tool = SimulatorTool::new(mock.clone());
        let result = tool
            .execute(&json!({
                "op": "launch",
                "udid": "ABC",
                "bundle": "com.example",
            }))
            .await;
        assert_eq!(result.pointer("/pid"), Some(&json!(42)));
        let last = mock.last.lock().unwrap().clone().unwrap();
        assert_eq!(
            last,
            SimulatorOp::Launch {
                udid: "ABC".into(),
                bundle: "com.example".into(),
            }
        );
    }

    #[tokio::test]
    async fn execute_surfaces_argument_error_in_envelope() {
        let mock = Arc::new(MockExec::ok(json!({})));
        let tool = SimulatorTool::new(mock);
        let result = tool.execute(&json!({ "op": "boot" })).await; // missing udid
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("argument_validation")
        );
    }

    #[tokio::test]
    async fn execute_surfaces_executor_error_in_envelope() {
        let mock = Arc::new(MockExec::err(SimulatorToolError::UnsupportedPlatform));
        let tool = SimulatorTool::new(mock);
        let result = tool.execute(&json!({ "op": "list" })).await;
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("unsupported_platform")
        );
    }

    #[tokio::test]
    async fn execute_rejects_unknown_op() {
        let mock = Arc::new(MockExec::ok(json!({})));
        let tool = SimulatorTool::new(mock);
        let result = tool.execute(&json!({ "op": "evaporate" })).await;
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("argument_validation")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn base64_round_trip_for_known_inputs() {
        // Spot-check against the RFC 4648 examples.
        assert_eq!(super::base64_encode(b""), "");
        assert_eq!(super::base64_encode(b"f"), "Zg==");
        assert_eq!(super::base64_encode(b"fo"), "Zm8=");
        assert_eq!(super::base64_encode(b"foo"), "Zm9v");
        assert_eq!(super::base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(super::base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(super::base64_encode(b"foobar"), "Zm9vYmFy");
    }
}
