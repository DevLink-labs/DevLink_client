use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use devlink_broker::{
    agent_context::ContextClient,
    central::{CentralIdentity, CentralState, ConnectionStatus},
    protocol::PresenceStatus,
    sessions::{ClaimMode, Identifier, ReadyIdentityProvider, SessionError, SessionManager},
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Notify, RwLock},
    task::JoinHandle,
    time::timeout,
};

#[derive(Clone, Default)]
struct Remote {
    calls: Arc<AtomicUsize>,
    unavailable: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    arrived: Arc<Notify>,
    resume: Arc<Notify>,
}

async fn context(State(remote): State<Remote>) -> (StatusCode, Json<Value>) {
    remote.calls.fetch_add(1, Ordering::SeqCst);
    let unavailable = remote.unavailable.load(Ordering::SeqCst);
    if remote.paused.load(Ordering::SeqCst) {
        remote.arrived.notify_one();
        remote.resume.notified().await;
    }
    if unavailable {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"success":false})),
        )
    } else {
        (
            StatusCode::OK,
            Json(json!({"success":true,"data":{"agentId":12,"ownerUserId":3,"teamId":1}})),
        )
    }
}

struct Fixture {
    remote: Remote,
    central: CentralState,
    manager: SessionManager,
    server: JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let remote = Remote::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("ws://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/v1/agents/me", get(context))
            .with_state(remote.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let central = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
            .with_context_client(
                ContextClient::new(&origin, "synthetic-session-validation-token").unwrap(),
            );
        let manager = SessionManager::new(
            Arc::new(ReadyIdentityProvider::new(central.clone())),
            central.clone(),
        );
        let fixture = Self {
            remote,
            central,
            manager,
            server,
        };
        fixture.legacy_ready("initial").await;
        fixture
    }

    async fn legacy_ready(&self, connection: &str) {
        let handle = self.central.begin_session().await;
        handle
            .set_identity(CentralIdentity {
                connection_id: connection.into(),
                agent_id: 12,
                user_id: None,
                team_id: None,
            })
            .await;
        handle.set_status(ConnectionStatus::Connected).await;
    }

    fn calls(&self) -> usize {
        self.remote.calls.load(Ordering::SeqCst)
    }

    async fn claim(&self, instance: &str) -> devlink_broker::sessions::Session {
        self.manager
            .claim(id(instance), ClaimMode::Fresh, Some(id("12")))
            .await
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn id(value: &str) -> Identifier {
    Identifier::parse(value).unwrap()
}

#[tokio::test]
async fn authorize_hydrates_legacy_reconnect_without_waiting_for_heartbeat() {
    let f = Fixture::new().await;
    let session = f.claim("owner").await;
    f.legacy_ready("reconnected").await;
    let before = f.calls();
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Offline));
    assert_eq!(
        f.manager
            .authorize(&session.session_id, &session.principal)
            .await
            .unwrap(),
        session.principal
    );
    assert_eq!(f.calls(), before + 1);
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Online));
    // The task path calls authorize twice. A confirmed context does not need
    // another REST read, nor does domain authorization renew a local lease.
    f.manager
        .authorize(&session.session_id, &session.principal)
        .await
        .unwrap();
    assert_eq!(f.calls(), before + 1);
}

#[tokio::test]
async fn stale_heartbeat_errors_win_over_central_failure_without_rest_calls() {
    let f = Fixture::new().await;
    let old = f.claim("old").await;
    let current = f.claim("current").await;
    let before = f.calls();
    f.remote.unavailable.store(true, Ordering::SeqCst);
    assert_eq!(
        f.manager.heartbeat(&old.session_id, &id("old")).await,
        Err(SessionError::Replaced)
    );
    assert_eq!(
        f.manager
            .heartbeat(&current.session_id, &id("foreign"))
            .await,
        Err(SessionError::Conflict)
    );
    f.manager
        .release(&current.session_id, &id("current"))
        .await
        .unwrap();
    assert_eq!(
        f.manager
            .heartbeat(&current.session_id, &id("current"))
            .await,
        Err(SessionError::Expired)
    );
    assert_eq!(
        f.manager.heartbeat(&id("missing"), &id("current")).await,
        Err(SessionError::Expired)
    );
    let connection = f.central.begin_session().await;
    connection.set_status(ConnectionStatus::Reconnecting).await;
    assert_eq!(
        f.manager.heartbeat(&old.session_id, &id("old")).await,
        Err(SessionError::Replaced)
    );
    assert_eq!(f.calls(), before);
}

#[tokio::test]
async fn authorize_rejects_local_spoof_or_released_session_before_legacy_lookup() {
    let f = Fixture::new().await;
    let session = f.claim("owner").await;
    f.legacy_ready("reconnected").await;
    let before = f.calls();
    let mut spoofed = session.principal.clone();
    spoofed.user_id = id("999");
    assert_eq!(
        f.manager.authorize(&session.session_id, &spoofed).await,
        Err(SessionError::Identity(
            devlink_broker::sessions::IdentityError::Forbidden
        ))
    );
    f.manager
        .release(&session.session_id, &id("owner"))
        .await
        .unwrap();
    assert_eq!(
        f.manager
            .authorize(&session.session_id, &session.principal)
            .await,
        Err(SessionError::Expired)
    );
    assert_eq!(f.calls(), before);
}

#[tokio::test]
async fn authorize_rechecks_release_after_legacy_identity_await() {
    let f = Fixture::new().await;
    let session = f.claim("owner").await;
    f.legacy_ready("reconnected").await;
    f.remote.paused.store(true, Ordering::SeqCst);
    let manager = f.manager.clone();
    let original = session.clone();
    let pending = tokio::spawn(async move {
        manager
            .authorize(&original.session_id, &original.principal)
            .await
    });
    timeout(Duration::from_secs(2), f.remote.arrived.notified())
        .await
        .unwrap();
    // This must complete while the REST response is paused: the registry lock
    // cannot span the network request.
    timeout(
        Duration::from_secs(1),
        f.manager.release(&session.session_id, &id("owner")),
    )
    .await
    .unwrap()
    .unwrap();
    f.remote.resume.notify_one();
    assert_eq!(pending.await.unwrap(), Err(SessionError::Expired));
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Offline));
}

#[tokio::test]
async fn authorize_rechecks_fresh_replacement_after_legacy_identity_await() {
    let f = Fixture::new().await;
    let session = f.claim("old").await;
    f.legacy_ready("reconnected").await;
    f.remote.paused.store(true, Ordering::SeqCst);
    let manager = f.manager.clone();
    let original = session.clone();
    let pending = tokio::spawn(async move {
        manager
            .authorize(&original.session_id, &original.principal)
            .await
    });
    timeout(Duration::from_secs(2), f.remote.arrived.notified())
        .await
        .unwrap();
    f.remote.paused.store(false, Ordering::SeqCst);
    let winner = timeout(Duration::from_secs(1), f.claim("winner"))
        .await
        .unwrap();
    f.remote.resume.notify_one();
    assert_eq!(pending.await.unwrap(), Err(SessionError::Replaced));
    f.manager
        .heartbeat(&winner.session_id, &id("winner"))
        .await
        .unwrap();
}

#[tokio::test]
async fn heartbeat_rechecks_release_after_live_identity_await() {
    let f = Fixture::new().await;
    let session = f.claim("owner").await;
    f.remote.paused.store(true, Ordering::SeqCst);
    let manager = f.manager.clone();
    let original = session.clone();
    let pending =
        tokio::spawn(async move { manager.heartbeat(&original.session_id, &id("owner")).await });
    timeout(Duration::from_secs(2), f.remote.arrived.notified())
        .await
        .unwrap();
    timeout(
        Duration::from_secs(1),
        f.manager.release(&session.session_id, &id("owner")),
    )
    .await
    .unwrap()
    .unwrap();
    f.remote.resume.notify_one();
    assert_eq!(pending.await.unwrap(), Err(SessionError::Expired));
    assert_eq!(f.central.presence().await, Some(PresenceStatus::Offline));
}
