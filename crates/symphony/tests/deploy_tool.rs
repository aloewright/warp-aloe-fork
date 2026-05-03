//! Integration tests for the daemon-mediated `deploy` tool (PDX-114 [E2]).
//!
//! Coverage:
//! * happy path → tool returns `{ workflow_instance_id,
//!   status: "awaiting_approval" }` and the underlying client was
//!   called with the resolved approver / secrets / kind from
//!   `WORKFLOW.md`'s `deploys.<target>` block.
//! * an agent that omits the target argument hits a structured
//!   `argument_validation` error (no panic, no silent drop).
//! * an agent that requests an undeclared target hits `not_configured`.
//! * an agent that requests a disallowed env hits
//!   `environment_not_allowed`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};
use symphony::deploy_tool::{
    DeployConfigResolver, DeployTool, DeployToolError, DeployWorkflowClient,
    DeployWorkflowParams,
};
use symphony::workflow::DeployConfig;

struct FixedResolver(HashMap<String, DeployConfig>);
impl DeployConfigResolver for FixedResolver {
    fn lookup(&self, target: &str) -> Option<DeployConfig> {
        self.0.get(target).cloned()
    }
}

#[derive(Default)]
struct RecordingClient {
    last: Mutex<Option<DeployWorkflowParams>>,
    instance_id: String,
}
#[async_trait]
impl DeployWorkflowClient for RecordingClient {
    async fn create_instance(
        &self,
        params: &DeployWorkflowParams,
    ) -> Result<String, DeployToolError> {
        *self.last.lock().unwrap() = Some(params.clone());
        Ok(self.instance_id.clone())
    }
}

fn cfg(kind: &str) -> DeployConfig {
    DeployConfig {
        kind: kind.into(),
        environments: vec!["staging".into(), "production".into()],
        approvers: vec!["alice@example.com".into(), "bob@example.com".into()],
        secrets: vec!["CLOUDFLARE_API_TOKEN".into()],
        approval_timeout: Some("24 hours".into()),
    }
}

fn args(target: &str, env_name: &str) -> Value {
    json!({
        "deploy_id": "deploy-PDX-99-attempt-1",
        "target": target,
        "env_name": env_name,
        "artifact": "deadbeefcafe",
    })
}

#[tokio::test]
async fn agent_tool_call_creates_workflow_instance_and_returns_id() {
    let mut configs = HashMap::new();
    configs.insert("helm-control-plane".into(), cfg("cloudflare_worker"));

    let client = Arc::new(RecordingClient {
        last: Mutex::new(None),
        instance_id: "deploy-instance-42".into(),
    });
    let tool = DeployTool::new(client.clone(), Arc::new(FixedResolver(configs)));

    let envelope = tool.execute(&args("helm-control-plane", "production")).await;

    // Agent receives workflow id + awaiting-approval status — never the
    // underlying `wrangler deploy --env production` invocation.
    assert_eq!(
        envelope.get("workflow_instance_id").and_then(|v| v.as_str()),
        Some("deploy-instance-42")
    );
    assert_eq!(
        envelope.get("status").and_then(|v| v.as_str()),
        Some("awaiting_approval")
    );
    let approvers = envelope
        .get("approvers")
        .and_then(|v| v.as_array())
        .expect("approvers echoed back");
    assert_eq!(approvers.len(), 2);

    // Daemon-side params reflect the WORKFLOW.md config: kind +
    // approvers + secrets all sourced from `deploys.<target>`, not
    // from the agent's tool args.
    let last = client.last.lock().unwrap().clone().unwrap();
    assert_eq!(last.target, "helm-control-plane");
    assert_eq!(last.env_name, "production");
    assert_eq!(last.kind, "cloudflare_worker");
    assert_eq!(last.approvers, vec!["alice@example.com", "bob@example.com"]);
    assert_eq!(last.secrets, vec!["CLOUDFLARE_API_TOKEN".to_string()]);
    assert_eq!(last.approval_timeout.as_deref(), Some("24 hours"));
}

#[tokio::test]
async fn missing_target_returns_argument_validation() {
    let configs: HashMap<String, DeployConfig> = HashMap::new();
    let client = Arc::new(RecordingClient::default());
    let tool = DeployTool::new(client, Arc::new(FixedResolver(configs)));
    let envelope = tool
        .execute(&json!({
            "deploy_id": "x", "env_name": "production", "artifact": "y"
        }))
        .await;
    assert_eq!(
        envelope.pointer("/error/kind").and_then(|v| v.as_str()),
        Some("argument_validation")
    );
}

#[tokio::test]
async fn unknown_target_returns_not_configured() {
    let configs: HashMap<String, DeployConfig> = HashMap::new();
    let client = Arc::new(RecordingClient::default());
    let tool = DeployTool::new(client, Arc::new(FixedResolver(configs)));
    let envelope = tool.execute(&args("helm-control-plane", "production")).await;
    assert_eq!(
        envelope.pointer("/error/kind").and_then(|v| v.as_str()),
        Some("not_configured")
    );
}

#[tokio::test]
async fn disallowed_env_returns_environment_not_allowed() {
    let mut configs = HashMap::new();
    let mut c = cfg("cloudflare_worker");
    c.environments = vec!["staging".into()]; // production NOT allowed
    configs.insert("helm-control-plane".into(), c);
    let client = Arc::new(RecordingClient::default());
    let tool = DeployTool::new(client, Arc::new(FixedResolver(configs)));
    let envelope = tool.execute(&args("helm-control-plane", "production")).await;
    assert_eq!(
        envelope.pointer("/error/kind").and_then(|v| v.as_str()),
        Some("environment_not_allowed")
    );
}

#[tokio::test]
async fn target_with_no_approvers_refuses() {
    let mut configs = HashMap::new();
    let mut c = cfg("cloudflare_worker");
    c.approvers.clear();
    configs.insert("helm-control-plane".into(), c);
    let client = Arc::new(RecordingClient::default());
    let tool = DeployTool::new(client, Arc::new(FixedResolver(configs)));
    let envelope = tool.execute(&args("helm-control-plane", "production")).await;
    assert_eq!(
        envelope.pointer("/error/kind").and_then(|v| v.as_str()),
        Some("no_approvers")
    );
}
