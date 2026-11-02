use devlink_broker::{
    central::{self, CentralState, ConnectionStatus},
    config::Config,
    notifications::Notifications,
    private_file,
    task_operations::TaskStatus,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{Notify, RwLock},
    time::{sleep, timeout},
};
use tokio_tungstenite::{accept_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

fn notice(version: u64, status: &str) -> Value {
    if status == "COMPLETED" {
        json!({"type":"TASK_COMPLETED","event_id":uuid::Uuid::new_v4().to_string(),"task_id":7,"version":version,
            "payload":{"status":status,"result_text":"  完了\n검증 통과  ","partially_modified":false,"started_at":"2026-11-02T10:00:00","completed_at":"2026-11-02T10:01:00"}})
    } else {
        json!({"type":"TASK_STATUS_CHANGED","event_id":uuid::Uuid::new_v4().to_string(),"task_id":7,"version":version,
            "payload":{"previous_status":if status=="FAILED" {"IN_PROGRESS"} else {"PENDING"},"status":status}})
    }
}

#[tokio::test]
async fn admitted_burst_survives_socket_close_without_unreported_loss() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let store = Notifications::open(&path).unwrap();
    let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
        .with_notifications(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("ws://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket.send(Message::Text(json!({"type":"BROKER_READY","payload":{"connection_id":"burst","agent_id":11,"user_id":3,"team_id":31}}).to_string().into())).await.unwrap();
        for task in 1..=8 {
            let mut event = notice(2, "COMPLETED");
            event["task_id"] = json!(task);
            socket
                .send(Message::Text(event.to_string().into()))
                .await
                .unwrap();
        }
        socket.send(Message::Close(None)).await.unwrap();
    });
    let cancel = CancellationToken::new();
    timeout(
        Duration::from_secs(10),
        central::run_with_state(
            Config {
                server_url: origin.clone(),
                auto_reconnect: false,
                ..Config::default()
            },
            "synthetic-token".into(),
            state,
            cancel,
        ),
    )
    .await
    .unwrap();
    server.await.unwrap();
    timeout(Duration::from_secs(15), async {
        loop {
            let stored = Notifications::list_config(&path, &origin, 11, 20).unwrap();
            if stored.len() as u64 + store.rejected() == 8 {
                assert_eq!(stored.len(), 8);
                break;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
}

async fn exercise(corrupt: bool, replaced: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    if corrupt {
        private_file::write(&Notifications::path(&path), &json!({"invalid":true}), 1024).unwrap();
    }
    let store = Notifications::open(&path).unwrap();
    let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
        .with_notifications(store.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("ws://{}", listener.local_addr().unwrap());
    let release = Arc::new(Notify::new());
    let release_server = release.clone();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"BROKER_READY","payload":{
            "connection_id":"requester-socket","agent_id":11,"user_id":3,"team_id":31}})
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        release_server.notified().await;
        let completed = notice(2, "COMPLETED");
        for event in [
            notice(1, "IN_PROGRESS"),
            completed.clone(),
            completed,
            notice(1, "IN_PROGRESS"),
            notice(2, "FAILED"),
        ] {
            socket
                .send(Message::Text(event.to_string().into()))
                .await
                .unwrap();
        }
        if replaced {
            return;
        }
        socket.send(Message::Ping(vec![7].into())).await.unwrap();
        // Notification persistence must not generate TASK_CREATED ACKs, even on error.
        timeout(Duration::from_secs(5), async {
            loop {
                match socket.next().await.unwrap().unwrap() {
                    Message::Pong(_) => break,
                    Message::Text(text) => {
                        assert_ne!(serde_json::from_str::<Value>(&text).unwrap()["type"], "ACK")
                    }
                    other => panic!("Unexpected frame {other:?}"),
                }
            }
        })
        .await
        .unwrap();
        while let Some(Ok(message)) = socket.next().await {
            if matches!(message, Message::Close(_)) {
                break;
            }
            if let Message::Text(text) = message {
                assert_ne!(serde_json::from_str::<Value>(&text).unwrap()["type"], "ACK");
            }
        }
    });
    let cancel = CancellationToken::new();
    let client = tokio::spawn(central::run_with_state(
        Config {
            server_url: origin.clone(),
            auto_reconnect: false,
            auth_timeout_ms: 2000,
            ..Config::default()
        },
        "synthetic-token".into(),
        state.clone(),
        cancel.clone(),
    ));
    timeout(Duration::from_secs(5), async {
        while state.status().await != ConnectionStatus::Connected {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    if replaced {
        state.begin_session().await;
    }
    release.notify_one();
    if replaced {
        timeout(Duration::from_secs(5), client)
            .await
            .unwrap()
            .unwrap();
        assert!(
            Notifications::list_config(&path, &origin, 11, 20)
                .unwrap()
                .is_empty()
        );
    } else if corrupt {
        timeout(Duration::from_secs(10), async {
            while store.rejected() < 6 {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(state.status().await, ConnectionStatus::Connected);
        assert!(Notifications::list_config(&path, &origin, 11, 20).is_err());
        cancel.cancel();
        timeout(Duration::from_secs(5), client)
            .await
            .unwrap()
            .unwrap();
    } else {
        timeout(Duration::from_secs(10), async {
            while store.rejected() == 0 {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let list = Notifications::list_config(&path, &origin, 11, 20).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, TaskStatus::Completed);
        assert_eq!(list[0].summary.as_deref(), Some("  完了\n검증 통과  "));
        assert_eq!(list[0].version, "2");
        assert!(
            Notifications::list_config(&path, &origin, 22, 20)
                .unwrap()
                .is_empty()
        );
        assert_eq!(state.status().await, ConnectionStatus::Connected);
        cancel.cancel();
        timeout(Duration::from_secs(5), client)
            .await
            .unwrap()
            .unwrap();
    }
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn socket_notifications_are_requester_scoped_deduplicated_monotonic_and_never_acked() {
    exercise(false, false).await;
}
#[tokio::test]
async fn store_failure_preserves_connection_and_central_lookup_path() {
    exercise(true, false).await;
}
#[tokio::test]
async fn replaced_generation_cannot_publish_old_socket_notifications() {
    exercise(false, true).await;
}
