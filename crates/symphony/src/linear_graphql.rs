//! `linear_graphql` daemon-mediated tool (PDX-112 / Symphony §10.5).
//!
//! Exposes Linear GraphQL access to the agent without ever placing the API
//! token in the subprocess environment. The agent emits a tool call named
//! `linear_graphql` with `{ query, variables }`; Symphony intercepts the call
//! in the agent event stream, executes the request from the daemon using the
//! existing [`LinearClient`] transport, and emits a synthetic
//! [`AgentEvent::ToolResult`] carrying the structured GraphQL response back
//! into the audit log (and, in the broader app, back to the agent via the
//! MCP forwarder).
//!
//! The token NEVER appears in agent stdin/stdout/stderr or the subprocess
//! env. The orchestrator strips any `LINEAR_*` variables out of the
//! [`Task::context`] env before spawn (see `orchestrator::run_agent`).
//!
//! ## Rate-limit
//!
//! Default 30 calls / minute / agent run, configurable via `WORKFLOW.md`'s
//! `agent.linear_graphql_rate_per_minute`. When the limiter trips, the tool
//! returns an in-band error (`{ errors: [{ message: "rate limit exceeded" }],
//! data: null }`) so the agent can self-correct rather than silently fail.
//!
//! ## Error shape
//!
//! All responses are wrapped as `{ data: <value or null>, errors: <array or
//! omitted> }`, mirroring the GraphQL spec. Network / transport failures are
//! surfaced as `errors` entries with a `extensions.kind = "transport"` tag.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::tracker::{LinearClient, TrackerError};

/// Tool name advertised on the agent boundary. Constant so that both the
/// agent registration side (Claude Code MCP forwarder, Codex tool list) and
/// the interception side agree on the spelling.
pub const TOOL_NAME: &str = "linear_graphql";

/// Default rate limit (calls per minute per agent run).
///
/// Symphony §10.5 calls out 30/minute as the safe default; this can be
/// overridden via `WorkflowConfig.agent.linear_graphql_rate_per_minute`.
pub const DEFAULT_RATE_PER_MINUTE: u32 = 30;

/// Daemon-side handle to the [`LinearClient`] that executes the GraphQL
/// request on the agent's behalf. Cheaply cloneable — the inner state is
/// behind an `Arc`/`Mutex`.
#[derive(Clone)]
pub struct LinearGraphQlTool {
    client: Arc<dyn LinearGraphQlExecutor>,
    limiter: Arc<Mutex<RateLimiter>>,
}

/// Tracker-agnostic executor trait so tests can supply a mock without
/// running an actual `reqwest` client. The production implementation is
/// blanket-impl'd for `LinearClient`.
#[async_trait]
pub trait LinearGraphQlExecutor: Send + Sync {
    /// POST a raw GraphQL `{ query, variables }` body and return the parsed
    /// JSON response (`{ data, errors }`).
    async fn post_raw(&self, query: &str, variables: Value) -> Result<Value, TrackerError>;
}

#[async_trait]
impl LinearGraphQlExecutor for LinearClient {
    async fn post_raw(&self, query: &str, variables: Value) -> Result<Value, TrackerError> {
        // Reuse the existing transport via the `LinearClient::post_raw_query`
        // helper so we don't open a second HTTP path.
        LinearClient::post_raw_query(self, query, variables).await
    }
}

impl LinearGraphQlTool {
    /// Construct a new tool with the default rate limit.
    pub fn new(client: Arc<dyn LinearGraphQlExecutor>) -> Self {
        Self::with_rate(client, DEFAULT_RATE_PER_MINUTE)
    }

    /// Construct with an explicit per-minute rate limit. `0` disables the
    /// limiter (useful in tests).
    pub fn with_rate(client: Arc<dyn LinearGraphQlExecutor>, rate_per_minute: u32) -> Self {
        Self {
            client,
            limiter: Arc::new(Mutex::new(RateLimiter::new(
                rate_per_minute,
                Duration::from_secs(60),
            ))),
        }
    }

    /// Execute one tool call. Always returns a structured GraphQL-shape JSON
    /// value (`{ data, errors }`). Network / transport / rate-limit errors
    /// are surfaced as `errors` array entries rather than panics or
    /// `Err(_)` so the agent can read them and self-correct.
    pub async fn execute(&self, args: &Value) -> Value {
        // Validate the argument shape up front.
        let Some(query) = args.get("query").and_then(|v| v.as_str()) else {
            return graphql_error(
                "missing or non-string `query` field",
                "argument_validation",
            );
        };
        let variables = args
            .get("variables")
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        if !variables.is_object() && !variables.is_null() {
            return graphql_error(
                "`variables` must be an object",
                "argument_validation",
            );
        }

        // Rate-limit gate.
        if !self.limiter.lock().await.allow(Instant::now()) {
            return graphql_error(
                "rate limit exceeded for linear_graphql tool",
                "rate_limited",
            );
        }

        match self.client.post_raw(query, variables).await {
            Ok(value) => normalize_response(value),
            Err(e) => graphql_error(&format!("transport error: {e}"), "transport"),
        }
    }
}

/// Build a GraphQL-shaped error envelope.
fn graphql_error(message: &str, kind: &str) -> Value {
    json!({
        "data": Value::Null,
        "errors": [{
            "message": message,
            "extensions": { "kind": kind },
        }],
    })
}

/// Ensure the response always has both keys, even if Linear omitted one of
/// them in the success path (`data` only) or error path (`errors` only).
fn normalize_response(value: Value) -> Value {
    let data = value.get("data").cloned().unwrap_or(Value::Null);
    let errors = value.get("errors").cloned();
    let mut obj = serde_json::Map::new();
    obj.insert("data".to_string(), data);
    if let Some(e) = errors {
        obj.insert("errors".to_string(), e);
    }
    Value::Object(obj)
}

// ---------------------------------------------------------------------------
// Rate limiter — fixed-window token bucket sized at one minute.
// ---------------------------------------------------------------------------

/// Simple sliding-window rate limiter. Records timestamps of recent calls
/// and rejects when the count within `window` exceeds `capacity`.
#[derive(Debug)]
struct RateLimiter {
    capacity: u32,
    window: Duration,
    events: std::collections::VecDeque<Instant>,
}

impl RateLimiter {
    fn new(capacity: u32, window: Duration) -> Self {
        Self {
            capacity,
            window,
            events: std::collections::VecDeque::new(),
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        if self.capacity == 0 {
            return true;
        }
        // Drop expired entries.
        let cutoff = now.checked_sub(self.window);
        if let Some(cutoff) = cutoff {
            while let Some(&front) = self.events.front() {
                if front < cutoff {
                    self.events.pop_front();
                } else {
                    break;
                }
            }
        }
        if self.events.len() as u32 >= self.capacity {
            return false;
        }
        self.events.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Mock executor that records calls and yields a configured response.
    struct MockExecutor {
        response: Mutex<Result<Value, TrackerError>>,
        calls: AtomicU32,
    }

    impl MockExecutor {
        fn ok(value: Value) -> Self {
            Self {
                response: Mutex::new(Ok(value)),
                calls: AtomicU32::new(0),
            }
        }
        fn err(e: TrackerError) -> Self {
            Self {
                response: Mutex::new(Err(e)),
                calls: AtomicU32::new(0),
            }
        }
    }

    #[async_trait]
    impl LinearGraphQlExecutor for MockExecutor {
        async fn post_raw(&self, _q: &str, _v: Value) -> Result<Value, TrackerError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Clone the inner Result rather than draining it so the mock can
            // service repeated calls (rate-limit test needs >30).
            let guard = self.response.lock().await;
            match &*guard {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(TrackerError::GraphQl(e.to_string())),
            }
        }
    }

    fn args(query: &str) -> Value {
        json!({ "query": query, "variables": {} })
    }

    #[tokio::test]
    async fn happy_path_normalizes_response() {
        let exec = Arc::new(MockExecutor::ok(json!({
            "data": { "viewer": { "id": "u_1" } }
        })));
        let tool = LinearGraphQlTool::new(exec.clone());
        let result = tool.execute(&args("{ viewer { id } }")).await;
        assert_eq!(result.pointer("/data/viewer/id"), Some(&json!("u_1")));
        assert!(result.get("errors").is_none());
        assert_eq!(exec.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn graphql_error_surfaces_in_errors_array() {
        let exec = Arc::new(MockExecutor::ok(json!({
            "data": null,
            "errors": [{ "message": "bad query" }]
        })));
        let tool = LinearGraphQlTool::new(exec);
        let result = tool.execute(&args("garbage")).await;
        let errors = result.get("errors").and_then(|v| v.as_array()).unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].get("message").and_then(|v| v.as_str()), Some("bad query"));
        // data key always present even when null.
        assert!(result.get("data").is_some());
    }

    #[tokio::test]
    async fn missing_query_returns_validation_error() {
        let exec = Arc::new(MockExecutor::ok(json!({})));
        let tool = LinearGraphQlTool::new(exec);
        let result = tool.execute(&json!({ "variables": {} })).await;
        assert_eq!(
            result
                .pointer("/errors/0/extensions/kind")
                .and_then(|v| v.as_str()),
            Some("argument_validation")
        );
    }

    #[tokio::test]
    async fn transport_error_surfaces_as_errors_kind_transport() {
        let exec = Arc::new(MockExecutor::err(TrackerError::Http("net down".into())));
        let tool = LinearGraphQlTool::new(exec);
        let result = tool.execute(&args("{ viewer { id } }")).await;
        let kind = result
            .pointer("/errors/0/extensions/kind")
            .and_then(|v| v.as_str());
        assert_eq!(kind, Some("transport"));
    }

    #[tokio::test]
    async fn rate_limit_trips_after_capacity() {
        let exec = Arc::new(MockExecutor::ok(json!({ "data": { "ok": true } })));
        // Capacity 2 to keep the test fast.
        let tool = LinearGraphQlTool::with_rate(exec.clone(), 2);

        let r1 = tool.execute(&args("{}")).await;
        assert!(r1.get("errors").is_none(), "first call should succeed");
        let r2 = tool.execute(&args("{}")).await;
        assert!(r2.get("errors").is_none(), "second call should succeed");
        let r3 = tool.execute(&args("{}")).await;
        let kind = r3
            .pointer("/errors/0/extensions/kind")
            .and_then(|v| v.as_str());
        assert_eq!(kind, Some("rate_limited"));
        // Importantly: the third call never reached the executor.
        assert_eq!(exec.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rate_limiter_allows_zero_capacity_as_disabled() {
        let mut rl = RateLimiter::new(0, Duration::from_secs(60));
        for _ in 0..1_000 {
            assert!(rl.allow(Instant::now()));
        }
    }

    #[test]
    fn rate_limiter_recovers_after_window() {
        let mut rl = RateLimiter::new(2, Duration::from_millis(50));
        let t0 = Instant::now();
        assert!(rl.allow(t0));
        assert!(rl.allow(t0));
        assert!(!rl.allow(t0));
        // Advance past the window.
        let t1 = t0 + Duration::from_millis(100);
        assert!(rl.allow(t1));
    }
}
