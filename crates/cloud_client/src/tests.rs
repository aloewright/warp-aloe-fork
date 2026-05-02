//! End-to-end tests using the in-memory transport.
//!
//! The reconnect/resume state machine is unit-tested in
//! `reconnect.rs` and `session.rs`; this module proves the pieces wire
//! together correctly: a [`crate::CloudAgent`] paired with a server-side
//! [`crate::InMemoryTransport`] should round-trip a TaskSubmit into a
//! TaskEvent stream that surfaces as `AgentEvent`s on the orchestrator
//! side.

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use cloud_protocol::{
    Envelope, Message, OutputStream, TaskControl, TaskEvent, TaskEventKind, TaskResult,
    V1Message,
};
use futures_util::StreamExt;
use orchestrator::{Agent, AgentEvent, AgentId, Role, Task, TaskContext, TaskId};
use tokio::sync::mpsc;

use crate::agent::{CloudAgent, CloudAgentConfig, TransportFactory};
use crate::error::ConnectError;
use crate::reconnect::ReconnectPolicy;
use crate::transport::{InMemoryTransport, Transport};

/// Build a synthetic Task that the cloud agent will dispatch.
fn make_task() -> Task {
    Task {
        id: TaskId::new(),
        role: Role::Worker,
        prompt: "do the thing".into(),
        context: TaskContext {
            cwd: PathBuf::from("/tmp"),
            env: Default::default(),
            metadata: Default::default(),
        },
        budget_hint: None,
    }
}

/// Server-side helper: pop the next frame from the transport, asserting
/// it parses as the expected variant.
async fn expect_submit(t: &mut InMemoryTransport) -> String {
    let env = t.recv().await.unwrap().expect("frame");
    let Message::V1(v1) = env.message;
    match v1 {
        V1Message::TaskSubmit(s) => s.task_id,
        other => panic!("expected TaskSubmit, got {other:?}"),
    }
}

async fn send_event(t: &mut InMemoryTransport, evt: TaskEvent) {
    let env = Envelope::v1(V1Message::TaskEvent(evt));
    t.send(env).await.unwrap();
}

/// Wrap a single in-memory transport in a one-shot factory: the first
/// connect attempt yields the transport, subsequent attempts fail.
fn one_shot_factory(t: InMemoryTransport) -> TransportFactory {
    let cell = Arc::new(Mutex::new(Some(t)));
    Arc::new(move || {
        let cell = cell.clone();
        Box::pin(async move {
            let mut g = cell.lock().unwrap();
            match g.take() {
                Some(t) => {
                    let boxed: Box<dyn Transport> = Box::new(t);
                    Ok(boxed)
                }
                None => Err(ConnectError::Handshake("one-shot exhausted".into())),
            }
        })
    })
}

#[tokio::test]
async fn submit_round_trips_to_event_stream() {
    let (client_t, mut server_t) = InMemoryTransport::pair();
    let policy = ReconnectPolicy::new(
        Duration::from_millis(5),
        Duration::from_millis(20),
        Some(0), // no retries
    );
    let factory = one_shot_factory(client_t);
    let agent = CloudAgent::new(CloudAgentConfig {
        id: AgentId("cloud".into()),
        reconnect: policy,
        transport_factory: factory,
    });

    let task = make_task();
    let task_id = task.id;

    let stream = agent.execute(task).await.unwrap();
    tokio::pin!(stream);

    // Server reads the submit, then drives a synthetic task lifecycle.
    let cloud_task_id = expect_submit(&mut server_t).await;
    send_event(
        &mut server_t,
        TaskEvent {
            task_id: cloud_task_id.clone(),
            sequence: 0,
            timestamp: None,
            kind: TaskEventKind::Output {
                stream: OutputStream::Stdout,
                data: "hello\n".into(),
            },
        },
    )
    .await;
    send_event(
        &mut server_t,
        TaskEvent {
            task_id: cloud_task_id.clone(),
            sequence: 1,
            timestamp: None,
            kind: TaskEventKind::Completed {
                result: TaskResult::Success {
                    output: Some(serde_json::json!("done!")),
                },
            },
        },
    )
    .await;

    // Collect events until terminal.
    let mut got_started = false;
    let mut got_chunk: Option<String> = None;
    let mut got_completed = false;
    while let Some(ev) = stream.next().await {
        match ev {
            AgentEvent::Started { task_id: t } => {
                assert_eq!(t, task_id);
                got_started = true;
            }
            AgentEvent::OutputChunk { text } => got_chunk = Some(text),
            AgentEvent::Completed {
                task_id: t,
                summary,
            } => {
                assert_eq!(t, task_id);
                assert_eq!(summary.as_deref(), Some("done!"));
                got_completed = true;
                break;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(got_started, "Started not emitted");
    assert_eq!(got_chunk.as_deref(), Some("hello\n"));
    assert!(got_completed, "Completed not emitted");
}

#[tokio::test]
async fn task_failure_translates_to_failed_event() {
    let (client_t, mut server_t) = InMemoryTransport::pair();
    let factory = one_shot_factory(client_t);
    let agent = CloudAgent::new(CloudAgentConfig {
        id: AgentId("cloud".into()),
        reconnect: ReconnectPolicy::new(
            Duration::from_millis(5),
            Duration::from_millis(20),
            Some(0),
        ),
        transport_factory: factory,
    });

    let task = make_task();
    let task_id = task.id;
    let stream = agent.execute(task).await.unwrap();
    tokio::pin!(stream);

    let cloud_task_id = expect_submit(&mut server_t).await;
    send_event(
        &mut server_t,
        TaskEvent {
            task_id: cloud_task_id,
            sequence: 0,
            timestamp: None,
            kind: TaskEventKind::Completed {
                result: TaskResult::Error {
                    code: "boom".into(),
                    message: "kaboom".into(),
                    details: None,
                },
            },
        },
    )
    .await;

    let mut saw_failed = false;
    while let Some(ev) = stream.next().await {
        if let AgentEvent::Failed {
            task_id: t,
            error,
        } = ev
        {
            assert_eq!(t, task_id);
            assert!(error.contains("boom"));
            assert!(error.contains("kaboom"));
            saw_failed = true;
            break;
        }
    }
    assert!(saw_failed, "Failed event not emitted");
}

#[tokio::test]
async fn cancelled_terminal_translates_to_failed_cancelled() {
    let (client_t, mut server_t) = InMemoryTransport::pair();
    let factory = one_shot_factory(client_t);
    let agent = CloudAgent::new(CloudAgentConfig {
        id: AgentId("cloud".into()),
        reconnect: ReconnectPolicy::new(
            Duration::from_millis(5),
            Duration::from_millis(20),
            Some(0),
        ),
        transport_factory: factory,
    });

    let task = make_task();
    let stream = agent.execute(task).await.unwrap();
    tokio::pin!(stream);

    let cloud_task_id = expect_submit(&mut server_t).await;
    send_event(
        &mut server_t,
        TaskEvent {
            task_id: cloud_task_id,
            sequence: 0,
            timestamp: None,
            kind: TaskEventKind::Completed {
                result: TaskResult::Cancelled,
            },
        },
    )
    .await;

    let mut saw = false;
    while let Some(ev) = stream.next().await {
        if let AgentEvent::Failed { error, .. } = ev {
            assert_eq!(error, "cancelled");
            saw = true;
            break;
        }
    }
    assert!(saw);
}

#[tokio::test]
async fn reconnect_resumes_with_last_sequence_signal() {
    // Two transports: the server-side of the FIRST connection drops mid-
    // task; the SECOND connection should see a Resume signal that names
    // the high-water sequence the client already saw.
    let (client_a, mut server_a) = InMemoryTransport::pair();
    let (client_b, mut server_b) = InMemoryTransport::pair();

    let queue: Arc<Mutex<Vec<InMemoryTransport>>> = Arc::new(Mutex::new(vec![client_b, client_a]));
    let attempts = Arc::new(AtomicUsize::new(0));
    let attempts_for_factory = attempts.clone();

    let factory: TransportFactory = Arc::new(move || {
        let queue = queue.clone();
        let attempts = attempts_for_factory.clone();
        Box::pin(async move {
            attempts.fetch_add(1, Ordering::SeqCst);
            let mut g = queue.lock().unwrap();
            match g.pop() {
                Some(t) => {
                    let boxed: Box<dyn Transport> = Box::new(t);
                    Ok(boxed)
                }
                None => Err(ConnectError::Handshake("queue empty".into())),
            }
        })
    });

    let agent = CloudAgent::new(CloudAgentConfig {
        id: AgentId("cloud".into()),
        // Tight backoff so the test runs fast.
        reconnect: ReconnectPolicy::new(
            Duration::from_millis(1),
            Duration::from_millis(5),
            Some(5),
        ),
        transport_factory: factory,
    });

    let task = make_task();
    let stream = agent.execute(task).await.unwrap();
    tokio::pin!(stream);

    // First connection: client submits, server delivers seq=3, then closes.
    let cloud_task_id = expect_submit(&mut server_a).await;
    send_event(
        &mut server_a,
        TaskEvent {
            task_id: cloud_task_id.clone(),
            sequence: 3,
            timestamp: None,
            kind: TaskEventKind::Output {
                stream: OutputStream::Stdout,
                data: "first\n".into(),
            },
        },
    )
    .await;
    server_a.close().await.unwrap();

    // Second connection: expect a Resume signal carrying last_sequence = 3.
    let env = server_b.recv().await.unwrap().expect("resume frame");
    let Message::V1(v1) = env.message;
    let last = match v1 {
        V1Message::TaskControl(TaskControl::Signal {
            task_id,
            name,
            payload,
        }) => {
            assert_eq!(task_id, cloud_task_id);
            assert_eq!(name, "resume");
            payload.expect("payload")["last_sequence"]
                .as_u64()
                .expect("u64")
        }
        other => panic!("expected resume signal, got {other:?}"),
    };
    assert_eq!(last, 3);

    // Server replays seq=4 and then completes. Client must not deliver a
    // duplicate of seq=3 (which it already saw on the first connection)
    // and should surface seq=4 plus the terminal Completed.
    send_event(
        &mut server_b,
        TaskEvent {
            task_id: cloud_task_id.clone(),
            sequence: 3, // duplicate of what we already delivered
            timestamp: None,
            kind: TaskEventKind::Output {
                stream: OutputStream::Stdout,
                data: "should be deduped\n".into(),
            },
        },
    )
    .await;
    send_event(
        &mut server_b,
        TaskEvent {
            task_id: cloud_task_id.clone(),
            sequence: 4,
            timestamp: None,
            kind: TaskEventKind::Output {
                stream: OutputStream::Stdout,
                data: "second\n".into(),
            },
        },
    )
    .await;
    send_event(
        &mut server_b,
        TaskEvent {
            task_id: cloud_task_id,
            sequence: 5,
            timestamp: None,
            kind: TaskEventKind::Completed {
                result: TaskResult::Success { output: None },
            },
        },
    )
    .await;

    let mut chunks: Vec<String> = vec![];
    let mut completed = false;
    let (done_tx, mut done_rx) = mpsc::channel::<()>(1);
    let collect = async {
        while let Some(ev) = stream.next().await {
            match ev {
                AgentEvent::Started { .. } => {}
                AgentEvent::OutputChunk { text } => chunks.push(text),
                AgentEvent::Completed { .. } => {
                    completed = true;
                    let _ = done_tx.send(()).await;
                    break;
                }
                AgentEvent::Failed { error, .. } => panic!("unexpected fail: {error}"),
                _ => {}
            }
        }
    };
    tokio::select! {
        _ = collect => {}
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("test timed out; chunks so far: {chunks:?} completed={completed}");
        }
    }
    let _ = done_rx.recv().await;

    assert!(completed, "Completed not emitted");
    assert_eq!(chunks, vec!["first\n".to_string(), "second\n".to_string()]);
    assert!(attempts.load(Ordering::SeqCst) >= 2);
}

#[tokio::test]
async fn reconnect_exhausted_flips_health_and_fails() {
    // Factory always errors → policy gives up after 1 attempt.
    let factory: TransportFactory = Arc::new(|| {
        Box::pin(async {
            Err::<Box<dyn Transport>, _>(ConnectError::Handshake("nope".into()))
        })
    });
    let agent = CloudAgent::new(CloudAgentConfig {
        id: AgentId("cloud".into()),
        reconnect: ReconnectPolicy::new(
            Duration::from_millis(1),
            Duration::from_millis(2),
            Some(1),
        ),
        transport_factory: factory,
    });

    let task = make_task();
    let stream = agent.execute(task).await.unwrap();
    tokio::pin!(stream);

    let mut saw = false;
    while let Some(ev) = stream.next().await {
        if let AgentEvent::Failed { error, .. } = ev {
            assert!(
                error.contains("reconnect exhausted") || error.contains("nope"),
                "unexpected error string: {error}"
            );
            saw = true;
            break;
        }
    }
    assert!(saw);
    // Health should be flipped to unhealthy by the time the driver gave up.
    assert!(!agent.health().healthy);
}
