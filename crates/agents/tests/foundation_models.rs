//! Integration tests for the [`agents::FoundationModelsAgent`] (PDX-16).
//!
//! These tests exercise the public surface only; the underlying Swift
//! bridge is stubbed at compile time on non-macOS targets, so health
//! checks and `execute()` are deterministic. On macOS the bridge probes
//! the live runtime, so we keep assertions tolerant of either outcome.

use agents::FoundationModelsAgent;
use orchestrator::{Agent, AgentId, Role};

#[cfg(not(target_os = "macos"))]
use orchestrator::{AgentEvent, Task, TaskContext, TaskId};

#[cfg(not(target_os = "macos"))]
use futures_util::StreamExt;

#[test]
fn new_succeeds_unconditionally() {
    // Construction probes `is_supported` once but never panics; should
    // succeed on every platform regardless of the runtime's availability.
    let _ = FoundationModelsAgent::new(AgentId("fm".into()));
    let _ = FoundationModelsAgent::new(AgentId("another".into()));
}

/// On non-macOS targets the bridge is a compile-time stub and reports
/// `is_supported() == false`, so the agent must surface `healthy = false`
/// and the router will skip it on every dispatch.
#[cfg(not(target_os = "macos"))]
#[test]
fn health_is_unhealthy_off_mac() {
    let agent = FoundationModelsAgent::new(AgentId("fm".into()));
    let h = agent.health();
    assert!(!h.healthy);
    assert_eq!(h.error_rate, 0.0);
}

/// Off-Mac, `execute` must yield `Started` then `Failed` so the
/// dispatcher can fall through to a Haiku 4.5 fallback (PDX-16
/// acceptance criterion 3). On Mac the outcome depends on the live
/// bridge, so this assertion is gated.
#[cfg(not(target_os = "macos"))]
#[tokio::test]
async fn execute_returns_failed_off_mac() {
    let agent = FoundationModelsAgent::new(AgentId("fm".into()));
    let task = Task {
        id: TaskId::new(),
        role: Role::Inline,
        prompt: "hi".to_string(),
        context: TaskContext::default(),
        budget_hint: None,
    };
    let mut stream = agent.execute(task).await.expect("execute returns stream");
    // First event must be Started.
    match stream.next().await {
        Some(AgentEvent::Started { .. }) => {}
        other => panic!("expected Started, got {other:?}"),
    }
    // Second event is Failed because the runtime is unavailable.
    match stream.next().await {
        Some(AgentEvent::Failed { error, .. }) => {
            assert!(
                error.contains("foundation_models"),
                "unexpected error: {error}"
            );
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// PDX-16 acceptance: capability advertisements match the issue spec
/// exactly. Adding `Worker` here would mean FM bids on tasks that need
/// far more than its 4 K context window.
#[test]
fn capabilities_match_pdx16_spec() {
    let agent = FoundationModelsAgent::new(AgentId("fm".into()));
    let caps = agent.capabilities();
    assert!(caps.roles.contains(&Role::Inline));
    assert!(caps.roles.contains(&Role::ToolRouter));
    assert!(caps.roles.contains(&Role::Summarize));
    // Foundation Models is a tiny on-device tier; nothing else.
    assert!(!caps.roles.contains(&Role::Planner));
    assert!(!caps.roles.contains(&Role::Reviewer));
    assert!(!caps.roles.contains(&Role::Worker));
    assert!(!caps.roles.contains(&Role::BulkRefactor));

    assert!(!caps.supports_tools);
    assert!(!caps.supports_vision);
    assert_eq!(caps.max_context_tokens, 4_096);
}

#[test]
fn id_round_trips() {
    let agent = FoundationModelsAgent::new(AgentId("fm-7".into()));
    assert_eq!(agent.id(), AgentId("fm-7".to_string()));
}

/// `is_supported` is exposed so the router-side registration helper can
/// short-circuit registration on hosts without the runtime; the call
/// must not panic on any target.
#[test]
fn is_supported_does_not_panic() {
    let _ = FoundationModelsAgent::is_supported();
}
