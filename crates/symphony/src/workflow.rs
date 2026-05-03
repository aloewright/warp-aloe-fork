//! WORKFLOW.md loader and config schema (spec §5).
//!
//! A workflow file is a Markdown document with optional YAML front matter
//! delimited by `---` lines. The front matter configures tracker, polling,
//! workspace, hook, and agent defaults; the body is a Liquid template that
//! is rendered per-issue to produce the prompt sent to the agent.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::tracker::Issue;

/// Top-level parsed workflow file.
#[derive(Debug, Clone)]
pub struct WorkflowDefinition {
    /// Configuration parsed from the YAML front matter.
    pub config: WorkflowConfig,
    /// Liquid template body used to render per-issue prompts.
    pub prompt_template: String,
}

/// Front-matter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowConfig {
    /// Issue tracker settings.
    pub tracker: TrackerConfig,
    /// Polling cadence.
    #[serde(default)]
    pub polling: PollingConfig,
    /// Workspace placement.
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    /// Optional shell hooks.
    #[serde(default)]
    pub hooks: HooksConfig,
    /// Agent dispatch knobs.
    #[serde(default)]
    pub agent: AgentConfig,
    /// Optional `server` extension (PDX-26 D3): cron triggers and the
    /// HMAC-validated webhook receiver. When omitted, neither surface
    /// is started.
    #[serde(default)]
    pub server: ServerConfig,
    /// PDX-114 [E2]: per-target deploy config. Map of target name (e.g.
    /// `"helm-control-plane"`) to a [`DeployConfig`] describing the
    /// deploy kind, environment knobs, secrets refs, and the approver
    /// allowlist. Empty by default — the `deploy` daemon-mediated tool
    /// rejects targets that are not declared here.
    #[serde(default)]
    pub deploys: HashMap<String, DeployConfig>,
}

/// PDX-114 [E2]: per-target deploy config block.
///
/// Declared in `WORKFLOW.md` front matter under the top-level `deploys:`
/// key. The `deploy` daemon-mediated tool consults this map at tool-call
/// time to decide:
///   * whether the target is allowed at all,
///   * which approvers may satisfy the gate,
///   * which environment names are valid for the target,
///   * which Doppler-fronted secrets to inject when invoking the deploy
///     step inside the Cloudflare Workflow runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployConfig {
    /// Deploy kind. Restricted set:
    ///   * `cloudflare_worker` — invokes `wrangler deploy --env <env>`.
    ///   * `npm_publish` — invokes `npm publish`.
    ///   * `cargo_publish` — invokes `cargo publish`.
    ///   * `gh_release` — invokes `gh release create`.
    pub kind: String,
    /// Allowed environment names (e.g. `["staging", "production"]`).
    /// The `deploy` tool refuses an env not in this list.
    #[serde(default)]
    pub environments: Vec<String>,
    /// Linear identifiers (email or name) authorized to satisfy the
    /// approval gate via a `+approve` Linear comment. At least one
    /// approver must be listed; the deploy tool refuses otherwise.
    #[serde(default)]
    pub approvers: Vec<String>,
    /// Doppler-fronted secret references injected when running the
    /// underlying deploy command. Each entry is the env var name as it
    /// will appear when the deploy step runs (e.g.
    /// `"CLOUDFLARE_API_TOKEN"`); the actual value is resolved by the
    /// agent-runtime container against the Doppler project.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Optional approval timeout override (Cloudflare Workflows
    /// `WorkflowTimeoutDuration` string, e.g. `"24 hours"`). Defaults
    /// are set Workflow-side per [`DEFAULT_APPROVAL_TIMEOUT`] in
    /// `cloudflare-control-plane/src/workers/workflows/deploy-workflow.ts`.
    #[serde(default)]
    pub approval_timeout: Option<String>,
}

/// Tracker block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackerConfig {
    /// Tracker kind (`"linear"` is the only supported value in the MVP).
    #[serde(default = "default_tracker_kind")]
    pub kind: String,
    /// GraphQL endpoint URL.
    #[serde(default = "default_linear_endpoint")]
    pub endpoint: String,
    /// Raw `api_key` value as it appeared in YAML; may be a `$VAR` indirection.
    pub api_key: String,
    /// Linear project slug to filter issues by.
    pub project_slug: String,
    /// Team key (e.g. `"PDX"`) used to scope WorkflowState lookups for
    /// state transitions. Optional; required only when
    /// `agent.handoff_state_on_*` is configured.
    #[serde(default)]
    pub team_key: Option<String>,
    /// Issue states considered active for polling.
    #[serde(default = "default_active_states")]
    pub active_states: Vec<String>,
    /// Issue states considered terminal (used for reconciliation; MVP no-op).
    #[serde(default = "default_terminal_states")]
    pub terminal_states: Vec<String>,
}

/// Polling cadence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollingConfig {
    /// Poll interval in milliseconds.
    pub interval_ms: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self { interval_ms: 30_000 }
    }
}

/// Workspace placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Root directory under which per-issue workspaces are created.
    pub root: PathBuf,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: default_workspace_root(),
        }
    }
}

/// Optional shell hooks. All hooks run via `sh -lc` inside the workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksConfig {
    /// Hook executed once on first workspace creation.
    #[serde(default)]
    pub after_create: Option<String>,
    /// Hook executed before each agent run.
    #[serde(default)]
    pub before_run: Option<String>,
    /// Hook executed after each agent run (success or failure).
    #[serde(default)]
    pub after_run: Option<String>,
    /// Per-hook timeout in milliseconds.
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for HooksConfig {
    fn default() -> Self {
        Self {
            after_create: None,
            before_run: None,
            after_run: None,
            timeout_ms: default_hook_timeout_ms(),
        }
    }
}

/// Agent dispatch configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Maximum concurrent in-flight agent runs.
    #[serde(default = "default_max_concurrent_agents")]
    pub max_concurrent_agents: usize,
    /// Maximum allowed diff size (in inserted+deleted lines) before the
    /// run is treated as a guard-rail failure.
    #[serde(default = "default_max_diff_lines")]
    pub max_diff_lines: usize,
    /// Maximum number of conversational turns the agent is allowed to take.
    #[serde(default = "default_max_turns")]
    pub max_turns: usize,
    /// Linear label that must be present on an issue before it is dispatched.
    #[serde(default = "default_agent_label_required")]
    pub agent_label_required: String,
    /// Whether Symphony posts a comment back to the Linear issue when an
    /// agent run completes (success or failure). Default `true`. Set to
    /// `false` to operate purely in audit-log mode without ticket writes.
    #[serde(default = "default_comment_on_completion")]
    pub comment_on_completion: bool,
    /// Optional state name to transition the issue to when the agent run
    /// completes successfully (e.g. `"In Review"`). Requires
    /// `tracker.team_key` to be set. None disables the transition.
    #[serde(default)]
    pub handoff_state_on_success: Option<String>,
    /// Optional state name to transition the issue to on agent failure
    /// (e.g. `"Backlog"`). Requires `tracker.team_key` to be set.
    /// None leaves the issue in its current state for manual triage.
    #[serde(default)]
    pub handoff_state_on_failure: Option<String>,
    /// How long an agent run can go without emitting any event before the
    /// orchestrator considers it stalled and aborts it. Default 5 minutes
    /// per Symphony spec §5.3.6 codex.stall_timeout_ms. 0 disables stall
    /// detection entirely (useful for tests).
    #[serde(default = "default_stall_timeout_ms")]
    pub stall_timeout_ms: u64,
    /// Cap on exponential retry backoff (Symphony §8.4
    /// `agent.max_retry_backoff_ms`). Default 5 minutes.
    #[serde(default = "default_max_retry_backoff_ms")]
    pub max_retry_backoff_ms: u64,
    /// Cap on retry attempts before the orchestrator gives up and leaves
    /// the issue in its current state. Default 3.
    #[serde(default = "default_max_retry_attempts")]
    pub max_retry_attempts: u32,
    /// Rate limit (calls/minute/agent run) on the daemon-mediated
    /// `linear_graphql` tool (Symphony §10.5 / PDX-112). Default 30.
    /// Set `0` to disable the limiter entirely.
    #[serde(default = "default_linear_graphql_rate_per_minute")]
    pub linear_graphql_rate_per_minute: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_concurrent_agents: default_max_concurrent_agents(),
            max_diff_lines: default_max_diff_lines(),
            max_turns: default_max_turns(),
            agent_label_required: default_agent_label_required(),
            comment_on_completion: default_comment_on_completion(),
            handoff_state_on_success: None,
            handoff_state_on_failure: None,
            stall_timeout_ms: default_stall_timeout_ms(),
            max_retry_backoff_ms: default_max_retry_backoff_ms(),
            max_retry_attempts: default_max_retry_attempts(),
            linear_graphql_rate_per_minute: default_linear_graphql_rate_per_minute(),
        }
    }
}

fn default_linear_graphql_rate_per_minute() -> u32 {
    crate::linear_graphql::DEFAULT_RATE_PER_MINUTE
}

fn default_comment_on_completion() -> bool {
    true
}

fn default_stall_timeout_ms() -> u64 {
    300_000
}

fn default_max_retry_backoff_ms() -> u64 {
    300_000
}

fn default_max_retry_attempts() -> u32 {
    3
}

fn default_tracker_kind() -> String {
    "linear".to_string()
}
fn default_linear_endpoint() -> String {
    "https://api.linear.app/graphql".to_string()
}
fn default_active_states() -> Vec<String> {
    vec!["Todo".to_string(), "In Progress".to_string()]
}
fn default_terminal_states() -> Vec<String> {
    vec![
        "Done".to_string(),
        "Cancelled".to_string(),
        "Canceled".to_string(),
        "Closed".to_string(),
    ]
}
fn default_workspace_root() -> PathBuf {
    home_dir().join(".warp/symphony_workspaces")
}
fn default_hook_timeout_ms() -> u64 {
    60_000
}
fn default_max_concurrent_agents() -> usize {
    1
}
fn default_max_diff_lines() -> usize {
    500
}
fn default_max_turns() -> usize {
    5
}
fn default_agent_label_required() -> String {
    "agent:claude".to_string()
}

// ---------------------------------------------------------------------------
// `server` extension — PDX-26 D3 (cron triggers + webhook receiver).
// ---------------------------------------------------------------------------

/// Optional `server` block for cron triggers and the GitHub/Slack/generic
/// webhook receiver. Both sub-blocks are optional and independent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerConfig {
    /// Configured cron jobs (5-field UTC expressions). Empty disables.
    #[serde(default)]
    pub cron_jobs: Vec<CronJobConfig>,
    /// Webhook receiver block. `None` disables the receiver entirely.
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
}

/// One cron-driven trigger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJobConfig {
    /// Job name, unique within the daemon.
    pub name: String,
    /// 5-field cron expression in UTC: `min hour dom mon dow`.
    pub cron: String,
    /// Opaque payload echoed back when the schedule fires; consumed by
    /// downstream handlers (e.g. Linear-issue templates, agent kickoffs).
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Webhook receiver block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Bind address. Defaults to `127.0.0.1:9278`.
    #[serde(default = "default_webhook_bind")]
    pub bind: std::net::SocketAddr,
    /// HMAC secret for `/webhook/github`. Empty disables the route.
    #[serde(default)]
    pub github_secret: String,
    /// HMAC secret for `/webhook/slack`. Empty disables the route.
    #[serde(default)]
    pub slack_secret: String,
    /// HMAC secret for `/webhook/generic`. Empty disables the route.
    #[serde(default)]
    pub generic_secret: String,
}

fn default_webhook_bind() -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 0, 0, 1], 9278))
}

/// Errors raised by the workflow loader.
#[derive(Debug, Error)]
pub enum WorkflowError {
    /// Failure reading the workflow file from disk.
    #[error("io error reading workflow: {0}")]
    Io(#[from] std::io::Error),
    /// Front-matter YAML parse failure.
    #[error("invalid YAML front matter: {0}")]
    Yaml(#[from] serde_yaml::Error),
    /// Required `$VAR` env indirection was unset in the environment.
    #[error("environment variable {0} referenced from workflow is not set")]
    MissingEnvVar(String),
    /// Liquid template parse / render failure.
    #[error("liquid template error: {0}")]
    Liquid(String),
    /// `tracker.kind` is not `"linear"` (only Linear is supported in the MVP).
    #[error("unsupported tracker.kind: {0} (only \"linear\" is supported)")]
    UnsupportedTracker(String),
}

impl WorkflowDefinition {
    /// Load and parse a `WORKFLOW.md` file from disk.
    pub fn load(path: &std::path::Path) -> Result<Self, WorkflowError> {
        let raw = std::fs::read_to_string(path)?;
        Self::from_str(&raw)
    }

    /// Parse a workflow file from a string.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(raw: &str) -> Result<Self, WorkflowError> {
        let (front, body) = split_front_matter(raw);

        let mut config: WorkflowConfig = if front.trim().is_empty() {
            // No front matter — caller still needs at least a stub config to
            // run, but since there's no api_key/project_slug it'll fail
            // tracker construction. We surface this through serde so the
            // error message is uniform.
            serde_yaml::from_str("tracker:\n  api_key: \"\"\n  project_slug: \"\"\n")?
        } else {
            serde_yaml::from_str(front)?
        };

        if config.tracker.kind != "linear" {
            return Err(WorkflowError::UnsupportedTracker(config.tracker.kind));
        }

        // Resolve $VAR indirection on string fields that commonly hold
        // secrets or paths.
        config.tracker.api_key = resolve_env_indirection(&config.tracker.api_key)?;
        let workspace_str = config.workspace.root.to_string_lossy().to_string();
        let resolved_root = resolve_env_indirection(&workspace_str)?;
        config.workspace.root = expand_tilde(&resolved_root);

        // Validate the template parses now so we fail fast at startup,
        // even though we render later.
        let _parser = liquid::ParserBuilder::with_stdlib()
            .build()
            .map_err(|e| WorkflowError::Liquid(e.to_string()))?;
        let _template = _parser
            .parse(body)
            .map_err(|e| WorkflowError::Liquid(e.to_string()))?;

        Ok(Self {
            config,
            prompt_template: body.to_string(),
        })
    }

    /// Render the prompt template against an issue and an optional attempt
    /// number. Strict-undefined: any reference to a missing variable fails.
    pub fn render_prompt(
        &self,
        issue: &Issue,
        attempt: Option<u32>,
    ) -> Result<String, WorkflowError> {
        let parser = liquid::ParserBuilder::with_stdlib()
            .build()
            .map_err(|e| WorkflowError::Liquid(e.to_string()))?;
        let template = parser
            .parse(&self.prompt_template)
            .map_err(|e| WorkflowError::Liquid(e.to_string()))?;

        let mut globals = liquid::Object::new();
        globals.insert("issue".into(), issue_to_liquid(issue));
        globals.insert(
            "attempt".into(),
            match attempt {
                Some(n) => liquid::model::Value::scalar(n as i64),
                None => liquid::model::Value::Nil,
            },
        );

        template
            .render(&globals)
            .map_err(|e| WorkflowError::Liquid(e.to_string()))
    }
}

fn issue_to_liquid(issue: &Issue) -> liquid::model::Value {
    let mut obj = liquid::Object::new();
    obj.insert("id".into(), liquid::model::Value::scalar(issue.id.clone()));
    obj.insert(
        "identifier".into(),
        liquid::model::Value::scalar(issue.identifier.clone()),
    );
    obj.insert(
        "title".into(),
        liquid::model::Value::scalar(issue.title.clone()),
    );
    obj.insert(
        "description".into(),
        match &issue.description {
            Some(d) => liquid::model::Value::scalar(d.clone()),
            None => liquid::model::Value::Nil,
        },
    );
    obj.insert(
        "priority".into(),
        match issue.priority {
            Some(p) => liquid::model::Value::scalar(p as i64),
            None => liquid::model::Value::Nil,
        },
    );
    obj.insert(
        "state".into(),
        liquid::model::Value::scalar(issue.state.clone()),
    );
    obj.insert(
        "url".into(),
        match &issue.url {
            Some(u) => liquid::model::Value::scalar(u.clone()),
            None => liquid::model::Value::Nil,
        },
    );
    let labels: Vec<liquid::model::Value> = issue
        .labels
        .iter()
        .map(|l| liquid::model::Value::scalar(l.clone()))
        .collect();
    obj.insert("labels".into(), liquid::model::Value::array(labels));
    liquid::model::Value::Object(obj)
}

/// Split a workflow file into `(front_matter_yaml, body)`.
///
/// Recognised forms:
///   * `---\nYAML\n---\nbody` → returns `(YAML, body)`.
///   * Anything else → returns `("", whole_file)`.
fn split_front_matter(raw: &str) -> (&str, &str) {
    let trimmed = raw.trim_start_matches('\u{FEFF}'); // strip BOM if any
    if !trimmed.starts_with("---") {
        return ("", raw);
    }
    // Find the closing `---` after the opening one.
    let after_open = &trimmed[3..];
    // Skip newline immediately after the opening fence.
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    if let Some(end) = after_open.find("\n---") {
        let yaml = &after_open[..end];
        let rest = &after_open[end + 4..]; // skip "\n---"
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        (yaml, rest)
    } else {
        ("", raw)
    }
}

/// If `value` is exactly `$VAR_NAME` (single var), look up `VAR_NAME` in
/// the environment and substitute. Otherwise return `value` unchanged.
fn resolve_env_indirection(value: &str) -> Result<String, WorkflowError> {
    if let Some(name) = value.strip_prefix('$') {
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return std::env::var(name).map_err(|_| WorkflowError::MissingEnvVar(name.to_string()));
        }
    }
    Ok(value.to_string())
}

fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        home_dir().join(rest)
    } else if p == "~" {
        home_dir()
    } else {
        PathBuf::from(p)
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

// Suppress unused-import warning when unused.
#[allow(dead_code)]
fn _unused_hashmap(_: HashMap<String, String>) {}
