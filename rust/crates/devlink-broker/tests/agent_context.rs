use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::get,
};
use devlink_broker::{
    agent_context::ContextClient,
    central::{CentralIdentity, CentralState, ConnectionStatus},
    protocol::PresenceStatus,
    sessions::{
        ClaimMode, Identifier, IdentityError, IdentityProvider, ReadyIdentityProvider,
        SessionError, SessionManager,
    },
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Mutex, Notify, RwLock};

#[derive(Clone)]
struct Mock {
    response: Arc<Mutex<(StatusCode, Value)>>,
    calls: Arc<AtomicUsize>,
    arrived: Arc<Notify>,
    resume: Arc<Notify>,
    delayed: Arc<std::sync::atomic::AtomicBool>,
}
async fn context(State(state): State<Mock>, headers: HeaderMap) -> (StatusCode, Json<Value>) {
    assert_eq!(headers["authorization"], "Bearer synthetic-agent-token");
    assert!(!headers.contains_key("cookie") && !headers.contains_key("origin"));
    state.calls.fetch_add(1, Ordering::SeqCst);
    let value = state.response.lock().await.clone();
    if state.delayed.load(Ordering::SeqCst) {
        state.arrived.notify_one();
        state.resume.notified().await;
    }
    (value.0, Json(value.1))
}
fn id(s: &str) -> Identifier {
    Identifier::parse(s).unwrap()
}
fn identity(connection: &str) -> CentralIdentity {
    CentralIdentity {
        connection_id: connection.into(),
        agent_id: 12,
        user_id: None,
        team_id: None,
    }
}
fn body() -> Value {
    json!({"success":true,"data":{"agentId":12,"ownerUserId":3,"teamId":1,"toolType":"CODEX"}})
}
async fn fixture() -> (Mock, CentralState, tokio::task::JoinHandle<()>) {
    let mock = Mock {
        response: Arc::new(Mutex::new((StatusCode::OK, body()))),
        calls: Arc::default(),
        arrived: Arc::default(),
        resume: Arc::default(),
        delayed: Arc::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/api/v1/agents/me", get(context))
        .with_state(mock.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
        .with_context_client(ContextClient::new(&url, "synthetic-agent-token").unwrap());
    let handle = state.begin_session().await;
    handle.set_identity(identity("first")).await;
    handle.set_status(ConnectionStatus::Connected).await;
    (mock, state, server)
}

#[tokio::test]
async fn legacy_ready_uses_live_context_and_reconnect_preserves_current_owner() {
    let (mock, state, server) = fixture().await;
    let manager = SessionManager::new(
        Arc::new(ReadyIdentityProvider::new(state.clone())),
        state.clone(),
    );
    let session = manager
        .claim(id("host"), ClaimMode::Fresh, Some(id("12")))
        .await
        .unwrap();
    assert_eq!(session.principal.user_id, id("3"));
    assert_eq!(state.presence().await, Some(PresenceStatus::Online));
    assert_eq!(
        manager
            .claim(id("forged"), ClaimMode::Fresh, Some(id("13")))
            .await,
        Err(SessionError::Identity(IdentityError::Forbidden))
    );
    let next = state.begin_session().await;
    next.set_identity(identity("reconnected")).await;
    next.set_status(ConnectionStatus::Connected).await;
    assert_eq!(state.presence().await, Some(PresenceStatus::Offline));
    manager.expire().await;
    assert_eq!(state.presence().await, Some(PresenceStatus::Offline));
    manager
        .heartbeat(&session.session_id, &id("host"))
        .await
        .unwrap();
    assert_eq!(
        manager
            .claim(id("host"), ClaimMode::Recover, Some(id("12")))
            .await
            .unwrap(),
        session
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), 4);
    server.abort();
}

#[tokio::test]
async fn revoked_token_cannot_renew_and_invalidates_previous_ownership() {
    let (mock, state, server) = fixture().await;
    let manager = SessionManager::new(
        Arc::new(ReadyIdentityProvider::new(state.clone())),
        state.clone(),
    );
    let old = manager
        .claim(id("host"), ClaimMode::Fresh, Some(id("12")))
        .await
        .unwrap();
    *mock.response.lock().await = (
        StatusCode::UNAUTHORIZED,
        json!({"message":"secret-do-not-echo"}),
    );
    assert_eq!(
        manager.heartbeat(&old.session_id, &id("host")).await,
        Err(SessionError::Identity(IdentityError::Unauthenticated))
    );
    assert_eq!(state.presence().await, Some(PresenceStatus::Offline));
    *mock.response.lock().await = (StatusCode::OK, body());
    assert_eq!(
        manager.heartbeat(&old.session_id, &id("host")).await,
        Err(SessionError::Expired)
    );
    let next = state.begin_session().await;
    next.set_identity(identity("after-revocation")).await;
    next.set_status(ConnectionStatus::Connected).await;
    assert_eq!(
        manager.heartbeat(&old.session_id, &id("host")).await,
        Err(SessionError::Expired)
    );
    server.abort();
}

#[tokio::test]
async fn legacy_agent_a_b_a_cannot_restore_the_original_owner() {
    let (_mock, state, server) = fixture().await;
    let manager = SessionManager::new(
        Arc::new(ReadyIdentityProvider::new(state.clone())),
        state.clone(),
    );
    let old = manager
        .claim(id("host"), ClaimMode::Fresh, Some(id("12")))
        .await
        .unwrap();
    let other = state.begin_session().await;
    let mut changed = identity("agent-b");
    changed.agent_id = 13;
    other.set_identity(changed).await;
    other.set_status(ConnectionStatus::Connected).await;
    // No context call for B; the authenticated READY already proves the Agent changed.
    let original = state.begin_session().await;
    original.set_identity(identity("agent-a-again")).await;
    original.set_status(ConnectionStatus::Connected).await;
    assert_eq!(
        manager.heartbeat(&old.session_id, &id("host")).await,
        Err(SessionError::Expired)
    );
    assert_eq!(
        manager.release(&old.session_id, &id("host")).await,
        Err(SessionError::Expired)
    );
    server.abort();
}

#[tokio::test]
async fn late_context_reply_cannot_authenticate_a_new_connection() {
    let (mock, state, server) = fixture().await;
    mock.delayed.store(true, Ordering::SeqCst);
    let provider = ReadyIdentityProvider::new(state.clone());
    let pending = tokio::spawn(async move { provider.resolve(Some(&id("12"))).await });
    mock.arrived.notified().await;
    let next = state.begin_session().await;
    next.set_identity(identity("newer")).await;
    next.set_status(ConnectionStatus::Connected).await;
    mock.resume.notify_one();
    assert_eq!(pending.await.unwrap(), Err(IdentityError::Unavailable));
    assert_eq!(state.identity().await.unwrap().user_id, None);
    server.abort();
}

#[tokio::test]
async fn bad_ids_mismatch_transport_and_disconnect_fail_closed() {
    let (mock, state, server) = fixture().await;
    let provider = ReadyIdentityProvider::new(state.clone());
    for value in [
        json!({"success":true,"data":{"agentId":"12","ownerUserId":3,"teamId":1}}),
        json!({"success":true,"data":{"agentId":0,"ownerUserId":3,"teamId":1}}),
        json!({"success":true,"data":{"agentId":13,"ownerUserId":3,"teamId":1}}),
    ] {
        let next = state.begin_session().await;
        next.set_identity(identity("validation-case")).await;
        next.set_status(ConnectionStatus::Connected).await;
        *mock.response.lock().await = (StatusCode::OK, value);
        assert!(provider.resolve(Some(&id("12"))).await.is_err());
    }
    *mock.response.lock().await = (StatusCode::SERVICE_UNAVAILABLE, json!({}));
    let next = state.begin_session().await;
    next.set_identity(identity("transport-case")).await;
    next.set_status(ConnectionStatus::Connected).await;
    assert_eq!(
        provider.resolve(None).await,
        Err(IdentityError::Unavailable)
    );
    let before = mock.calls.load(Ordering::SeqCst);
    state
        .begin_session()
        .await
        .set_status(ConnectionStatus::Disconnected)
        .await;
    assert_eq!(
        provider.resolve(None).await,
        Err(IdentityError::Unavailable)
    );
    assert_eq!(mock.calls.load(Ordering::SeqCst), before);
    server.abort();
}

#[tokio::test]
async fn context_deadline_and_size_limit_do_not_publish_identity() {
    let (mock, state, server) = fixture().await;
    let provider = ReadyIdentityProvider::new(state.clone());
    let mut oversized = body();
    oversized["padding"] = json!("x".repeat(256 * 1024));
    *mock.response.lock().await = (StatusCode::OK, oversized);
    assert_eq!(
        provider.resolve(None).await,
        Err(IdentityError::Unavailable)
    );
    assert_eq!(state.identity().await.unwrap().user_id, None);
    *mock.response.lock().await = (StatusCode::OK, body());
    mock.delayed.store(true, Ordering::SeqCst);
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(4), provider.resolve(None))
            .await
            .unwrap(),
        Err(IdentityError::Unavailable)
    );
    assert_eq!(state.identity().await.unwrap().user_id, None);
    mock.resume.notify_one();
    server.abort();
}

#[tokio::test]
async fn context_redirect_never_forwards_the_agent_credential() {
    let count = Arc::new(AtomicUsize::new(0));
    let trap = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let location = format!("http://{}/leak", trap.local_addr().unwrap());
    let captured = count.clone();
    let trap_task = tokio::spawn(async move {
        while trap.accept().await.is_ok() {
            captured.fetch_add(1, Ordering::SeqCst);
        }
    });
    let source = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", source.local_addr().unwrap());
    let app = Router::new().route(
        "/api/v1/agents/me",
        get(move || async move { (StatusCode::FOUND, [("location", location)]) }),
    );
    let source_task = tokio::spawn(async move { axum::serve(source, app).await.unwrap() });
    assert!(matches!(
        ContextClient::new(&url, "synthetic-agent-token")
            .unwrap()
            .get()
            .await,
        Err(IdentityError::Unavailable)
    ));
    assert_eq!(count.load(Ordering::SeqCst), 0);
    source_task.abort();
    trap_task.abort();
}
