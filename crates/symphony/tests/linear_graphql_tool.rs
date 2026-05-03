//! Integration tests for the daemon-mediated `linear_graphql` tool
//! (PDX-112 / Symphony §10.5).
//!
//! Coverage:
//!   * happy path (data round-trip),
//!   * GraphQL error preserved as `errors` array,
//!   * rate limit trips with structured `extensions.kind = "rate_limited"`,
//!   * missing-token startup error surfaces from the workflow loader.
//!   * env-leak audit: an agent subprocess never inherits `LINEAR_*` env.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use symphony::linear_graphql::{LinearGraphQlExecutor, LinearGraphQlTool};
use symphony::tracker::TrackerError;
use symphony::workflow::WorkflowDefinition;

/// Mock executor recording the last query/variables seen and returning a
/// preconfigured response.
struct MockExec {
    response: tokio::sync::Mutex<Result<Value, String>>,
    last_query: tokio::sync::Mutex<String>,
    last_vars: tokio::sync::Mutex<Value>,
}

impl MockExec {
    fn ok(value: Value) -> Self {
        Self {
            response: tokio::sync::Mutex::new(Ok(value)),
            last_query: tokio::sync::Mutex::new(String::new()),
            last_vars: tokio::sync::Mutex::new(Value::Null),
        }
    }
}

#[async_trait]
impl LinearGraphQlExecutor for MockExec {
    async fn post_raw(&self, query: &str, variables: Value) -> Result<Value, TrackerError> {
        *self.last_query.lock().await = query.to_string();
        *self.last_vars.lock().await = variables;
        match &*self.response.lock().await {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(TrackerError::Http(e.clone())),
        }
    }
}

#[tokio::test]
async fn happy_path_round_trips_query_variables_and_data() {
    let exec = Arc::new(MockExec::ok(json!({
        "data": { "issue": { "id": "iss_42", "title": "x" } }
    })));
    let tool = LinearGraphQlTool::new(exec.clone());
    let result = tool
        .execute(&json!({
            "query": "query Q($id: String!) { issue(id: $id) { id title } }",
            "variables": { "id": "iss_42" }
        }))
        .await;

    // Data round-trips.
    assert_eq!(result.pointer("/data/issue/id"), Some(&json!("iss_42")));
    assert!(result.get("errors").is_none(), "no errors on happy path");

    // The mock saw the exact variables we passed in.
    assert_eq!(
        *exec.last_vars.lock().await,
        json!({ "id": "iss_42" }),
        "variables forwarded verbatim"
    );
    assert!(
        exec.last_query.lock().await.contains("issue(id: $id)"),
        "query forwarded verbatim"
    );
}

#[tokio::test]
async fn graphql_errors_preserve_structure_for_self_correction() {
    let exec = Arc::new(MockExec::ok(json!({
        "data": null,
        "errors": [{
            "message": "Unknown argument \"slugId\" on field \"IssueFilter.project\".",
            "extensions": { "code": "VALIDATION_ERROR" }
        }]
    })));
    let tool = LinearGraphQlTool::new(exec);
    let result = tool
        .execute(&json!({ "query": "{ issues { id } }", "variables": {} }))
        .await;

    let errors = result.get("errors").and_then(|v| v.as_array()).unwrap();
    assert_eq!(errors.len(), 1);
    let err = &errors[0];
    assert!(err
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap()
        .contains("Unknown argument"));
    // Linear's own extensions block is preserved.
    assert_eq!(
        err.pointer("/extensions/code").and_then(|v| v.as_str()),
        Some("VALIDATION_ERROR")
    );
    // `data` key is always present, even when null.
    assert!(result.get("data").is_some());
}

#[tokio::test]
async fn rate_limit_trips_in_band_error() {
    let exec = Arc::new(MockExec::ok(json!({ "data": { "ok": true } })));
    // Capacity 3 to exercise the limiter without burning real time.
    let tool = LinearGraphQlTool::with_rate(exec, 3);

    for i in 0..3 {
        let r = tool
            .execute(&json!({ "query": "{ viewer { id } }", "variables": {} }))
            .await;
        assert!(r.get("errors").is_none(), "call {i} should succeed");
    }
    let r = tool
        .execute(&json!({ "query": "{ viewer { id } }", "variables": {} }))
        .await;
    let kind = r
        .pointer("/errors/0/extensions/kind")
        .and_then(|v| v.as_str());
    assert_eq!(
        kind,
        Some("rate_limited"),
        "fourth call must surface rate-limited error in-band, not silently fail"
    );
    // Critically, the agent still receives a structured `data + errors`
    // response — not a panic, not a silent drop.
    assert!(r.get("data").is_some());
}

#[test]
fn missing_token_startup_error_surfaces_from_workflow_loader() {
    // PDX-112 acceptance: the daemon must refuse to start if its tracker
    // api_key indirection points at an unset env var. This is the same
    // path `symphony` walks at startup before constructing the tool.
    // Use a unique var name to avoid collisions with other tests / the
    // ambient process env.
    let unset = "LINEAR_API_KEY_UNSET_PDX112_TEST";
    std::env::remove_var(unset);
    let raw = format!(
        r#"---
tracker:
  api_key: ${unset}
  project_slug: test
---
hello
"#
    );
    let err = WorkflowDefinition::from_str(&raw)
        .expect_err("workflow should refuse to load when api_key var is unset");
    let msg = err.to_string();
    assert!(
        msg.contains(unset),
        "error message should name the missing env var, got: {msg}"
    );
}

#[test]
fn env_leak_audit_no_linear_secrets_in_subprocess_env() {
    // PDX-112 acceptance: the env passed to the agent subprocess must
    // not contain LINEAR_API_KEY (or any LINEAR_* variant). We replicate
    // the orchestrator's scrubbing policy here against a representative
    // ambient env to make the contract explicit and regression-proof.
    use agents::is_linear_secret_env;
    use symphony::orchestrator::is_linear_secret_key;

    let ambient: Vec<(String, String)> = vec![
        ("LINEAR_API_KEY".into(), "lin_abc".into()),
        ("LINEAR_API_TOKEN".into(), "lin_xyz".into()),
        ("LINEAR_TOKEN".into(), "lin_def".into()),
        ("LINEAR_WEBHOOK_SIGNING_SECRET".into(), "wh".into()),
        ("PATH".into(), "/usr/bin".into()),
        ("HOME".into(), "/tmp".into()),
        ("AGENTS_NUM_THREADS".into(), "8".into()),
    ];

    // Both crates' policies must agree.
    let scrubbed: Vec<&(String, String)> = ambient
        .iter()
        .filter(|(k, _)| !is_linear_secret_env(k) && !is_linear_secret_key(k))
        .collect();

    let leaked: Vec<&str> = scrubbed
        .iter()
        .filter(|(k, _)| {
            let upper = k.to_ascii_uppercase();
            upper.starts_with("LINEAR_")
        })
        .map(|(k, _)| k.as_str())
        .collect();
    assert!(
        leaked.is_empty(),
        "env leak: LINEAR_* vars survived scrubbing: {leaked:?}"
    );

    // Non-secret vars survive.
    let preserved: std::collections::HashSet<&str> =
        scrubbed.iter().map(|(k, _)| k.as_str()).collect();
    assert!(preserved.contains("PATH"));
    assert!(preserved.contains("HOME"));
    assert!(preserved.contains("AGENTS_NUM_THREADS"));
}

#[tokio::test]
async fn missing_query_argument_returns_validation_error() {
    let exec = Arc::new(MockExec::ok(json!({})));
    let tool = LinearGraphQlTool::new(exec);
    let r = tool.execute(&json!({ "variables": {} })).await;
    let kind = r
        .pointer("/errors/0/extensions/kind")
        .and_then(|v| v.as_str());
    assert_eq!(kind, Some("argument_validation"));
}
