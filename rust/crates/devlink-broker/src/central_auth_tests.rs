//! Deterministic revocation test for a receipt whose durable write is in flight.
use crate::{
    agent_context::ContextClient,
    central::{self, CentralState, ConnectionStatus},
    config::Config,
    inbox::Inbox,
    protocol::{BrokerReadyPayload, ClientMessage, ServerMessage, TaskCreatedPayload},
    sessions::{Identifier, IdentityError, IdentityProvider, ReadyIdentityProvider},
};
use axum::{Json, Router, http::StatusCode, routing::get};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::RwLock, task::JoinHandle, time::timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

struct Background {
    cancel: CancellationToken,
    websocket: JoinHandle<()>,
    context: JoinHandle<()>,
}

impl Drop for Background {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.websocket.abort();
        self.context.abort();
    }
}

#[tokio::test]
async fn auth_denial_discards_ack_for_an_already_accepted_pending_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let inbox = Inbox::open(&path).unwrap();
    // Hold the store lock before receiving the task. The test-only semaphore
    // observation below proves record() started and is blocked on this lock.
    let (release, paused) = inbox.pause_writes();
    paused.await.unwrap();
    let denied = Arc::new(AtomicBool::new(false));
    let remote_denied = denied.clone();
    let context_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let context_origin = format!("ws://{}", context_listener.local_addr().unwrap());
    let app = Router::new().route(
        "/api/v1/agents/me",
        get(move || {
            let denied = remote_denied.clone();
            async move {
                if denied.load(Ordering::SeqCst) {
                    (StatusCode::UNAUTHORIZED, Json(json!({"success":false})))
                } else {
                    (
                        StatusCode::OK,
                        Json(json!({"success":true,"data":{
                            "agentId":12,"ownerUserId":3,"teamId":1
                        }})),
                    )
                }
            }
        }),
    );
    let context_runner = tokio::spawn(async move {
        axum::serve(context_listener, app).await.unwrap();
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("ws://{}", listener.local_addr().unwrap());
    let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
        .with_inbox(inbox.clone())
        .with_context_client(
            ContextClient::new(&context_origin, "synthetic-pending-ack-token").unwrap(),
        );
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(central::run_with_state(
        Config {
            server_url: origin.clone(),
            auto_reconnect: false,
            ..Config::default()
        },
        "synthetic-pending-ack-token".into(),
        state.clone(),
        cancel.clone(),
    ));
    let mut background = Background {
        cancel,
        websocket: runner,
        context: context_runner,
    };
    let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&ServerMessage::BrokerReady(BrokerReadyPayload {
                connection_id: "pending-ack-revocation".into(),
                agent_id: 12,
                user_id: Some(3),
                team_id: Some(1),
            }))
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while state.status().await != ConnectionStatus::Connected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let provider = ReadyIdentityProvider::new(state.clone());
    provider
        .resolve(Some(&Identifier::parse("12").unwrap()))
        .await
        .unwrap();
    socket
        .send(Message::Text(
            serde_json::to_string(&ServerMessage::TaskCreated {
                event_id: "accepted-before-auth-denial".into(),
                payload: TaskCreatedPayload {
                    task_id: 902,
                    team_id: 1,
                    requester_agent_id: 10,
                    receiver_agent_id: 12,
                    prompt: "accepted before context was revoked".into(),
                    created_at: "2026-10-29T09:00:00".into(),
                },
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(3), async {
        while !inbox.has_pending_write() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("receipt must be queued before authentication invalidation");
    denied.store(true, Ordering::SeqCst);
    assert_eq!(
        provider.resolve(None).await,
        Err(IdentityError::Unauthenticated)
    );
    assert_eq!(state.status().await, ConnectionStatus::AuthFailed);
    release.send(()).unwrap();
    timeout(Duration::from_secs(4), async {
        while let Some(Ok(frame)) = socket.next().await {
            match frame {
                Message::Text(text) => assert!(
                    !matches!(
                        serde_json::from_str::<ClientMessage>(&text),
                        Ok(ClientMessage::Ack(_))
                    ),
                    "queued receipt ACK escaped after authentication invalidation"
                ),
                Message::Close(_) => break,
                _ => {}
            }
        }
    })
    .await
    .expect("invalidated WS must terminate without sending queued ACK");
    timeout(Duration::from_secs(3), &mut background.websocket)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.status().await, ConnectionStatus::AuthFailed);
    assert_eq!(state.identity().await, None);
    // A write already accepted under valid authentication can complete. Preserve
    // its durable data; revocation suppresses the network ACK, not the file.
    timeout(Duration::from_secs(5), async {
        loop {
            let inbox = inbox.clone();
            let origin = origin.clone();
            let saved = tokio::task::spawn_blocking(move || inbox.list_for(&origin, 12).unwrap())
                .await
                .unwrap();
            if !saved.is_empty() {
                assert_eq!(saved[0].message.event_id, "accepted-before-auth-denial");
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted durable write must not be discarded");
}
