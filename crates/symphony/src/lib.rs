//! Symphony — Linear-driven coding agent orchestrator (Helm MVP).
//!
//! Polls a Linear-compatible tracker, claims active issues, materializes
//! per-issue workspaces, dispatches the work to a registered
//! `orchestrator::Agent`, and streams events back into an audit log.
//!
//! Stall detection, retry/backoff, reconciliation, and the daemon-mediated
//! `linear_graphql` tool (PDX-112 §10.5) are wired in as additional layers
//! on top of the dispatch core. See `docs/symphony/README.md` for the
//! divergences from the upstream Symphony spec.

#![deny(missing_docs)]

pub mod approval_poller;
pub mod audit;
pub mod deploy_tool;
pub mod diff_guard;
pub mod linear_graphql;
pub mod numstat;
pub mod orchestrator;
pub mod reload;
pub mod simulator_tool;
pub mod tracker;
pub mod triggers;
pub mod workflow;
pub mod workspace;

pub use approval_poller::{
    ApprovalComment, ApprovalPoller, ApprovalSink, CommentSource, HttpApprovalSink, PollOutcome,
    PollerState, DEFAULT_APPROVAL_TOKEN,
};
pub use audit::{AuditEvent, AuditLog};
pub use deploy_tool::{
    DeployConfigResolver, DeployTool, DeployToolError, DeployWorkflowClient,
    DeployWorkflowParams, HttpDeployWorkflowClient, TOOL_NAME as DEPLOY_TOOL_NAME,
};
pub use diff_guard::{DiffGuard, DiffGuardError, DiffStat};
pub use linear_graphql::{LinearGraphQlExecutor, LinearGraphQlTool, DEFAULT_RATE_PER_MINUTE, TOOL_NAME};
pub use orchestrator::{Orchestrator, OrchestratorError};
pub use reload::{apply_reload, WatchError, WorkflowHandle, WorkflowWatcher};
pub use simulator_tool::{
    SimulatorExecutor, SimulatorOp, SimulatorTool, SimulatorToolError,
    XcrunSimulatorExecutor, TOOL_NAME as SIMULATOR_TOOL_NAME,
};
pub use tracker::{BlockerRef, Issue, LinearClient, TrackerError};
pub use triggers::{spawn_triggers, TriggerError, TriggerSurfaces};
pub use workflow::{
    AgentConfig, CronJobConfig, DeployConfig, HooksConfig, PollingConfig, ServerConfig,
    TrackerConfig, WebhookConfig, WorkflowConfig, WorkflowDefinition, WorkflowError,
    WorkspaceConfig,
};
pub use workspace::{Workspace, WorkspaceError, WorkspaceManager};
