//! Symphony trigger surfaces (PDX-26 D3).
//!
//! Two side-channels into the orchestrator that don't require new
//! reconciliation state:
//!
//! 1. **Cron triggers** — local cron-like scheduler driven by
//!    [`cron_scheduler`]. Fires [`cron_scheduler::ScheduledTaskTriggered`]
//!    events on a tokio mpsc channel.
//! 2. **GitHub / Slack / generic webhook receiver** — HMAC-validated
//!    Axum HTTP server from [`github_webhook_receiver`] that emits
//!    [`github_webhook_receiver::WebhookEvent`]s on a separate channel.
//!
//! Both event streams are funneled into the audit log so operators can
//! confirm fan-out without standing up a real Linear ticket. The
//! create-or-update-Linear-issue translation lives in a follow-up wire-up
//! that lands once the receiver is exercised end-to-end (see PDX-25
//! Workflows for the consumer side).

use std::sync::Arc;

use crate::audit::{AuditEvent, AuditEventKind, AuditLog};
use crate::workflow::ServerConfig;
use cron_scheduler::{
    CronError, CronJob, CronScheduler, CronSchedulerConfig, ScheduledTaskTriggered,
};
use github_webhook_receiver::{ReceivedEvent, ReceiverConfig, ReceiverError, WebhookEvent};
use tokio::sync::mpsc;

/// Aggregate of trigger-related background tasks Symphony spawned at boot.
/// Kept around so callers can join them on shutdown if desired.
#[allow(dead_code)]
pub struct TriggerSurfaces {
    /// JoinHandle of the cron driver loop, if jobs were configured.
    pub cron_handle: Option<tokio::task::JoinHandle<()>>,
    /// Address the webhook server bound to (only set if `server` config
    /// supplied a non-empty bind).
    pub webhook_bind: Option<std::net::SocketAddr>,
    /// JoinHandle of the webhook server.
    pub webhook_handle: Option<tokio::task::JoinHandle<()>>,
    /// JoinHandle of the audit fan-out loop forwarding both streams into
    /// the audit log.
    pub fanout_handle: tokio::task::JoinHandle<()>,
}

/// Errors raised while spinning up the trigger surfaces.
#[derive(Debug, thiserror::Error)]
pub enum TriggerError {
    /// Bad cron expression in the `cron.jobs` config.
    #[error("cron config: {0}")]
    Cron(#[from] CronError),
    /// Webhook receiver bind failed.
    #[error("webhook receiver: {0}")]
    Webhook(#[from] ReceiverError),
}

/// Spawn the cron scheduler + webhook receiver based on the
/// [`ServerConfig`] in the workflow front matter. Returns a
/// [`TriggerSurfaces`] handle plus channel receivers if callers want to
/// pre-empt the audit-log fan-out (tests).
///
/// `audit` is cloned into a small fan-out task so cron and webhook events
/// land in the same audit log as orchestrator ticks.
pub async fn spawn_triggers(
    config: &ServerConfig,
    audit: Arc<AuditLog>,
) -> Result<TriggerSurfaces, TriggerError> {
    // Cron.
    let cron_config = CronSchedulerConfig {
        jobs: config
            .cron_jobs
            .iter()
            .map(|j| CronJob {
                name: j.name.clone(),
                cron: j.cron.clone(),
                payload: j.payload.clone(),
            })
            .collect(),
    };
    let scheduler = CronScheduler::from_config(&cron_config)?;
    let (cron_rx, cron_handle) = if scheduler.is_empty() {
        (None, None)
    } else {
        let (rx, h) = scheduler.run();
        (Some(rx), Some(h))
    };

    // Webhook receiver.
    let (webhook_rx, webhook_bind, webhook_handle) =
        if config.webhook.is_some() && webhook_enabled(config) {
            let receiver_config = build_receiver_config(config);
            let (tx, rx) = mpsc::channel::<ReceivedEvent>(64);
            let (bound, handle) = github_webhook_receiver::serve(receiver_config, tx).await?;
            tracing::info!(addr = %bound, "symphony: webhook receiver listening");
            (Some(rx), Some(bound), Some(handle))
        } else {
            (None, None, None)
        };

    // Fan-out task: forward cron + webhook events into the audit log.
    let fanout_handle = tokio::spawn(audit_fanout(cron_rx, webhook_rx, audit));

    Ok(TriggerSurfaces {
        cron_handle,
        webhook_bind,
        webhook_handle,
        fanout_handle,
    })
}

fn webhook_enabled(c: &ServerConfig) -> bool {
    c.webhook
        .as_ref()
        .map(|w| {
            !w.github_secret.is_empty()
                || !w.slack_secret.is_empty()
                || !w.generic_secret.is_empty()
        })
        .unwrap_or(false)
}

fn build_receiver_config(c: &ServerConfig) -> ReceiverConfig {
    let w = c.webhook.as_ref().expect("webhook block presence checked");
    ReceiverConfig {
        bind: w.bind,
        github_secret: w.github_secret.clone(),
        slack_secret: w.slack_secret.clone(),
        generic_secret: w.generic_secret.clone(),
    }
}

async fn audit_fanout(
    mut cron_rx: Option<mpsc::Receiver<ScheduledTaskTriggered>>,
    mut webhook_rx: Option<mpsc::Receiver<ReceivedEvent>>,
    audit: Arc<AuditLog>,
) {
    loop {
        if cron_rx.is_none() && webhook_rx.is_none() {
            return;
        }
        tokio::select! {
            biased;
            // Cron events.
            ev = next_opt(cron_rx.as_mut()) => match ev {
                Some(ev) => {
                    let summary = format!(
                        "cron `{}` fired ({} UTC)",
                        ev.name,
                        ev.fired_at.format("%Y-%m-%dT%H:%M:%SZ"),
                    );
                    tracing::info!(target: "symphony::cron", "{}", summary);
                    audit.record(
                        AuditEvent::new(AuditEventKind::Tick).with_message(summary),
                    );
                }
                None => {
                    cron_rx = None;
                }
            },
            // Webhook events.
            ev = next_opt(webhook_rx.as_mut()) => match ev {
                Some(env) => {
                    let summary = describe_webhook(&env.event);
                    tracing::info!(target: "symphony::webhook", "{}", summary);
                    audit.record(
                        AuditEvent::new(AuditEventKind::Tick).with_message(summary),
                    );
                }
                None => {
                    webhook_rx = None;
                }
            },
        }
        if cron_rx.is_none() && webhook_rx.is_none() {
            return;
        }
    }
}

/// Helper that turns `&mut Option<Receiver<T>>` into a future that yields
/// `Option<T>` (and `None` when the channel is closed). Returns a future
/// that never resolves when the option is `None`, so the `tokio::select!`
/// branch is effectively disabled.
async fn next_opt<T>(rx: Option<&mut mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn describe_webhook(ev: &WebhookEvent) -> String {
    match ev {
        WebhookEvent::GithubPullRequest {
            action,
            repo,
            number,
            title,
            ..
        } => format!("github PR {repo}#{number} {action}: {title}"),
        WebhookEvent::GithubPullRequestReview {
            action,
            state,
            repo,
            number,
            ..
        } => format!("github PR review {repo}#{number} {action}/{state}"),
        WebhookEvent::GithubIssue {
            action,
            repo,
            number,
            title,
            ..
        } => format!("github issue {repo}#{number} {action}: {title}"),
        WebhookEvent::GithubPush {
            repo,
            ref_,
            commits,
            pusher,
        } => format!("github push {repo} {ref_} ({commits} commit(s) by {pusher})"),
        WebhookEvent::Slack {
            command,
            channel,
            user,
            ..
        } => {
            format!("slack {command} from {user} in {channel}")
        }
        WebhookEvent::Generic { .. } => "generic webhook".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{CronJobConfig, ServerConfig, WebhookConfig};
    use std::net::SocketAddr;

    #[tokio::test]
    async fn empty_server_config_spawns_no_listeners() {
        let cfg = ServerConfig::default();
        let audit = Arc::new(AuditLog::open(
            std::env::temp_dir().join(format!("symphony-test-{}.log", uuid::Uuid::new_v4())),
        ));
        let surfaces = spawn_triggers(&cfg, audit).await.unwrap();
        assert!(surfaces.cron_handle.is_none());
        assert!(surfaces.webhook_bind.is_none());
        assert!(surfaces.webhook_handle.is_none());
        // Fan-out should exit cleanly because both channels are absent.
        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            surfaces.fanout_handle,
        )
        .await
        .expect("fanout should exit promptly")
        .unwrap();
    }

    #[tokio::test]
    async fn webhook_receiver_binds_when_secret_set() {
        let cfg = ServerConfig {
            cron_jobs: vec![],
            webhook: Some(WebhookConfig {
                bind: SocketAddr::from(([127, 0, 0, 1], 0)),
                github_secret: "topsecret".into(),
                slack_secret: String::new(),
                generic_secret: String::new(),
            }),
        };
        let audit = Arc::new(AuditLog::open(
            std::env::temp_dir().join(format!("symphony-test-{}.log", uuid::Uuid::new_v4())),
        ));
        let surfaces = spawn_triggers(&cfg, audit).await.unwrap();
        assert!(surfaces.webhook_bind.is_some(), "should bind a port");
        // Tear down.
        if let Some(h) = surfaces.webhook_handle {
            h.abort();
        }
    }

    #[tokio::test]
    async fn cron_config_validation_fails_loudly() {
        let cfg = ServerConfig {
            cron_jobs: vec![CronJobConfig {
                name: "bad".into(),
                cron: "not-cron".into(),
                payload: serde_json::Value::Null,
            }],
            webhook: None,
        };
        let audit = Arc::new(AuditLog::open(
            std::env::temp_dir().join(format!("symphony-test-{}.log", uuid::Uuid::new_v4())),
        ));
        assert!(spawn_triggers(&cfg, audit).await.is_err());
    }
}
