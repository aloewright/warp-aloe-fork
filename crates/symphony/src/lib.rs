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

pub mod audit;
pub mod diff_guard;
pub mod linear_graphql;
pub mod numstat;
pub mod orchestrator;
pub mod tracker;
pub mod triggers;
pub mod workflow;
pub mod workspace;

pub use audit::{AuditEvent, AuditLog};
pub use diff_guard::{DiffGuard, DiffGuardError, DiffStat};
pub use linear_graphql::{LinearGraphQlExecutor, LinearGraphQlTool, DEFAULT_RATE_PER_MINUTE, TOOL_NAME};
pub use orchestrator::{Orchestrator, OrchestratorError};
pub use tracker::{BlockerRef, Issue, LinearClient, TrackerError};
pub use triggers::{spawn_triggers, TriggerError, TriggerSurfaces};
pub use workflow::{
    AgentConfig, CronJobConfig, HooksConfig, PollingConfig, ServerConfig, TrackerConfig,
    WebhookConfig, WorkflowConfig, WorkflowDefinition, WorkflowError, WorkspaceConfig,
};
pub use workspace::{Workspace, WorkspaceError, WorkspaceManager};
