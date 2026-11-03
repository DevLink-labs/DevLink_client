//! Ordered real HTTP responses exercise concurrent verification on one WS generation.
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use devlink_broker::{
    agent_context::ContextClient,
    central::{CentralIdentity, CentralState, ConnectionStatus},
    sessions::{IdentityError, IdentityProvider, Principal, ReadyIdentityProvider},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Notify, RwLock},
    task::JoinHandle,
    time::timeout,
};

#[derive(Clone)]
struct Gate {
    next: Arc<AtomicUsize>,
    arrived: [Arc<Notify>; 2],
    release: [Arc<Notify>; 2],
    responses: Arc<[(StatusCode, Value); 2]>,
}

async fn context(State(gate): State<Gate>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    assert_eq!(
        headers["authorization"],
        "Bearer synthetic-context-review-token"
    );
    assert!(!headers.contains_key("cookie") && !headers.contains_key("origin"));
    let index = gate.next.fetch_add(1, Ordering::SeqCst);
    assert!(index < 2, "context verification unexpectedly retried");
    gate.arrived[index].notify_one();
    gate.release[index].notified().await;
    let (status, body) = gate.responses[index].clone();
    (status, Json(body))
}

fn identity(connection: &str, agent: u64) -> CentralIdentity {
    CentralIdentity {
        connection_id: connection.into(),
        agent_id: agent,
        user_id: None,
        team_id: None,
    }
}

fn valid(agent: u64, user: u64, team: u64) -> (StatusCode, Value) {
    (
        StatusCode::OK,
        json!({"success":true,"data":{"agentId":agent,"ownerUserId":user,"teamId":team}}),
    )
}

fn denied() -> (StatusCode, Value) {
    (
        StatusCode::UNAUTHORIZED,
        json!({"success":false,"code":"AUTHENTICATION_REQUIRED"}),
    )
}

struct Fixture {
    state: CentralState,
    gate: Gate,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new(responses: [(StatusCode, Value); 2]) -> Self {
        let gate = Gate {
            next: Arc::default(),
            arrived: [Arc::default(), Arc::default()],
            release: [Arc::default(), Arc::default()],
            responses: Arc::new(responses),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/v1/agents/me", get(context))
            .with_state(gate.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
            .with_context_client(
                ContextClient::new(&url, "synthetic-context-review-token").unwrap(),
            );
        let handle = state.begin_session().await;
        handle.set_identity(identity("first-ws", 12)).await;
        handle.set_status(ConnectionStatus::Connected).await;
        Self {
            state,
            gate,
            server,
        }
    }

    async fn request(&self, index: usize) -> JoinHandle<Result<Principal, IdentityError>> {
        let provider = ReadyIdentityProvider::new(self.state.clone());
        let request = tokio::spawn(async move { provider.resolve(None).await });
        timeout(Duration::from_secs(5), self.gate.arrived[index].notified())
            .await
            .expect("context request did not arrive");
        request
    }

    async fn finish(
        &self,
        index: usize,
        request: JoinHandle<Result<Principal, IdentityError>>,
    ) -> Result<Principal, IdentityError> {
        self.gate.release[index].notify_one();
        timeout(Duration::from_secs(5), request)
            .await
            .expect("context response did not settle")
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
async fn concurrent_legacy_successes_can_both_confirm_the_same_subject() {
    let f = Fixture::new([valid(12, 3, 1), valid(12, 3, 1)]).await;
    let first = f.request(0).await;
    let second = f.request(1).await;
    let principal = f.finish(0, first).await.unwrap();
    assert_eq!(f.finish(1, second).await, Ok(principal));
    assert_eq!(f.state.status().await, ConnectionStatus::Connected);
    assert_eq!(f.gate.next.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn concurrent_denial_after_success_invalidates_the_same_connection() {
    let f = Fixture::new([valid(12, 3, 1), denied()]).await;
    let first = f.request(0).await;
    let second = f.request(1).await;
    f.finish(0, first).await.unwrap();
    assert_eq!(
        f.finish(1, second).await,
        Err(IdentityError::Unauthenticated)
    );
    assert_eq!(f.state.status().await, ConnectionStatus::AuthFailed);
    assert_eq!(f.state.identity().await, None);
}

#[tokio::test]
async fn concurrent_success_after_denial_cannot_restore_authentication() {
    let f = Fixture::new([denied(), valid(12, 3, 1)]).await;
    let first = f.request(0).await;
    let second = f.request(1).await;
    assert_eq!(
        f.finish(0, first).await,
        Err(IdentityError::Unauthenticated)
    );
    assert!(f.finish(1, second).await.is_err());
    assert_eq!(f.state.status().await, ConnectionStatus::AuthFailed);
    assert_eq!(f.state.identity().await, None);
}

#[tokio::test]
async fn concurrent_conflicting_user_or_team_invalidates_the_connection() {
    for contradictory in [valid(12, 99, 1), valid(12, 3, 99)] {
        let f = Fixture::new([valid(12, 3, 1), contradictory]).await;
        let first = f.request(0).await;
        let second = f.request(1).await;
        f.finish(0, first).await.unwrap();
        assert_eq!(f.finish(1, second).await, Err(IdentityError::Forbidden));
        assert_eq!(f.state.status().await, ConnectionStatus::AuthFailed);
        assert_eq!(f.state.identity().await, None);
    }
}

#[tokio::test]
async fn denial_from_previous_generation_cannot_invalidate_new_verified_subject() {
    let f = Fixture::new([denied(), valid(12, 4, 2)]).await;
    let old = f.request(0).await;
    let handle = f.state.begin_session().await;
    // Reuse the wire identifiers to prove that the internal generation fence,
    // rather than merely a different connection_id/agent_id, protects the owner.
    handle.set_identity(identity("first-ws", 12)).await;
    handle.set_status(ConnectionStatus::Connected).await;
    let new = f.request(1).await;
    let principal = f.finish(1, new).await.unwrap();
    assert_eq!(principal.user_id.as_str(), "4");
    assert_eq!(principal.team_id.as_str(), "2");
    assert_eq!(f.finish(0, old).await, Err(IdentityError::Unauthenticated));
    assert_eq!(f.state.status().await, ConnectionStatus::Connected);
    let current = f.state.identity().await.unwrap();
    assert_eq!(current.connection_id, "first-ws");
    assert_eq!(current.agent_id, 12);
    assert_eq!(current.user_id, Some(4));
    assert_eq!(current.team_id, Some(2));
}
