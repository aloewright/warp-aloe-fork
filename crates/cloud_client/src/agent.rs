//! `orchestrator::Agent` impl for the cloud backend.
//!
//! Each call to [`CloudAgent::execute`] spawns a driver task that:
//!
//! 1. Connects to the configured WebSocket URL via the supplied transport
//!    factory (production = `tokio-tungstenite`; tests = in-memory pair).
//! 2. Sends a [`cloud_protocol::TaskSubmit`] for the task.
//! 3. Streams [`cloud_protocol::TaskEvent`]s back, translates them to
//!    [`orchestrator::AgentEvent`]s, and pushes them into a channel that
//!    the returned `AgentEventStream` drains.
//! 4. On unexpected disconnect, applies the [`crate::ReconnectPolicy`],
//!    flips the public health flag to `unhealthy`, reconnects, and sends
//!    a [`crate::session::resume_message`] so the server can replay
//!    events from `last_sequence + 1`.
//! 5. Stops on the first terminal `TaskResult` (Success / Failed /
//!    Cancelled) or on policy exhaustion.

use std::collections::HashSet;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use chrono::Utc;
use cloud_protocol::{
    Envelope, Message, OutputStream, TaskEvent, TaskEventKind, TaskResult, TaskSubmit, V1Message,
};
use orchestrator::{
    Agent, AgentError, AgentEvent, AgentEventStream, AgentId, Capabilities, Health, Role, Task,
    TaskId,
};
use serde_json::json;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use crate::error::ConnectError;
use crate::reconnect::{ReconnectDecision, ReconnectPolicy};
use crate::session::{resume_message, SessionState};
use crate::transport::{Transport, TransportError, TungsteniteTransport};

/// Context window the production cloud tier is expected to expose.
/// Mirrors the value advertised by the (now-superseded) stub
/// `agents::RemoteAgent` so the router sees consistent capabilities.
pub const CLOUD_CONTEXT_TOKENS: u32 = 200_000;

/// Boxed async factory for opening a fresh [`Transport`].
///
/// Returns `Err(ConnectError)` if the connect attempt itself fails (so
/// the reconnect policy can apply); a returned transport is assumed
/// usable until its `recv` produces `Closed`.
pub type TransportFactory = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn Transport>, ConnectError>> + Send>,
        > + Send
        + Sync,
>;

/// Config for a [`CloudAgent`].
#[derive(Clone)]
pub struct CloudAgentConfig {
    /// Stable agent id surfaced to the orchestrator.
    pub id: AgentId,
    /// Reconnect / backoff policy.
    pub reconnect: ReconnectPolicy,
    /// Factory that opens a fresh [`Transport`] each connect attempt.
    pub transport_factory: TransportFactory,
}

impl CloudAgentConfig {
    /// Convenience constructor for the production case: connect to the
    /// given `wss://…` URL using `tokio-tungstenite`.
    pub fn websocket(id: AgentId, url: impl Into<String>, reconnect: ReconnectPolicy) -> Self {
        let url: String = url.into();
        let transport_factory: TransportFactory = Arc::new(move || {
            let url = url.clone();
            Box::pin(async move {
                let t = TungsteniteTransport::connect(&url).await?;
                let boxed: Box<dyn Transport> = Box::new(t);
                Ok(boxed)
            })
        });
        Self {
            id,
            reconnect,
            transport_factory,
        }
    }
}

/// `orchestrator::Agent` backed by a WebSocket cloud worker.
pub struct CloudAgent {
    id: AgentId,
    capabilities: Capabilities,
    health: Arc<Mutex<Health>>,
    config: CloudAgentConfig,
}

impl CloudAgent {
    /// Construct a new agent. The transport is opened lazily on the
    /// first call to [`Agent::execute`], so this is infallible.
    pub fn new(config: CloudAgentConfig) -> Self {
        let roles: HashSet<Role> = [
            Role::Planner,
            Role::Reviewer,
            Role::Worker,
            Role::BulkRefactor,
            Role::Summarize,
            Role::ToolRouter,
            Role::Inline,
        ]
        .into_iter()
        .collect();
        let capabilities = Capabilities {
            roles,
            max_context_tokens: CLOUD_CONTEXT_TOKENS,
            supports_tools: true,
            supports_vision: true,
        };
        // We start optimistic; the driver will flip us unhealthy if
        // reconnect is exhausted.
        let health = Arc::new(Mutex::new(Health {
            healthy: true,
            last_check: Utc::now(),
            error_rate: 0.0,
        }));
        Self {
            id: config.id.clone(),
            capabilities,
            health,
            config,
        }
    }

    async fn set_healthy(health: &Arc<Mutex<Health>>, healthy: bool) {
        let mut g = health.lock().await;
        g.healthy = healthy;
        g.last_check = Utc::now();
    }
}

#[async_trait]
impl Agent for CloudAgent {
    fn id(&self) -> AgentId {
        self.id.clone()
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn health(&self) -> Health {
        if let Ok(g) = self.health.try_lock() {
            g.clone()
        } else {
            // Conservative: if we can't read it, report best-effort fresh.
            Health {
                healthy: true,
                last_check: Utc::now(),
                error_rate: 0.0,
            }
        }
    }

    async fn execute(&self, task: Task) -> Result<AgentEventStream, AgentError> {
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let task_id = task.id;
        let cloud_task_id = format!("{}", task_id.0);
        let role = task.role;
        let prompt = task.prompt.clone();
        let cwd = task
            .context
            .cwd
            .to_string_lossy()
            .to_string();
        let factory = self.config.transport_factory.clone();
        let policy = self.config.reconnect.clone();
        let health = self.health.clone();

        tokio::spawn(async move {
            run_session(
                task_id,
                cloud_task_id,
                role,
                prompt,
                cwd,
                factory,
                policy,
                health,
                tx,
            )
            .await;
        });

        let s = stream! {
            while let Some(ev) = rx.recv().await {
                yield ev;
            }
        };
        Ok(Box::pin(s))
    }
}

/// Driver loop. Owns the session state and the reconnect policy, fans
/// `AgentEvent`s into `tx`, and exits as soon as it observes a terminal
/// `TaskResult` or runs out of reconnect budget.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    task_id: TaskId,
    cloud_task_id: String,
    role: Role,
    prompt: String,
    cwd: String,
    factory: TransportFactory,
    policy: ReconnectPolicy,
    health: Arc<Mutex<Health>>,
    tx: mpsc::Sender<AgentEvent>,
) {
    let mut session = SessionState::new();
    session.mark_submitted(&cloud_task_id);
    let mut started_emitted = false;
    let mut attempt: u32 = 0;
    let mut submitted = false;

    loop {
        // (Re)connect.
        let transport = match (factory)().await {
            Ok(t) => {
                CloudAgent::set_healthy(&health, true).await;
                attempt = 0;
                t
            }
            Err(err) => {
                warn!(?err, "cloud_client connect failed");
                CloudAgent::set_healthy(&health, false).await;
                attempt = attempt.saturating_add(1);
                match policy.next_decision(attempt) {
                    ReconnectDecision::Retry { delay, .. } => {
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    ReconnectDecision::GiveUp => {
                        let _ = tx
                            .send(AgentEvent::Failed {
                                task_id,
                                error: format!(
                                    "cloud_client: reconnect exhausted: {err}"
                                ),
                            })
                            .await;
                        return;
                    }
                }
            }
        };

        // Drive one connection. If it terminates without a final task
        // result, the loop tries again per the reconnect policy.
        let outcome = drive_connection(
            transport,
            &cloud_task_id,
            role,
            &prompt,
            &cwd,
            &mut session,
            &mut started_emitted,
            &mut submitted,
            task_id,
            &tx,
        )
        .await;

        match outcome {
            ConnectionOutcome::Terminal => {
                // Task ended; we're done.
                return;
            }
            ConnectionOutcome::Disconnected => {
                CloudAgent::set_healthy(&health, false).await;
                attempt = attempt.saturating_add(1);
                match policy.next_decision(attempt) {
                    ReconnectDecision::Retry { delay, .. } => {
                        debug!(?delay, attempt, "cloud_client reconnecting");
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    ReconnectDecision::GiveUp => {
                        let _ = tx
                            .send(AgentEvent::Failed {
                                task_id,
                                error:
                                    "cloud_client: reconnect attempts exhausted after disconnect"
                                        .to_string(),
                            })
                            .await;
                        return;
                    }
                }
            }
        }
    }
}

enum ConnectionOutcome {
    /// We saw a terminal `TaskResult` — the driver should exit.
    Terminal,
    /// The transport closed before any terminal event — let the policy
    /// decide whether to reconnect.
    Disconnected,
}

#[allow(clippy::too_many_arguments)]
async fn drive_connection(
    mut transport: Box<dyn Transport>,
    task_id_str: &str,
    role: Role,
    prompt: &str,
    cwd: &str,
    session: &mut SessionState,
    started_emitted: &mut bool,
    submitted: &mut bool,
    task_id: TaskId,
    tx: &mpsc::Sender<AgentEvent>,
) -> ConnectionOutcome {
    // Resume hints for any in-flight tasks, sent before re-submitting.
    if *submitted {
        for hint in session.resume_hints() {
            let env = resume_message(&hint);
            if transport.send(env).await.is_err() {
                return ConnectionOutcome::Disconnected;
            }
        }
    } else {
        // First connection: submit the task.
        let submit = TaskSubmit {
            task_id: task_id_str.to_string(),
            kind: "agent".to_string(),
            payload: json!({
                "role": format!("{role:?}"),
                "prompt": prompt,
                "cwd": cwd,
            }),
            metadata: Default::default(),
        };
        let env = Envelope::v1(V1Message::TaskSubmit(submit));
        if transport.send(env).await.is_err() {
            return ConnectionOutcome::Disconnected;
        }
        *submitted = true;
    }

    // Read events until terminal or disconnect.
    loop {
        let env_opt = match transport.recv().await {
            Ok(opt) => opt,
            Err(TransportError::Closed) => return ConnectionOutcome::Disconnected,
            Err(e) => {
                warn!(?e, "cloud_client recv error");
                return ConnectionOutcome::Disconnected;
            }
        };
        let env = match env_opt {
            Some(e) => e,
            None => return ConnectionOutcome::Disconnected,
        };
        let Message::V1(v1) = env.message;
        let evt = match v1 {
            V1Message::TaskEvent(e) => e,
            _ => continue,
        };
        if evt.task_id != task_id_str {
            continue;
        }
        if session.is_duplicate(&evt.task_id, evt.sequence) {
            continue;
        }
        session.observe(&evt.task_id, evt.sequence);

        if !*started_emitted {
            *started_emitted = true;
            if tx.send(AgentEvent::Started { task_id }).await.is_err() {
                return ConnectionOutcome::Terminal; // consumer hung up
            }
        }

        match translate(task_id, evt) {
            TranslatedEvent::Forward(ev) => {
                if tx.send(ev).await.is_err() {
                    return ConnectionOutcome::Terminal;
                }
            }
            TranslatedEvent::Terminal(ev) => {
                let _ = tx.send(ev).await;
                session.complete(task_id_str);
                return ConnectionOutcome::Terminal;
            }
            TranslatedEvent::Drop => {}
        }
    }
}

enum TranslatedEvent {
    /// Forward a single non-terminal `AgentEvent`.
    Forward(AgentEvent),
    /// Forward a terminal `AgentEvent` (Completed/Failed); the driver
    /// must exit after sending it.
    Terminal(AgentEvent),
    /// Nothing of interest (e.g. Progress with no ergonomic mapping).
    Drop,
}

fn translate(task_id: TaskId, evt: TaskEvent) -> TranslatedEvent {
    match evt.kind {
        TaskEventKind::StatusChanged { .. } => TranslatedEvent::Drop,
        TaskEventKind::Progress { .. } => TranslatedEvent::Drop,
        TaskEventKind::Output { stream, data } => {
            // Map every output stream onto an OutputChunk; the prefix
            // disambiguates stderr/log/agent if the consumer cares.
            let text = match stream {
                OutputStream::Stdout | OutputStream::Agent => data,
                OutputStream::Stderr => format!("[stderr] {data}"),
                OutputStream::Log => format!("[log] {data}"),
            };
            TranslatedEvent::Forward(AgentEvent::OutputChunk { text })
        }
        TaskEventKind::Completed { result } => match result {
            TaskResult::Success { output } => {
                let summary = output.and_then(|v| match v {
                    serde_json::Value::String(s) => Some(s),
                    other => Some(other.to_string()),
                });
                TranslatedEvent::Terminal(AgentEvent::Completed { task_id, summary })
            }
            TaskResult::Error {
                code,
                message,
                details: _,
            } => TranslatedEvent::Terminal(AgentEvent::Failed {
                task_id,
                error: format!("{code}: {message}"),
            }),
            TaskResult::Cancelled => TranslatedEvent::Terminal(AgentEvent::Failed {
                task_id,
                error: "cancelled".to_string(),
            }),
        },
    }
}
