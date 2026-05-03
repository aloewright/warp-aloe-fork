//! Live `WORKFLOW.md` reload (PDX-111 / Symphony §6.2).
//!
//! Wraps the loaded [`WorkflowDefinition`] in a swap-able handle and runs a
//! debounced filesystem watcher that re-parses the workflow file on changes.
//!
//! ## Semantics
//!
//! * The watcher subscribes to the *file path* supplied at startup, not the
//!   parent directory — replacing the file (`mv tmp WORKFLOW.md`) is treated
//!   as a deliberate "reload" gesture and triggers a re-read.
//! * Debounce is 200ms; rapid edits coalesce into a single reload attempt.
//! * Re-parse failures are logged via [`tracing::warn!`] and **the previously
//!   loaded definition stays live** — no surface is torn down on bad edits.
//! * `workspace.root` is **immutable** at runtime. Switching root mid-run
//!   would orphan the open per-issue workspaces, so a reload that mutates
//!   `workspace.root` is rejected: the previous definition is kept and a
//!   [`AuditEventKind::WorkflowReloadRejected`] event is recorded.
//! * In-flight runs continue under the snapshot they captured at dispatch
//!   time; only the next tick (the orchestrator re-loads via the handle) sees
//!   the new config.
//!
//! ## Atomicity
//!
//! The handle uses `std::sync::RwLock<Arc<WorkflowDefinition>>`. Readers call
//! [`WorkflowHandle::load`], which clones the `Arc` under a brief read lock —
//! cheap and contention-free in the common case. Writers (the watcher
//! callback) hold the write lock only long enough to swap the `Arc`.

use std::path::{Path, PathBuf};
use std::sync::{mpsc as std_mpsc, Arc, RwLock};
use std::thread;
use std::time::Duration;

use notify_debouncer_full::{
    new_debouncer_opt,
    notify::{Config, EventKind, RecommendedWatcher, RecursiveMode},
    DebounceEventHandler, DebounceEventResult, NoCache,
};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::audit::{AuditEvent, AuditEventKind, AuditLog};
use crate::workflow::{WorkflowDefinition, WorkflowError};

const DEBOUNCE_MS: u64 = 200;

/// Atomic, swap-able container around a [`WorkflowDefinition`].
///
/// Cheap to clone via `Arc`. All readers should call [`WorkflowHandle::load`]
/// to obtain a snapshot `Arc<WorkflowDefinition>` and read from it; the
/// snapshot stays valid even if the watcher swaps in a new definition
/// concurrently.
pub struct WorkflowHandle {
    inner: RwLock<Arc<WorkflowDefinition>>,
}

impl std::fmt::Debug for WorkflowHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowHandle").finish_non_exhaustive()
    }
}

impl WorkflowHandle {
    /// Wrap an initial definition.
    pub fn new(initial: WorkflowDefinition) -> Self {
        Self {
            inner: RwLock::new(Arc::new(initial)),
        }
    }

    /// Cheap snapshot read. The returned `Arc` is independent of the handle:
    /// once cloned, a concurrent swap will not invalidate it.
    pub fn load(&self) -> Arc<WorkflowDefinition> {
        // `RwLock::read` poisons only on writer panic — fall back to a fresh
        // wrapper around the inner value so we don't crash the orchestrator
        // on observability surfaces.
        match self.inner.read() {
            Ok(g) => Arc::clone(&g),
            Err(poisoned) => Arc::clone(&poisoned.into_inner()),
        }
    }

    /// Replace the live definition. Called by the watcher when a parse
    /// succeeds and the new config is compatible with the running daemon.
    fn store(&self, new: Arc<WorkflowDefinition>) {
        match self.inner.write() {
            Ok(mut g) => {
                *g = new;
            }
            Err(poisoned) => {
                let mut g = poisoned.into_inner();
                *g = new;
            }
        }
    }
}

/// Errors raised when starting the [`WorkflowWatcher`].
#[derive(Debug, Error)]
pub enum WatchError {
    /// The OS-level file watcher could not be constructed.
    #[error("failed to create file watcher: {0}")]
    Watcher(String),
}

/// Background watcher that re-loads `WORKFLOW.md` on debounced changes.
///
/// The watcher owns:
///
///   * an OS-level [`notify_debouncer_full`] instance running on a dedicated
///     thread,
///   * a Tokio task that drains debounced events and applies them via
///     [`apply_reload`].
///
/// Drop the [`WorkflowWatcher`] to stop both.
pub struct WorkflowWatcher {
    /// Dropping this signals the watcher thread to exit cleanly.
    _stop: std_mpsc::SyncSender<()>,
    /// Dropping this aborts the Tokio fan-in task.
    _task: tokio::task::JoinHandle<()>,
}

impl WorkflowWatcher {
    /// Start watching `path`. Reload events update `handle`; rejections are
    /// recorded against `audit`.
    ///
    /// Returns an error only if the OS file-watcher itself cannot be
    /// constructed — a failure to register the path (e.g. parent directory
    /// missing) is logged but not fatal.
    pub fn start(
        path: PathBuf,
        handle: Arc<WorkflowHandle>,
        audit: Arc<AuditLog>,
    ) -> Result<Self, WatchError> {
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<PathBuf>();
        let (stop_tx, stop_rx) = std_mpsc::sync_channel::<()>(0);
        let (setup_tx, setup_rx) = std_mpsc::sync_channel::<Result<(), WatchError>>(0);

        let watch_path = path.clone();
        thread::Builder::new()
            .name("symphony-workflow-reload".into())
            .spawn(move || {
                watcher_thread(watch_path, raw_tx, stop_rx, setup_tx);
            })
            .map_err(|e| WatchError::Watcher(e.to_string()))?;

        setup_rx
            .recv()
            .unwrap_or_else(|_| {
                Err(WatchError::Watcher(
                    "watcher thread exited before signalling setup".into(),
                ))
            })?;

        let task = tokio::spawn(async move {
            while let Some(changed) = raw_rx.recv().await {
                apply_reload(&changed, &handle, &audit);
            }
        });

        Ok(Self {
            _stop: stop_tx,
            _task: task,
        })
    }
}

/// Re-parse `path` and atomically swap the new definition into `handle`.
///
/// Public for unit-testability — callers in production go through
/// [`WorkflowWatcher::start`].
pub fn apply_reload(path: &Path, handle: &WorkflowHandle, audit: &AuditLog) {
    let new_def = match WorkflowDefinition::load(path) {
        Ok(d) => d,
        Err(e) => {
            log_and_audit_failure(path, &e, audit);
            return;
        }
    };

    let prev = handle.load();
    if new_def.config.workspace.root != prev.config.workspace.root {
        let msg = format!(
            "rejected workflow reload: workspace.root changed from {} to {} \
             (mid-run root changes orphan open workspaces); keeping previous root",
            prev.config.workspace.root.display(),
            new_def.config.workspace.root.display(),
        );
        tracing::error!(
            path = %path.display(),
            previous_root = %prev.config.workspace.root.display(),
            attempted_root = %new_def.config.workspace.root.display(),
            "workflow reload rejected: workspace.root is immutable at runtime"
        );
        audit.record(
            AuditEvent::new(AuditEventKind::WorkflowReloadRejected)
                .with_error(msg),
        );
        return;
    }

    handle.store(Arc::new(new_def));
    tracing::info!(
        path = %path.display(),
        "workflow reloaded; new config will take effect on the next tick"
    );
    audit.record(
        AuditEvent::new(AuditEventKind::WorkflowReloaded)
            .with_message(path.display().to_string()),
    );
}

fn log_and_audit_failure(path: &Path, err: &WorkflowError, audit: &AuditLog) {
    tracing::warn!(
        path = %path.display(),
        error = %err,
        "workflow reload failed; keeping previous definition live"
    );
    audit.record(
        AuditEvent::new(AuditEventKind::WorkflowReloadFailed)
            .with_message(path.display().to_string())
            .with_error(err.to_string()),
    );
}

/// Runs in the dedicated watcher thread. Configures a debounced
/// `notify-debouncer-full` against the *file* (not the directory) and parks
/// until [`WorkflowWatcher`] is dropped.
fn watcher_thread(
    path: PathBuf,
    tx: mpsc::UnboundedSender<PathBuf>,
    stop_rx: std_mpsc::Receiver<()>,
    setup_tx: std_mpsc::SyncSender<Result<(), WatchError>>,
) {
    let watch_target = path.clone();
    let bridge = WatcherBridge {
        tx,
        target: watch_target,
    };
    let mut debouncer = match new_debouncer_opt::<_, RecommendedWatcher, _>(
        Duration::from_millis(DEBOUNCE_MS),
        None,
        bridge,
        NoCache,
        Config::default(),
    ) {
        Ok(d) => d,
        Err(e) => {
            let _ = setup_tx.send(Err(WatchError::Watcher(e.to_string())));
            return;
        }
    };

    // Watch the file path itself. On macOS FSEvents (the backend used in
    // this workspace's pinned `notify` fork) this surfaces both modify and
    // rename events even when an editor saves via "atomic write" (rename
    // tmp → target), which is precisely the deliberate "reload gesture"
    // PDX-111 calls out.
    if let Err(e) = debouncer.watch(&path, RecursiveMode::NonRecursive) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "symphony workflow watcher: cannot watch path"
        );
        // Still signal "ready" so the daemon can continue without live
        // reload — this matches Symphony's design that reload is a
        // best-effort observability surface, not load-bearing.
    }

    let _ = setup_tx.send(Ok(()));
    tracing::info!(path = %path.display(), "symphony: watching WORKFLOW.md for live reloads");

    // Park until the WorkflowWatcher is dropped (stop_tx drop ⇒ RecvError).
    let _ = stop_rx.recv();
    tracing::debug!("symphony workflow watcher thread exiting");
    // `debouncer` drops here, releasing OS resources.
}

/// Bridges debounced `notify` events into a Tokio channel. We forward only
/// events whose paths intersect the watched target (defence against stray
/// directory events that some backends emit alongside file events).
struct WatcherBridge {
    tx: mpsc::UnboundedSender<PathBuf>,
    target: PathBuf,
}

impl DebounceEventHandler for WatcherBridge {
    fn handle_event(&mut self, result: DebounceEventResult) {
        match result {
            Ok(events) => {
                for event in events {
                    match event.event.kind {
                        EventKind::Create(_)
                        | EventKind::Modify(_)
                        | EventKind::Remove(_) => {
                            // Some backends emit events with empty `paths`
                            // (FSEvents historic-flag flushes); fall back to
                            // a synthetic "target" event so a deliberate
                            // `mv tmp WORKFLOW.md` still triggers a reload.
                            if event.paths.is_empty() {
                                if self.tx.send(self.target.clone()).is_err() {
                                    return;
                                }
                                continue;
                            }
                            // `event` is a `&DebouncedEvent`, so iterate by
                            // reference and clone matching paths.
                            for p in event.paths.iter() {
                                if path_matches(p, &self.target) {
                                    if self.tx.send(p.clone()).is_err() {
                                        return;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(errors) => {
                for e in errors {
                    tracing::warn!(error = ?e, "symphony workflow watcher error");
                }
            }
        }
    }
}

/// `true` if `event` references the target file. We compare canonicalized
/// paths when both are available so symlinks and `./` prefixes match.
fn path_matches(event: &Path, target: &Path) -> bool {
    if event == target {
        return true;
    }
    match (event.canonicalize(), target.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => event.file_name() == target.file_name() && event.parent() == target.parent(),
    }
}
