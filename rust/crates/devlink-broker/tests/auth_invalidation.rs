use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use devlink_broker::{
    agent_context::ContextClient,
    central::{self, CentralState, ConnectionStatus},
    config::Config,
    inbox::Inbox,
    protocol::{BrokerReadyPayload, ClientMessage, ServerMessage, TaskCreatedPayload},
    sessions::{Identifier, IdentityError, IdentityProvider, ReadyIdentityProvider},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::RwLock,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-auth-invalidation-token";

struct Fixture {
    _dir: tempfile::TempDir,
    state: CentralState,
    inbox: Inbox,
    origin: String,
    mode: Arc<AtomicU8>,
    listener: TcpListener,
    socket: WebSocketStream<TcpStream>,
    cancel: CancellationToken,
    runner: JoinHandle<()>,
    context_runner: JoinHandle<()>,
}

impl Fixture {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Inbox::open(&dir.path().join("config.json")).unwrap();
        let mode = Arc::new(AtomicU8::new(0));
        let remote_mode = mode.clone();
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rest_origin = format!("ws://{}", http.local_addr().unwrap());
        let app = Router::new().route(
            "/api/v1/agents/me",
            get(move |headers: HeaderMap| {
                let mode = remote_mode.clone();
                async move {
                    assert_eq!(headers["authorization"], format!("Bearer {TOKEN}"));
                    match mode.load(Ordering::SeqCst) {
                        1 => (StatusCode::UNAUTHORIZED, Json(json!({"success":false}))),
                        2 => (StatusCode::FORBIDDEN, Json(json!({"success":false}))),
                        value => (
                            StatusCode::OK,
                            Json(json!({"success":true,"data":{
                                "agentId":if value == 3 {13} else {12},"ownerUserId":3,"teamId":1
                            }})),
                        ),
                    }
                }
            }),
        );
        let context_runner = tokio::spawn(async move {
            axum::serve(http, app).await.unwrap();
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("ws://{}", listener.local_addr().unwrap());
        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
            .with_inbox(inbox.clone())
            .with_context_client(ContextClient::new(&rest_origin, TOKEN).unwrap());
        let cancel = CancellationToken::new();
        let runner = tokio::spawn(central::run_with_state(
            Config {
                server_url: origin.clone(),
                reconnect_initial_ms: 20,
                reconnect_max_ms: 20,
                ..Config::default()
            },
            TOKEN.into(),
            state.clone(),
            cancel.clone(),
        ));
        let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                serde_json::to_string(&ServerMessage::BrokerReady(BrokerReadyPayload {
                    connection_id: "auth-invalidation-connection".into(),
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
        Self {
            _dir: dir,
            state,
            inbox,
            origin,
            mode,
            listener,
            socket,
            cancel,
            runner,
            context_runner,
        }
    }

    async fn check_revocation(mut self, mode: u8, expected: IdentityError) {
        self.mode.store(mode, Ordering::SeqCst);
        let provider = ReadyIdentityProvider::new(self.state.clone());
        assert_eq!(provider.resolve(None).await, Err(expected));
        assert_eq!(self.state.status().await, ConnectionStatus::AuthFailed);
        // The old implementation accepted this after clearing team_id, despite
        // the earlier verified team being 1. An already closed socket is safe.
        let message = ServerMessage::TaskCreated {
            event_id: "after-revocation".into(),
            payload: TaskCreatedPayload {
                task_id: 901,
                team_id: 99,
                requester_agent_id: 10,
                receiver_agent_id: 12,
                prompt: "synthetic task after auth denial".into(),
                created_at: "2026-10-29T09:00:00".into(),
            },
        };
        let _ = self
            .socket
            .send(Message::Text(
                serde_json::to_string(&message).unwrap().into(),
            ))
            .await;
        timeout(Duration::from_secs(4), async {
            while let Some(Ok(frame)) = self.socket.next().await {
                match frame {
                    Message::Text(text) => assert!(
                        !matches!(
                            serde_json::from_str::<ClientMessage>(&text),
                            Ok(ClientMessage::Ack(_))
                        ),
                        "ACK after authentication invalidation"
                    ),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        })
        .await
        .expect("invalidated WS must close without ACK");
        timeout(Duration::from_secs(3), &mut self.runner)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(self.state.status().await, ConnectionStatus::AuthFailed);
        assert_eq!(self.state.identity().await, None);
        assert!(self.inbox.list_for(&self.origin, 12).unwrap().is_empty());
        assert!(
            timeout(Duration::from_millis(150), self.listener.accept())
                .await
                .is_err(),
            "authentication denial must not reconnect automatically"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.runner.abort();
        self.context_runner.abort();
    }
}

#[tokio::test]
async fn context_401_closes_ws_without_storing_or_acknowledging_later_tasks() {
    Fixture::start()
        .await
        .check_revocation(1, IdentityError::Unauthenticated)
        .await;
}

#[tokio::test]
async fn context_403_closes_ws_without_storing_or_acknowledging_later_tasks() {
    Fixture::start()
        .await
        .check_revocation(2, IdentityError::Unauthenticated)
        .await;
}

#[tokio::test]
async fn contradictory_context_closes_ws_without_storing_or_acknowledging_later_tasks() {
    Fixture::start()
        .await
        .check_revocation(3, IdentityError::Forbidden)
        .await;
}

#[tokio::test]
async fn verified_context_still_persists_and_acknowledges_a_valid_task() {
    let mut fixture = Fixture::start().await;
    fixture
        .socket
        .send(Message::Text(
            serde_json::to_string(&ServerMessage::TaskCreated {
                event_id: "still-authenticated".into(),
                payload: TaskCreatedPayload {
                    task_id: 903,
                    team_id: 1,
                    requester_agent_id: 10,
                    receiver_agent_id: 12,
                    prompt: "synthetic valid authenticated task".into(),
                    created_at: "2026-10-29T09:00:00".into(),
                },
            })
            .unwrap()
            .into(),
        ))
        .await
        .unwrap();
    timeout(Duration::from_secs(4), async {
        loop {
            if let Some(Ok(Message::Text(text))) = fixture.socket.next().await
                && let Ok(ClientMessage::Ack(ack)) = serde_json::from_str(&text)
            {
                assert_eq!(ack.event_id, "still-authenticated");
                break;
            }
        }
    })
    .await
    .expect("authenticated receipt should persist and ACK normally");
    assert_eq!(
        fixture.inbox.list_for(&fixture.origin, 12).unwrap().len(),
        1
    );
    fixture.cancel.cancel();
    timeout(Duration::from_secs(3), &mut fixture.runner)
        .await
        .unwrap()
        .unwrap();
}
