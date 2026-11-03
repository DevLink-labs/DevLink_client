use devlink_broker::{
    central::{self, CentralState, ConnectionStatus},
    config::Config,
    inbox::Inbox,
    protocol::{
        BrokerReadyPayload, ClientMessage, HeartbeatPing, ServerMessage, TaskCreatedPayload,
    },
};
use futures_util::{SinkExt, StreamExt};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::RwLock,
    time::{sleep, timeout},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;

type Socket = WebSocketStream<TcpStream>;

fn task(event: &str, receiver: u64) -> ServerMessage {
    ServerMessage::TaskCreated {
        event_id: event.into(),
        payload: TaskCreatedPayload {
            task_id: 101,
            team_id: 1,
            requester_agent_id: 10,
            receiver_agent_id: receiver,
            prompt: "synthetic receipt task".into(),
            created_at: "2026-10-29T09:00:00".into(),
        },
    }
}

async fn send(socket: &mut Socket, message: ServerMessage) {
    socket
        .send(Message::Text(
            serde_json::to_string(&message).unwrap().into(),
        ))
        .await
        .unwrap();
}

async fn ready(listener: &TcpListener) -> Socket {
    let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    send(
        &mut socket,
        ServerMessage::BrokerReady(BrokerReadyPayload {
            connection_id: uuid::Uuid::new_v4().to_string(),
            agent_id: 12,
            user_id: None,
            team_id: None,
        }),
    )
    .await;
    socket
}

async fn ack(socket: &mut Socket) -> devlink_broker::protocol::TaskAckPayload {
    timeout(Duration::from_secs(5), async {
        loop {
            let frame = socket.next().await.unwrap().unwrap();
            if let Message::Text(text) = frame {
                match serde_json::from_str::<ClientMessage>(&text).unwrap() {
                    ClientMessage::Ack(ack) => return ack,
                    ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }) => {
                        send(socket, ServerMessage::heartbeat_pong(ping_id)).await;
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .unwrap()
}

fn spawn(
    config: Config,
    inbox: Inbox,
    cancel: CancellationToken,
) -> (CentralState, tokio::task::JoinHandle<()>) {
    let state =
        CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting))).with_inbox(inbox);
    let runner = tokio::spawn(central::run_with_state(
        config,
        "test-receipt-token".into(),
        state.clone(),
        cancel,
    ));
    (state, runner)
}

#[tokio::test]
async fn ack_follows_persistence_and_duplicate_is_safe_across_reconnect_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("ws://{}", listener.local_addr().unwrap());
    let config = Config {
        server_url: origin.clone(),
        reconnect_initial_ms: 50,
        reconnect_max_ms: 50,
        ..Config::default()
    };
    let inbox = Inbox::open(&path).unwrap();
    let cancel = CancellationToken::new();
    let (_, runner) = spawn(config.clone(), inbox.clone(), cancel.clone());
    let mut socket = ready(&listener).await;
    send(&mut socket, task("first", 12)).await;
    assert_eq!(ack(&mut socket).await.event_id, "first");
    // Inspect persisted state independently, not just the live in-memory store.
    let disk = Inbox::open(&path).unwrap();
    assert_eq!(disk.list_for(&origin, 12).unwrap().len(), 1);
    let original = std::fs::read(Inbox::path(&path)).unwrap();
    drop(disk);
    // The same delivery is acknowledged again without rewriting the durable snapshot.
    send(&mut socket, task("first", 12)).await;
    let duplicate = ack(&mut socket).await;
    assert_eq!(
        (duplicate.event_id.as_str(), duplicate.task_id),
        ("first", 101)
    );
    assert_eq!(std::fs::read(Inbox::path(&path)).unwrap(), original);
    // Simulate another delivery after the server loses its receipt observation.
    socket.close(None).await.unwrap();
    drop(socket);
    let mut socket = ready(&listener).await;
    send(&mut socket, task("reconnected", 12)).await;
    assert_eq!(ack(&mut socket).await.event_id, "reconnected");
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(Inbox::path(&path)).unwrap()).unwrap();
    let original: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(after["tasks"], original["tasks"]);
    assert_eq!(after["receipts"].as_array().unwrap().len(), 2);
    cancel.cancel();
    timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
    drop(socket);
    drop(inbox);
    let cancel = CancellationToken::new();
    let (_, runner) = spawn(config, Inbox::open(&path).unwrap(), cancel.clone());
    let mut socket = ready(&listener).await;
    let before = std::fs::read(Inbox::path(&path)).unwrap();
    send(&mut socket, task("first", 12)).await;
    let duplicate = ack(&mut socket).await;
    assert_eq!(
        (duplicate.event_id.as_str(), duplicate.task_id),
        ("first", 101)
    );
    assert_eq!(std::fs::read(Inbox::path(&path)).unwrap(), before);
    send(&mut socket, task("after-restart", 12)).await;
    assert_eq!(ack(&mut socket).await.event_id, "after-restart");
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(Inbox::path(&path)).unwrap()).unwrap();
    assert_eq!(after["tasks"], original["tasks"]);
    assert_eq!(after["receipts"].as_array().unwrap().len(), 3);
    cancel.cancel();
    timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn unready_or_invalid_delivery_never_persists_or_acknowledges() {
    for case in [
        "before-ready",
        "wrong-team",
        "wrong-receiver",
        "missing-task-id",
        "string-task-id",
        "missing-event",
        "zero-requester",
        "oversized-task-id",
        "empty-event",
        "blank-prompt",
        "null-prompt",
        "empty-created-at",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            server_url: format!("ws://{}", listener.local_addr().unwrap()),
            auto_reconnect: false,
            ..Config::default()
        };
        let (state, runner) = spawn(
            config,
            Inbox::open(&path).unwrap(),
            CancellationToken::new(),
        );
        let (stream, _) = timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        if case != "before-ready" {
            send(
                &mut socket,
                ServerMessage::BrokerReady(BrokerReadyPayload {
                    connection_id: format!("invalid-delivery-{case}"),
                    agent_id: 12,
                    user_id: Some(3),
                    team_id: Some(1),
                }),
            )
            .await;
        }
        let mut message = serde_json::to_value(task("invalid-delivery", 12)).unwrap();
        match case {
            "wrong-team" => message["payload"]["team_id"] = 999.into(),
            "wrong-receiver" => message["payload"]["receiver_agent_id"] = 13.into(),
            "missing-task-id" => {
                message["payload"]
                    .as_object_mut()
                    .unwrap()
                    .remove("task_id");
            }
            "string-task-id" => message["payload"]["task_id"] = "101".into(),
            "missing-event" => {
                message.as_object_mut().unwrap().remove("event_id");
            }
            "zero-requester" => message["payload"]["requester_agent_id"] = 0.into(),
            "oversized-task-id" => message["payload"]["task_id"] = u64::MAX.into(),
            "empty-event" => message["event_id"] = "".into(),
            "blank-prompt" => message["payload"]["prompt"] = " \n ".into(),
            "null-prompt" => message["payload"]["prompt"] = serde_json::Value::Null,
            "empty-created-at" => message["payload"]["created_at"] = "".into(),
            "before-ready" => {}
            _ => unreachable!(),
        }
        socket
            .send(Message::Text(message.to_string().into()))
            .await
            .unwrap();
        timeout(Duration::from_secs(3), async {
            while let Some(Ok(frame)) = socket.next().await {
                match frame {
                    Message::Text(text) => assert!(
                        !matches!(
                            serde_json::from_str::<ClientMessage>(&text),
                            Ok(ClientMessage::Ack(_))
                        ),
                        "invalid {case} delivery was acknowledged"
                    ),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("invalid {case} delivery was silently ignored"));
        timeout(Duration::from_secs(3), runner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.status().await,
            ConnectionStatus::ProtocolViolation,
            "{case}"
        );
        assert_eq!(state.identity().await, None, "{case}");
        assert!(
            !Inbox::path(&path).exists(),
            "invalid {case} delivery was persisted"
        );
    }
}

#[tokio::test]
async fn invalid_recipient_or_failed_storage_never_acknowledges() {
    for storage_failure in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        if storage_failure {
            std::fs::create_dir(Inbox::path(&path)).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = Config {
            server_url: format!("ws://{}", listener.local_addr().unwrap()),
            ..Config::default()
        };
        let cancel = CancellationToken::new();
        let (state, runner) = spawn(config, inbox, cancel);
        let mut socket = ready(&listener).await;
        send(
            &mut socket,
            task("must-not-ack", if storage_failure { 12 } else { 13 }),
        )
        .await;
        timeout(Duration::from_secs(5), async {
            while let Some(Ok(frame)) = socket.next().await {
                match frame {
                    Message::Text(text) => assert!(!matches!(
                        serde_json::from_str::<ClientMessage>(&text),
                        Ok(ClientMessage::Ack(_))
                    )),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        timeout(Duration::from_secs(3), runner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            state.status().await,
            if storage_failure {
                ConnectionStatus::InboxFailed
            } else {
                ConnectionStatus::ProtocolViolation
            }
        );
        assert_eq!(state.identity().await, None);
        assert!(
            timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "permanent failure retried"
        );
        sleep(Duration::from_millis(1)).await;
    }
}

#[tokio::test]
async fn conflicting_redelivery_preserves_original_without_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let inbox = Inbox::open(&path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = Config {
        server_url: format!("ws://{}", listener.local_addr().unwrap()),
        ..Config::default()
    };
    let cancel = CancellationToken::new();
    let (state, runner) = spawn(config, inbox, cancel);
    let mut socket = ready(&listener).await;
    send(&mut socket, task("first", 12)).await;
    assert_eq!(ack(&mut socket).await.task_id, 101);
    let original = std::fs::read(Inbox::path(&path)).unwrap();
    let mut conflict = task("conflict", 12);
    if let ServerMessage::TaskCreated { payload, .. } = &mut conflict {
        payload.prompt = "changed payload".into();
    }
    send(&mut socket, conflict).await;
    timeout(Duration::from_secs(5), async {
        while let Some(Ok(frame)) = socket.next().await {
            if matches!(frame, Message::Close(_)) {
                break;
            }
            if let Message::Text(text) = frame {
                assert!(!matches!(
                    serde_json::from_str::<ClientMessage>(&text),
                    Ok(ClientMessage::Ack(_))
                ));
            }
        }
    })
    .await
    .unwrap();
    timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.status().await, ConnectionStatus::InboxFailed);
    assert_eq!(std::fs::read(Inbox::path(&path)).unwrap(), original);
}

#[tokio::test]
async fn sixteen_task_burst_persists_and_acknowledges_within_central_receipt_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let inbox = Inbox::open(&path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("ws://{}", listener.local_addr().unwrap());
    let config = Config {
        server_url: origin.clone(),
        ..Config::default()
    };
    let cancel = CancellationToken::new();
    let (_, runner) = spawn(config, inbox, cancel.clone());
    let mut socket = ready(&listener).await;
    let start = std::time::Instant::now();
    for i in 0..16 {
        let mut message = task(&format!("burst-{i}"), 12);
        if let ServerMessage::TaskCreated { payload, .. } = &mut message {
            payload.task_id = 101 + i;
        };
        send(&mut socket, message).await;
    }
    let mut seen = std::collections::HashSet::new();
    for _ in 0..16 {
        let receipt = ack(&mut socket).await;
        assert!(seen.insert((receipt.event_id, receipt.task_id)));
    }
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "16 ACKs exceeded central's default receipt timeout"
    );
    let disk = Inbox::open(&path).unwrap();
    assert_eq!(disk.list_for(&origin, 12).unwrap().len(), 16);
    cancel.cancel();
    timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
}
