//! `deploy` daemon-mediated tool (PDX-114 [E2]).
//!
//! Mirrors the [`crate::linear_graphql`] pattern from PDX-112 §10.5: the
//! agent emits a `tool_use` event with `name = "deploy"` and structured
//! arguments; Symphony intercepts it from the daemon, validates against
//! the [`crate::workflow::DeployConfig`] for the requested target, and
//! creates a Cloudflare DeployWorkflow instance via
//! `POST /api/workflows/deploy/instances`. The agent never sees the
//! deploy command itself — it gets back a `{ workflow_instance_id,
//! status: "awaiting_approval" }` envelope and learns to wait for the
//! human approval to land.
//!
//! This is the security inversion that makes "agent triggers production
//! deploy" actually safe:
//!
//! * The daemon — not the agent subprocess — holds the Cloudflare API
//!   token used to talk to the control-plane Worker. The agent runtime
//!   environment is scrubbed of any deploy credentials (Cloudflare,
//!   npm, GitHub) before the agent is spawned.
//! * The deploy tool refuses any target / env combination that is not
//!   explicitly declared in `WORKFLOW.md`'s `deploys.<target>` block,
//!   so an agent invoking `tool: deploy` with an arbitrary target hits
//!   a structured `{ error: { kind: "not_configured" } }` error rather
//!   than escalating into the build/test/deploy pipeline.
//! * The approval gate is consumed asynchronously by
//!   [`crate::approval_poller`] watching Linear comments, so the agent
//!   has no way to satisfy the gate itself.
//!
//! ## Tool surface
//!
//! Single argument: a JSON object with the shape:
//!
//! ```json
//! {
//!   "target": "helm-control-plane",
//!   "env_name": "production",
//!   "artifact": "deadbeef",
//!   "deploy_id": "PDX-99-attempt-1",
//!   "rationale": "release patch for PDX-99"
//! }
//! ```
//!
//! The tool returns:
//!
//! ```json
//! {
//!   "workflow_instance_id": "deploy-abc",
//!   "status": "awaiting_approval",
//!   "target": "helm-control-plane",
//!   "env_name": "production",
//!   "approvers": ["alice@example.com"]
//! }
//! ```
//!
//! Errors are returned as `{ error: { kind, message } }`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::workflow::DeployConfig;

/// Tool name advertised on the agent boundary.
pub const TOOL_NAME: &str = "deploy";

/// Daemon-side handle for the deploy tool.
///
/// Owns:
/// * a [`DeployWorkflowClient`] used to talk to the control-plane
///   Worker that hosts `DeployWorkflow`, and
/// * a snapshot view of the configured `deploys.<target>` map (keyed
///   lookup happens at every tool call so live-reload picks up new
///   targets without a restart).
#[derive(Clone)]
pub struct DeployTool {
    client: Arc<dyn DeployWorkflowClient>,
    /// Resolver for the live `deploys.<target>` map. We accept an
    /// `Arc<dyn Fn() -> HashMap>` rather than a snapshot so live-reload
    /// (PDX-111) lets operators add a new deploy target without
    /// restarting the daemon.
    resolver: Arc<dyn DeployConfigResolver>,
}

/// Resolver trait so the tool always reads the *current* `deploys`
/// snapshot. Production wires this to the [`crate::reload::WorkflowHandle`].
pub trait DeployConfigResolver: Send + Sync {
    /// Look up the deploy config for `target`. Returns `None` if the
    /// target is not declared in `WORKFLOW.md`.
    fn lookup(&self, target: &str) -> Option<DeployConfig>;
}

/// Outbound transport that creates a DeployWorkflow instance via the
/// control-plane Worker's HTTP surface. Tests inject a mock; production
/// wires an HTTP client pointed at the configured control-plane URL.
#[async_trait]
pub trait DeployWorkflowClient: Send + Sync {
    /// Create a new DeployWorkflow instance and return the instance id.
    async fn create_instance(
        &self,
        params: &DeployWorkflowParams,
    ) -> Result<String, DeployToolError>;
}

/// Strongly-typed payload mirroring the TS
/// `DeployWorkflowParams` shape in
/// `cloudflare-control-plane/src/workers/workflows/types.ts`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployWorkflowParams {
    /// Caller-stable id; the control-plane Worker uses this as the
    /// Workflow instance id when we set `id` on the create call. We
    /// expose it as part of the params so the daemon can keep its own
    /// `deploy_id` aligned with the Workflow instance id.
    pub deploy_id: String,
    /// Target name (matches the `deploys.<target>` map key).
    pub target: String,
    /// Build artifact reference (git sha, R2 object, container digest).
    pub artifact: String,
    /// Approver allowlist resolved from `DeployConfig.approvers`.
    pub approvers: Vec<String>,
    /// Optional approval timeout, propagated from
    /// `DeployConfig.approval_timeout`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_timeout: Option<String>,
    /// Environment name (e.g. `"production"`). Validated against
    /// `DeployConfig.environments` before this struct is built.
    pub env_name: String,
    /// Deploy kind (`cloudflare_worker`, `npm_publish`, etc.). Mirrored
    /// for downstream consumers; the workflow Worker decides which step
    /// command to run from this field.
    pub kind: String,
    /// Doppler-fronted secret env var names to inject on the deploy
    /// step (e.g. `["CLOUDFLARE_API_TOKEN"]`).
    #[serde(default)]
    pub secrets: Vec<String>,
}

/// Errors raised by the deploy tool. All errors are surfaced to the
/// agent as a structured `{ error: { kind, message } }` envelope rather
/// than panics or `Err(_)` — the agent is expected to read them and
/// either correct its arguments or back off.
#[derive(Debug, thiserror::Error)]
pub enum DeployToolError {
    /// Argument validation failed (missing field, wrong type, etc.).
    #[error("argument validation: {0}")]
    Argument(String),
    /// Target not declared in `WORKFLOW.md`'s `deploys.<target>` map.
    #[error("target `{0}` is not configured under deploys.<target> in WORKFLOW.md")]
    NotConfigured(String),
    /// Environment not on the allowlist for this target.
    #[error("environment `{env}` is not allowed for target `{target}` (allowed: {allowed:?})")]
    EnvironmentNotAllowed {
        /// Requested env.
        env: String,
        /// Target.
        target: String,
        /// Allowlist from config.
        allowed: Vec<String>,
    },
    /// Approver allowlist is empty — the deploy tool refuses to create
    /// a workflow that nobody can approve.
    #[error("target `{0}` has no approvers configured; refusing to enqueue an unapprovable deploy")]
    NoApprovers(String),
    /// Control-plane HTTP call failed.
    #[error("control plane: {0}")]
    Transport(String),
}

impl DeployToolError {
    fn kind(&self) -> &'static str {
        match self {
            Self::Argument(_) => "argument_validation",
            Self::NotConfigured(_) => "not_configured",
            Self::EnvironmentNotAllowed { .. } => "environment_not_allowed",
            Self::NoApprovers(_) => "no_approvers",
            Self::Transport(_) => "transport",
        }
    }
}

impl DeployTool {
    /// Construct from a client + resolver pair.
    pub fn new(
        client: Arc<dyn DeployWorkflowClient>,
        resolver: Arc<dyn DeployConfigResolver>,
    ) -> Self {
        Self { client, resolver }
    }

    /// Execute one tool call. Always returns a JSON envelope —
    /// argument / config / transport errors are surfaced as
    /// `{ error: { kind, message } }` rather than `Err(_)` so the agent
    /// can self-correct.
    pub async fn execute(&self, args: &Value) -> Value {
        match self.execute_inner(args).await {
            Ok(v) => v,
            Err(e) => error_envelope(&e),
        }
    }

    async fn execute_inner(&self, args: &Value) -> Result<Value, DeployToolError> {
        let parsed = parse_args(args)?;

        // Look up the per-target config from the live workflow.
        let cfg = self
            .resolver
            .lookup(&parsed.target)
            .ok_or_else(|| DeployToolError::NotConfigured(parsed.target.clone()))?;

        if cfg.approvers.is_empty() {
            return Err(DeployToolError::NoApprovers(parsed.target.clone()));
        }

        // The env must be in the configured allowlist. Empty allowlist
        // is treated as "no envs allowed" — operators must declare at
        // least one valid env to enable a target.
        if !cfg.environments.iter().any(|e| e == &parsed.env_name) {
            return Err(DeployToolError::EnvironmentNotAllowed {
                env: parsed.env_name.clone(),
                target: parsed.target.clone(),
                allowed: cfg.environments.clone(),
            });
        }

        let params = DeployWorkflowParams {
            deploy_id: parsed.deploy_id.clone(),
            target: parsed.target.clone(),
            artifact: parsed.artifact.clone(),
            approvers: cfg.approvers.clone(),
            approval_timeout: cfg.approval_timeout.clone(),
            env_name: parsed.env_name.clone(),
            kind: cfg.kind.clone(),
            secrets: cfg.secrets.clone(),
        };

        let instance_id = self.client.create_instance(&params).await?;

        Ok(json!({
            "workflow_instance_id": instance_id,
            "status": "awaiting_approval",
            "target": parsed.target,
            "env_name": parsed.env_name,
            "approvers": cfg.approvers,
        }))
    }
}

/// Parsed tool arguments after the JSON validation step.
#[derive(Debug, Clone)]
struct ParsedArgs {
    deploy_id: String,
    target: String,
    env_name: String,
    artifact: String,
}

fn parse_args(args: &Value) -> Result<ParsedArgs, DeployToolError> {
    let obj = args
        .as_object()
        .ok_or_else(|| DeployToolError::Argument("args must be a JSON object".into()))?;

    let str_field = |k: &str| -> Result<String, DeployToolError> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                DeployToolError::Argument(format!("missing or empty string field `{k}`"))
            })
    };

    Ok(ParsedArgs {
        deploy_id: str_field("deploy_id")?,
        target: str_field("target")?,
        env_name: str_field("env_name")?,
        artifact: str_field("artifact")?,
    })
}

fn error_envelope(e: &DeployToolError) -> Value {
    json!({
        "error": {
            "kind": e.kind(),
            "message": e.to_string(),
        }
    })
}

// ---------------------------------------------------------------------------
// Default HTTP-backed client
// ---------------------------------------------------------------------------

/// Production [`DeployWorkflowClient`] that POSTs to
/// `<control_plane_url>/api/workflows/deploy/instances` with the
/// daemon-held `helm` JWT in the `Authorization` header.
///
/// This struct is light by design: it only does the one HTTP call, and
/// any retry / circuit-breaking belongs in the workflow runtime, not in
/// the tool layer.
pub struct HttpDeployWorkflowClient {
    base_url: String,
    bearer_token: Option<String>,
    http: reqwest::Client,
}

impl HttpDeployWorkflowClient {
    /// Construct a new client. `base_url` must be the origin of the
    /// control-plane Worker (e.g. `"https://helm.example.com"`); the
    /// `/api/workflows/deploy/instances` path is appended at call time.
    pub fn new(base_url: impl Into<String>, bearer_token: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            bearer_token,
            http: reqwest::Client::builder()
                .build()
                .expect("reqwest client builds"),
        }
    }
}

#[async_trait]
impl DeployWorkflowClient for HttpDeployWorkflowClient {
    async fn create_instance(
        &self,
        params: &DeployWorkflowParams,
    ) -> Result<String, DeployToolError> {
        let url = format!(
            "{}/api/workflows/deploy/instances",
            self.base_url.trim_end_matches('/')
        );
        let body = json!({
            "id": params.deploy_id,
            "params": params,
        });
        let mut req = self.http.post(&url).json(&body);
        if let Some(tok) = &self.bearer_token {
            req = req.bearer_auth(tok);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| DeployToolError::Transport(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| DeployToolError::Transport(format!("body read: {e}")))?;
        if !status.is_success() {
            return Err(DeployToolError::Transport(format!(
                "POST {url} returned {status}: {text}"
            )));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| DeployToolError::Transport(format!("json parse: {e}: {text}")))?;
        let id = value
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DeployToolError::Transport(format!("response missing `id` field: {text}"))
            })?
            .to_string();
        Ok(id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Test resolver backed by an in-memory map.
    struct FixedResolver(HashMap<String, DeployConfig>);

    impl DeployConfigResolver for FixedResolver {
        fn lookup(&self, target: &str) -> Option<DeployConfig> {
            self.0.get(target).cloned()
        }
    }

    /// Mock client that records the params it was asked to create and
    /// returns a configured instance id.
    struct MockClient {
        instance_id: String,
        last: Mutex<Option<DeployWorkflowParams>>,
    }

    impl MockClient {
        fn new(instance_id: &str) -> Self {
            Self {
                instance_id: instance_id.to_string(),
                last: Mutex::new(None),
            }
        }
        fn last(&self) -> Option<DeployWorkflowParams> {
            self.last.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DeployWorkflowClient for MockClient {
        async fn create_instance(
            &self,
            params: &DeployWorkflowParams,
        ) -> Result<String, DeployToolError> {
            *self.last.lock().unwrap() = Some(params.clone());
            Ok(self.instance_id.clone())
        }
    }

    fn cfg() -> DeployConfig {
        DeployConfig {
            kind: "cloudflare_worker".into(),
            environments: vec!["staging".into(), "production".into()],
            approvers: vec!["alice@example.com".into()],
            secrets: vec!["CLOUDFLARE_API_TOKEN".into()],
            approval_timeout: Some("24 hours".into()),
        }
    }

    fn tool(client: Arc<MockClient>, configs: HashMap<String, DeployConfig>) -> DeployTool {
        DeployTool::new(client, Arc::new(FixedResolver(configs)))
    }

    fn args() -> Value {
        json!({
            "deploy_id": "deploy-xyz",
            "target": "helm-control-plane",
            "env_name": "production",
            "artifact": "deadbeef",
        })
    }

    #[tokio::test]
    async fn happy_path_returns_workflow_instance_id() {
        let client = Arc::new(MockClient::new("deploy-abc"));
        let mut configs = HashMap::new();
        configs.insert("helm-control-plane".into(), cfg());

        let tool = tool(client.clone(), configs);
        let result = tool.execute(&args()).await;

        assert_eq!(
            result.get("workflow_instance_id").and_then(|v| v.as_str()),
            Some("deploy-abc")
        );
        assert_eq!(
            result.get("status").and_then(|v| v.as_str()),
            Some("awaiting_approval")
        );
        // Approvers echoed back so the agent knows who to mention in
        // its Linear comment.
        let approvers = result
            .get("approvers")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(approvers.len(), 1);

        // Mock saw the right params, including resolved secrets.
        let last = client.last().unwrap();
        assert_eq!(last.target, "helm-control-plane");
        assert_eq!(last.env_name, "production");
        assert_eq!(last.kind, "cloudflare_worker");
        assert_eq!(last.secrets, vec!["CLOUDFLARE_API_TOKEN".to_string()]);
        assert_eq!(last.approvers, vec!["alice@example.com".to_string()]);
    }

    #[tokio::test]
    async fn rejects_missing_target_argument() {
        let client = Arc::new(MockClient::new("deploy-abc"));
        let configs = HashMap::new();
        let tool = tool(client, configs);
        let result = tool.execute(&json!({
            "deploy_id": "x",
            "env_name": "production",
            "artifact": "y",
        })).await;
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("argument_validation")
        );
    }

    #[tokio::test]
    async fn rejects_unconfigured_target() {
        let client = Arc::new(MockClient::new("deploy-abc"));
        let configs = HashMap::new();
        let tool = tool(client, configs);
        let result = tool.execute(&args()).await;
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("not_configured")
        );
    }

    #[tokio::test]
    async fn rejects_disallowed_environment() {
        let client = Arc::new(MockClient::new("deploy-abc"));
        let mut configs = HashMap::new();
        let mut c = cfg();
        c.environments = vec!["staging".into()];
        configs.insert("helm-control-plane".into(), c);

        let tool = tool(client, configs);
        let result = tool.execute(&args()).await;
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("environment_not_allowed")
        );
        // Error message should mention what *was* allowed.
        assert!(result
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap()
            .contains("staging"));
    }

    #[tokio::test]
    async fn rejects_target_with_empty_approver_list() {
        let client = Arc::new(MockClient::new("deploy-abc"));
        let mut configs = HashMap::new();
        let mut c = cfg();
        c.approvers.clear();
        configs.insert("helm-control-plane".into(), c);
        let tool = tool(client, configs);
        let result = tool.execute(&args()).await;
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("no_approvers")
        );
    }

    #[tokio::test]
    async fn passes_approval_timeout_through_to_workflow_params() {
        let client = Arc::new(MockClient::new("deploy-abc"));
        let mut configs = HashMap::new();
        configs.insert("helm-control-plane".into(), cfg());
        let tool = tool(client.clone(), configs);
        tool.execute(&args()).await;
        let last = client.last().unwrap();
        assert_eq!(last.approval_timeout.as_deref(), Some("24 hours"));
    }

    #[tokio::test]
    async fn transport_error_surfaces_kind_transport() {
        struct FailingClient;
        #[async_trait]
        impl DeployWorkflowClient for FailingClient {
            async fn create_instance(
                &self,
                _: &DeployWorkflowParams,
            ) -> Result<String, DeployToolError> {
                Err(DeployToolError::Transport("connection refused".into()))
            }
        }
        let mut configs = HashMap::new();
        configs.insert("helm-control-plane".into(), cfg());
        let tool = DeployTool::new(
            Arc::new(FailingClient),
            Arc::new(FixedResolver(configs)),
        );
        let result = tool.execute(&args()).await;
        assert_eq!(
            result.pointer("/error/kind").and_then(|v| v.as_str()),
            Some("transport")
        );
    }
}
