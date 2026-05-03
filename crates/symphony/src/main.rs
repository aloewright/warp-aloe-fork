//! Symphony daemon entry point.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Output;
use std::sync::Arc;

use agents::{ClaudeCodeAgent, ClaudeModel};
use clap::Parser;
use doppler::{CommandRunner, DopplerClient, DopplerError, SecretValue, DEFAULT_TTL};
use orchestrator::{AgentRegistration, Budget, Cap, Provider, Router};
use symphony::audit::AuditLog;
use symphony::orchestrator::{IssueSource, Orchestrator};
use symphony::linear_graphql::LinearGraphQlTool;
use symphony::reload::{WorkflowHandle, WorkflowWatcher};
use symphony::tracker::LinearClient;
use symphony::workflow::WorkflowDefinition;
use symphony::workspace::WorkspaceManager;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "symphony", about = "Linear-driven coding-agent orchestrator")]
struct Cli {
    /// Path to the WORKFLOW.md file.
    #[arg(long, default_value = "./WORKFLOW.md")]
    workflow: PathBuf,
    /// Run a single tick and exit (smoke testing).
    #[arg(long, default_value_t = false)]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // rustls 0.23 requires an explicit crypto provider before any TLS client
    // is constructed (reqwest's rustls-tls in this workspace doesn't install
    // one automatically). Idempotent: ignore the error if a provider is
    // already installed (e.g. by another crate in the same process).
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let workflow = WorkflowDefinition::load(&cli.workflow)?;
    // PDX-111: wrap the parsed definition in a swap-able handle so the live
    // reload watcher and the orchestrator share the same authoritative
    // pointer. All static-at-startup wiring below (workspace root, tracker
    // endpoint, hooks) reads the *initial* snapshot — those surfaces are
    // not hot-reloadable on purpose; only `polling`, `agent.*`, and
    // `tracker.active_states`/labels actually take effect on subsequent
    // ticks. Mutating `workspace.root` is rejected by the reload watcher.
    let workflow_handle = Arc::new(WorkflowHandle::new(workflow));
    let initial = workflow_handle.load();

    let api_key = resolve_api_key(&initial.config.tracker.api_key).await?;

    let tracker = match &initial.config.tracker.team_key {
        Some(team_key) => LinearClient::new_with_team(
            initial.config.tracker.endpoint.clone(),
            api_key,
            initial.config.tracker.project_slug.clone(),
            team_key.clone(),
        )?,
        None => LinearClient::new(
            initial.config.tracker.endpoint.clone(),
            api_key,
            initial.config.tracker.project_slug.clone(),
        )?,
    };

    let workspaces = Arc::new(WorkspaceManager::new(
        initial.config.workspace.root.clone(),
        initial.config.hooks.clone(),
    ));

    let mut caps: HashMap<Provider, Cap> = HashMap::new();
    // Generous defaults for the MVP: $100/mo, $20/session per provider.
    let cap = Cap {
        monthly_micro_dollars: 100_000_000,
        session_micro_dollars: 20_000_000,
    };
    caps.insert(Provider::ClaudeCode, cap);
    let budget = Arc::new(Budget::new(caps));
    let mut router = Router::new(Arc::clone(&budget));

    let claude = ClaudeCodeAgent::new(
        orchestrator::AgentId("claude-sonnet-46".to_string()),
        ClaudeModel::Sonnet46,
    )?;
    router.register(AgentRegistration {
        agent: Arc::new(claude),
        provider: Provider::ClaudeCode,
        estimated_micros_per_task: 50_000,
    });
    let router = Arc::new(router);

    let audit_path = home_dir().join(".warp/symphony/audit.log");
    let audit = Arc::new(AuditLog::open(audit_path));

    // Spin up the optional `server` extension surfaces (cron triggers +
    // GitHub/Slack/generic webhook receiver) before the orchestrator
    // takes over the main task. These run in their own tokio tasks and
    // never block the orchestrator. PDX-26 D3.
    let trigger_surfaces = symphony::spawn_triggers(&initial.config.server, audit.clone()).await;
    if let Err(e) = &trigger_surfaces {
        tracing::warn!(error = %e, "trigger surfaces failed to start; continuing without them");
    }
    if let Ok(s) = &trigger_surfaces {
        if let Some(addr) = s.webhook_bind {
            tracing::info!(%addr, "symphony: webhook receiver bound");
        }
        if s.cron_handle.is_some() {
            tracing::info!("symphony: cron scheduler running");
        }
    }
    // Move the surfaces into a guard so they aren't dropped (which would
    // abort the JoinHandles). Even when shutdown happens via Ctrl-C, the
    // tokio runtime tear-down handles cleanup.
    let _trigger_guard = trigger_surfaces.ok();

    // Wrap the tracker in an Arc shared between the IssueSource role
    // (candidate fetch + comments + state transitions) and the
    // `linear_graphql` daemon-mediated tool (PDX-112 §10.5). The tool
    // executes GraphQL on the agent's behalf without ever exposing the
    // API token to the subprocess.
    let tracker_arc = Arc::new(tracker);
    let rate_per_minute = initial.config.agent.linear_graphql_rate_per_minute;
    let linear_graphql_tool = LinearGraphQlTool::with_rate(
        Arc::clone(&tracker_arc) as Arc<dyn symphony::linear_graphql::LinearGraphQlExecutor>,
        rate_per_minute,
    );
    // Drop the local snapshot ref so the watcher and orchestrator share the
    // handle as the only authoritative pointer.
    drop(initial);

    let orch = Arc::new(
        Orchestrator::with_handle(
            Arc::clone(&workflow_handle),
            tracker_arc as Arc<dyn IssueSource>,
            workspaces,
            router,
            audit.clone(),
        )
        .with_linear_graphql_tool(linear_graphql_tool),
    );

    // PDX-111: kick off the live-reload watcher. Failure to start the
    // watcher is logged but non-fatal — the daemon still runs against the
    // initial config.
    let _reload_guard = match WorkflowWatcher::start(
        cli.workflow.clone(),
        Arc::clone(&workflow_handle),
        audit.clone(),
    ) {
        Ok(w) => Some(w),
        Err(e) => {
            tracing::warn!(error = %e, "symphony: live workflow reload disabled (watcher failed to start)");
            None
        }
    };

    if cli.once {
        orch.tick().await?;
        // Wait indefinitely for spawned agent tasks to drain. Real coding
        // tasks against the warp source can take 10-30 minutes; capping the
        // drain causes Symphony to exit before agents finish, which means
        // diff-stat / comment / state-transition never run. Use Ctrl-C to
        // abort if needed.
        loop {
            let (running, _completed) = orch.state_snapshot().await;
            if running.is_empty() {
                tracing::info!("once: all dispatched agents finished");
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        return Ok(());
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            tracing::info!("ctrl-c received");
            let _ = shutdown_tx.send(());
        }
    });

    Arc::clone(&orch).run(shutdown_rx).await;
    Ok(())
}

/// Resolve the Linear API key into a [`SecretValue`].
///
/// The workflow loader has already substituted `$VAR` indirection. If the
/// resulting string is non-empty, we wrap it through a one-shot stub
/// [`CommandRunner`] (the only way to construct a `SecretValue` outside the
/// `doppler` crate without modifying its public surface). If the string is
/// empty, we fall back to the real Doppler CLI.
async fn resolve_api_key(spec: &str) -> Result<SecretValue, Box<dyn std::error::Error>> {
    if !spec.is_empty() {
        let runner = Arc::new(LiteralRunner {
            value: spec.to_string(),
        });
        let client = DopplerClient::with_runner(DEFAULT_TTL, runner);
        return Ok(client.get("LINEAR_API_KEY", None).await?);
    }

    match doppler::detect() {
        Ok(_) => {
            let client = DopplerClient::new(DEFAULT_TTL);
            let v = client.get("LINEAR_API_KEY", None).await?;
            Ok(v)
        }
        Err(DopplerError::NotInstalled { install_hint }) => Err(format!(
            "LINEAR_API_KEY is not set in env and Doppler is not installed ({install_hint})"
        )
        .into()),
        Err(e) => Err(Box::new(e)),
    }
}

/// One-shot [`CommandRunner`] that returns a fixed string as the secret.
struct LiteralRunner {
    value: String,
}

#[async_trait::async_trait]
impl CommandRunner for LiteralRunner {
    async fn run(
        &self,
        _args: &[&str],
        _cwd: Option<&std::path::Path>,
    ) -> std::io::Result<Output> {
        use std::os::unix::process::ExitStatusExt;
        Ok(Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: self.value.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}
