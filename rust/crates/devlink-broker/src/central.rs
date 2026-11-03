//! # DevLink Central WebSocket Client
//!
//! 중앙 서버(DevLink Server)와의 WebSocket 연결, 인증, `BROKER_READY` 수신,
//! 확인된 로컬 Agent 접속 상태(`AGENT_PRESENCE`) 보고, 하트비트 및 지수 백오프 기반 재연결을 담당하는 모듈입니다.

use crate::{
    config::Config,
    inbox::Inbox,
    protocol::{
        ClientMessage, HeartbeatPong, MAX_CENTRAL_MESSAGE_BYTES, PresenceStatus, ServerMessage,
        TaskAckPayload, TaskCreatedMessage,
    },
    sessions::IdentityError,
};
use anyhow::{Result, bail, ensure};
use futures_util::{Sink, SinkExt, StreamExt, stream::FuturesUnordered};
use serde::{Deserialize, Serialize};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    sync::{RwLock, watch},
    time::{Instant, sleep, timeout},
};
use tokio_tungstenite::tungstenite::{
    Error as WsError, Message,
    client::IntoClientRequest,
    http::{HeaderValue, Request, StatusCode, header::AUTHORIZATION},
    protocol::WebSocketConfig,
};
pub use tokio_util::sync::CancellationToken;

/// 운영 중앙의 40초 PING 무수신 제한 안에 전송하며 응답은 최대 20초 대기합니다.
/// 테스트는 같은 연결 루프에 짧은 시간을 주입해 실제 소켓에서 만료를 재현합니다.
#[derive(Clone, Copy)]
struct HeartbeatTiming {
    interval: Duration,
    pong_timeout: Duration,
}

const HEARTBEAT_TIMING: HeartbeatTiming = HeartbeatTiming {
    interval: Duration::from_secs(20),
    pong_timeout: Duration::from_secs(20),
};

/// 중앙 웹소켓 서버와의 연결 상태
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionStatus {
    /// 웹소켓 TCP/TLS 및 HTTP Upgrade 핸드셰이크 진행 중
    Connecting,
    /// Upgrade 성공 후 서버의 유효한 `BROKER_READY` 응답 대기 중
    Authenticating,
    /// 유효한 `BROKER_READY` 수신 완료 후 정상 연결 유지 상태
    /// (확인된 로컬 Agent 상태가 있는 경우 초기 `AGENT_PRESENCE` 전송까지 완료된 상태)
    Connected,
    /// 일시적 연결 해제 또는 타임아웃 후 재연결 시도 중
    Reconnecting,
    /// Upgrade HTTP 401/403 인증 거부 (자동 재시도 중단)
    AuthFailed,
    /// Close 4002: 동일 Agent의 새 연결로 교체됨 (자동 재시도 중단)
    Replaced,
    /// Close 1008 또는 잘못된 `BROKER_READY` 등 메시지/정책 위반 (자동 재시도 중단)
    ProtocolViolation,
    /// 로컬 수신함 영속화 실패 (디스크 오류 등; 재시작 필요)
    InboxFailed,
    /// 정상/강제 종료 또는 `auto_reconnect=false` 상태에서 연결 해제
    Disconnected,
}

/// 현재 활성 중앙 WebSocket 연결에 귀속되는 식별 정보 (`BROKER_READY` 수신 시 설정, 연결 종료/교체 시 무효화)
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CentralIdentity {
    /// 현재 중앙 서버 WebSocket 세션 식별자
    pub connection_id: String,
    /// 인증된 Agent의 숫자 식별자
    pub agent_id: u64,
    pub user_id: Option<u64>,
    pub team_id: Option<u64>,
}

/// 스레드/비동기 태스크 간 안전하게 공유되는 연결 상태 타입
pub type SharedStatus = Arc<RwLock<ConnectionStatus>>;

/// 현재 활성 연결의 식별 정보(`connection_id`, `agent_id`) 공유 타입
pub type SharedIdentity = Arc<RwLock<Option<CentralIdentity>>>;

/// 로컬 Agent의 현재 접속 상태 공유 타입
/// - `None`: 로컬 Agent 상태 미확인 (추측하여 `ONLINE`/`OFFLINE`을 보고하지 않음)
/// - `Some(PresenceStatus::Offline)`: 로컬 Agent 연결이 없다고 확인된 상태
/// - `Some(PresenceStatus::Online)`: 로컬 Agent 연결이 있다고 확인된 상태
pub type SharedPresence = watch::Sender<Option<PresenceStatus>>;

#[derive(Debug)]
struct StateInner {
    generation: u64,
    status: ConnectionStatus,
    identity: Option<CentralIdentity>,
    subject: Option<(u64, u64, u64)>,
    subject_epoch: u64,
}

/// 세대(generation) 번호를 통해 이전 연결의 지연 정리나 타이머가
/// 새 연결의 상태 및 식별 정보(`connection_id`, `agent_id`)를 덮어쓰지 못하도록 보호하는 상태 관리 구조체
#[derive(Clone)]
pub struct CentralState {
    pub(crate) context_client: Option<crate::agent_context::ContextClient>,
    inner: Arc<RwLock<StateInner>>,
    status: SharedStatus,
    identity: SharedIdentity,
    presence: SharedPresence,
    inbox: Option<Inbox>,
    notifications: Option<crate::notifications::Notifications>,
    subject_changes: watch::Sender<u64>,
    auth_failures: watch::Sender<u64>,
}

impl CentralState {
    /// 기본 상태 저장소를 생성합니다.
    /// 2단계에서는 로컬 Agent 상태 감지 및 `/v1/sessions/*` API가 아직 연결되지 않았으므로
    /// 로컬 Agent 상태를 추측하지 않고 미확인(`None`)으로 초기화합니다.
    pub fn new(status: SharedStatus) -> Self {
        Self::with_details(status, Arc::new(RwLock::new(None)), watch::channel(None).0)
    }

    /// 외부에서 상태·식별자·확인된 로컬 Agent 접속 상태(`Option<PresenceStatus>`)를 주입하여 생성합니다.
    pub fn with_details(
        status: SharedStatus,
        identity: SharedIdentity,
        presence: SharedPresence,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(StateInner {
                generation: 0,
                status: ConnectionStatus::Connecting,
                identity: None,
                subject: None,
                subject_epoch: 0,
            })),
            status,
            identity,
            presence,
            inbox: None,
            notifications: None,
            context_client: None,
            subject_changes: watch::channel(0).0,
            auth_failures: watch::channel(0).0,
        }
    }

    pub fn with_inbox(mut self, inbox: Inbox) -> Self {
        self.inbox = Some(inbox);
        self
    }

    pub fn with_notifications(
        mut self,
        notifications: crate::notifications::Notifications,
    ) -> Self {
        self.notifications = Some(notifications);
        self
    }

    pub fn notification_rejections(&self) -> Option<u64> {
        self.notifications
            .as_ref()
            .map(crate::notifications::Notifications::rejected)
    }

    pub fn with_context_client(mut self, client: crate::agent_context::ContextClient) -> Self {
        self.context_client = Some(client);
        self
    }

    pub(crate) async fn context_snapshot(
        &self,
    ) -> (u64, ConnectionStatus, Option<CentralIdentity>) {
        let guard = self.inner.read().await;
        (guard.generation, guard.status, guard.identity.clone())
    }

    pub(crate) async fn confirm_context(
        &self,
        generation: u64,
        expected: &CentralIdentity,
        context: Option<&crate::agent_context::AgentContext>,
    ) -> Result<(), IdentityError> {
        let mut guard = self.inner.write().await;
        if guard.generation != generation || guard.status != ConnectionStatus::Connected {
            return Err(IdentityError::Unavailable);
        }
        let Some(current) = guard.identity.as_ref() else {
            return Err(IdentityError::Unavailable);
        };
        if current.connection_id != expected.connection_id || current.agent_id != expected.agent_id
        {
            return Err(IdentityError::Unavailable);
        }
        // Hydration by another request is not a connection change. However,
        // already verified fields must agree with every concurrent success.
        let rejection = match context {
            None => Some(IdentityError::Unauthenticated),
            Some(context)
                if context.agent_id != current.agent_id
                    || current
                        .user_id
                        .is_some_and(|id| id != context.owner_user_id)
                    || current.team_id.is_some_and(|id| id != context.team_id) =>
            {
                Some(IdentityError::Forbidden)
            }
            Some(_) => None,
        };
        if let Some(error) = rejection {
            guard.subject = None;
            guard.subject_epoch = guard.subject_epoch.saturating_add(1);
            guard.identity = None;
            *self.identity.write().await = None;
            guard.status = ConnectionStatus::AuthFailed;
            *self.status.write().await = ConnectionStatus::AuthFailed;
            if self.presence.borrow().is_some() {
                self.set_presence(PresenceStatus::Offline);
            }
            self.subject_changes.send_replace(guard.subject_epoch);
            self.auth_failures.send_replace(generation);
            return Err(error);
        }
        let context = context.expect("accepted context");
        let mut identity = current.clone();
        identity.user_id = Some(context.owner_user_id);
        identity.team_id = Some(context.team_id);
        let subject = identity
            .user_id
            .zip(identity.team_id)
            .map(|(u, t)| (u, t, identity.agent_id));
        if guard.subject != subject {
            guard.subject = subject;
            guard.subject_epoch = guard.subject_epoch.saturating_add(1);
            if self.presence.borrow().is_some() {
                self.set_presence(PresenceStatus::Offline);
            }
            self.subject_changes.send_replace(guard.subject_epoch);
        }
        guard.identity = Some(identity.clone());
        *self.identity.write().await = Some(identity);
        Ok(())
    }

    pub fn inbox_diagnostics(&self) -> Option<crate::inbox::InboxDiagnostics> {
        self.inbox.as_ref().map(Inbox::diagnostics)
    }

    /// 새 연결 세션을 시작하며 세대(generation)를 증가시키고 이전 연결의 식별 정보를 즉시 무효화합니다.
    pub async fn begin_session(&self) -> SessionHandle {
        let mut guard = self.inner.write().await;
        guard.generation = guard.generation.saturating_add(1);
        guard.identity = None;
        *self.identity.write().await = None;
        SessionHandle {
            state: self.clone(),
            generation: guard.generation,
        }
    }

    /// 현재 연결 상태를 조회합니다.
    pub async fn status(&self) -> ConnectionStatus {
        *self.status.read().await
    }

    /// 현재 활성 연결의 식별 정보를 조회합니다.
    pub async fn identity(&self) -> Option<CentralIdentity> {
        self.identity.read().await.clone()
    }

    /// Apply a local identity-dependent mutation while central identity cannot change.
    pub(crate) async fn with_ready_context<T>(
        &self,
        operation: impl FnOnce(ConnectionStatus, Option<&CentralIdentity>, u64) -> T,
    ) -> T {
        let guard = self.inner.read().await;
        operation(guard.status, guard.identity.as_ref(), guard.subject_epoch)
    }

    pub(crate) fn subject_changes(&self) -> watch::Receiver<u64> {
        self.subject_changes.subscribe()
    }

    /// 현재 확인된 로컬 Agent 접속 상태를 조회합니다 (`None`이면 미확인).
    pub async fn presence(&self) -> Option<PresenceStatus> {
        *self.presence.borrow()
    }

    /// 로컬 세션 관리가 확인한 현재 상태를 게시합니다.
    /// 동일 상태는 재전송하지 않으며, 연결 전·재접속 중 갱신도 다음 READY까지 보존합니다.
    pub fn set_presence(&self, status: PresenceStatus) {
        self.presence.send_if_modified(|current| {
            if *current == Some(status) {
                return false;
            }
            *current = Some(status);
            true
        });
    }
}

/// 특정 연결 세대(generation)에 귀속된 상태 변경 핸들.
/// 더 새로운 연결 세대가 시작된 이후에는 이전 핸들의 모든 상태/식별자 변경 요청이 무시됩니다.
#[derive(Clone)]
pub struct SessionHandle {
    state: CentralState,
    generation: u64,
}

impl SessionHandle {
    async fn finish_connection(&self, outcome: Outcome) -> Outcome {
        let mut guard = self.state.inner.write().await;
        if guard.generation != self.generation {
            return Outcome::Stopped;
        }
        // Classify and clear under one lock. Otherwise a context denial between
        // the status check and clear could be overwritten by Reconnecting.
        let outcome = if guard.status == ConnectionStatus::AuthFailed {
            Outcome::AuthFailed
        } else {
            outcome
        };
        guard.identity = None;
        *self.state.identity.write().await = None;
        outcome
    }
    /// 세대 번호를 반환합니다.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 현재 핸들이 최신 연결 세대인 경우에만 연결 상태를 갱신합니다.
    pub async fn set_status(&self, next: ConnectionStatus) -> bool {
        let mut guard = self.state.inner.write().await;
        if guard.generation != self.generation {
            return false;
        }
        guard.status = next;
        *self.state.status.write().await = next;
        true
    }

    /// 현재 핸들이 최신 연결 세대인 경우에만 `BROKER_READY` 식별 정보를 기록합니다.
    pub async fn set_identity(&self, identity: CentralIdentity) -> bool {
        let mut guard = self.state.inner.write().await;
        if guard.generation != self.generation {
            return false;
        }
        let subject = identity
            .user_id
            .zip(identity.team_id)
            .map(|(user, team)| (user, team, identity.agent_id));
        let same_legacy_agent = subject.is_none()
            && self.state.context_client.is_some()
            && guard
                .subject
                .is_some_and(|(_, _, agent)| agent == identity.agent_id);
        // Missing user/team can preserve ownership only for the same Agent.
        // An authenticated A -> B -> A READY sequence must fence A even when
        // no REST context request happens while B is connected.
        if guard.subject != subject && !same_legacy_agent {
            guard.subject = subject;
            guard.subject_epoch = guard.subject_epoch.saturating_add(1);
            // Never advertise old local ownership for a different authenticated subject.
            if self.state.presence.borrow().is_some() {
                self.state.set_presence(PresenceStatus::Offline);
            }
            self.state.subject_changes.send_replace(guard.subject_epoch);
        }
        if subject.is_none()
            && self.state.context_client.is_some()
            && self.state.presence.borrow().is_some()
        {
            self.state.set_presence(PresenceStatus::Offline);
        }
        guard.identity = Some(identity.clone());
        *self.state.identity.write().await = Some(identity);
        true
    }

    /// 현재 핸들이 최신 연결 세대인 경우에만 식별 정보를 무효화합니다.
    pub async fn clear_identity(&self) -> bool {
        let mut guard = self.state.inner.write().await;
        if guard.generation != self.generation {
            return false;
        }
        guard.identity = None;
        *self.state.identity.write().await = None;
        true
    }

    /// 현재 상태가 `Reconnecting`이 아니면 `Connecting`으로 전환합니다.
    pub async fn mark_connecting(&self) -> bool {
        let mut guard = self.state.inner.write().await;
        if guard.generation != self.generation {
            return false;
        }
        if guard.status != ConnectionStatus::Reconnecting {
            guard.status = ConnectionStatus::Connecting;
            *self.state.status.write().await = ConnectionStatus::Connecting;
        }
        true
    }

    /// 현재 확인된 로컬 Agent의 접속 상태(`Some(Online)` / `Some(Offline)` / 미확인 `None`)를 읽습니다.
    pub async fn current_presence(&self) -> Option<PresenceStatus> {
        *self.state.presence.borrow()
    }
}

/// 단일 웹소켓 세션 종료 결과
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// 재연결 필요 (일시적 네트워크 단절, `BROKER_READY` 대기 타임아웃 등)
    Retry {
        /// 해당 세션에서 유효한 `BROKER_READY`를 받아 `Connected`에 도달했었는지 여부
        was_connected: bool,
    },
    /// 인증 거부로 인한 영구 중단 (HTTP 401/403)
    AuthFailed,
    /// 동일 Agent의 새 연결로 교체되어 영구 중단 (Close 4002)
    Replaced,
    /// 메시지/정책 위반 또는 잘못된 `BROKER_READY` 수신으로 인한 영구 중단 (Close 1008 등)
    ProtocolViolation,
    /// 수신함(Inbox) 디스크 저장 실패로 인한 중단
    InboxFailed,
    /// 브로커 종료 시그널(cancel)에 의한 정상 종료
    Stopped,
}

/// 설정된 `server_url`을 중앙 브로커 엔드포인트(`/ws/broker`) URL로 검증·정규화합니다.
/// - 루트 경로(`""` 또는 `"/"`)인 경우 `"/ws/broker"`로 정규화합니다.
/// - 명시적 `"/ws/broker"` 경로는 그대로 허용합니다.
/// - 그 외 임의 경로는 허용하지 않고 오류로 거부하여 다른 경로로 Bearer 토큰이 전송되지 않게 합니다.
/// - URL 쿼리, 프래그먼트, 사용자 자격 증명은 허용하지 않습니다.
pub fn endpoint_url(server_url: &str) -> Result<String> {
    let mut url = url::Url::parse(server_url).map_err(|_| anyhow::anyhow!("Invalid serverUrl"))?;
    ensure!(
        matches!(url.scheme(), "ws" | "wss") && url.has_host(),
        "serverUrl must use ws:// or wss://"
    );
    ensure!(
        url.username().is_empty() && url.password().is_none(),
        "serverUrl must not include credentials"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "serverUrl must not include query parameters or fragments"
    );
    match url.path() {
        "" | "/" => url.set_path("/ws/broker"),
        "/ws/broker" => {}
        _ => bail!("serverUrl path must be empty, '/', or '/ws/broker'"),
    }
    Ok(url.into())
}

/// `/ws/broker` WebSocket Upgrade 요청을 생성하고 `Authorization: Bearer <Agent 토큰>` 헤더를 설정합니다.
/// - 토큰은 오직 `/ws/broker` 경로의 `Authorization` 헤더로만 전달하며 URL 쿼리나 쿠키로 전달하지 않습니다.
/// - 임의 `Origin`이나 `Cookie` 헤더를 추가하지 않습니다.
/// - `HeaderValue::set_sensitive(true)`를 설정해 디버그 출력 시 헤더 값이 노출되지 않도록 보호합니다.
pub fn build_upgrade_request(server_url: &str, token: &str) -> Result<Request<()>> {
    let trimmed = token.trim();
    ensure!(
        !trimmed.is_empty() && token.len() <= 8192,
        "DEVLINK_TOKEN must be nonempty and at most 8192 bytes"
    );
    let normalized = endpoint_url(server_url)?;
    let mut request = normalized
        .as_str()
        .into_client_request()
        .map_err(|_| anyhow::anyhow!("Cannot build WebSocket upgrade request"))?;

    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| anyhow::anyhow!("DEVLINK_TOKEN contains invalid HTTP header characters"))?;
    bearer.set_sensitive(true);

    request.headers_mut().remove("origin");
    request.headers_mut().remove("cookie");
    request.headers_mut().insert(AUTHORIZATION, bearer);
    Ok(request)
}

/// 중앙 서버 연결 관리의 최상위 루프
/// - 설정된 `auto_reconnect` 옵션 및 지수 백오프(exponential backoff)에 따라 재연결을 관리합니다.
pub async fn run(config: Config, token: String, status: SharedStatus, cancel: CancellationToken) {
    let state = CentralState::new(status);
    run_with_state(config, token, state, cancel).await;
}

/// 상태/식별자/로컬 Agent 접속 상태 저장소를 지정하여 중앙 서버 연결 루프를 실행합니다.
pub async fn run_with_state(
    config: Config,
    token: String,
    state: CentralState,
    cancel: CancellationToken,
) {
    run_with_heartbeat(config, token, state, cancel, HEARTBEAT_TIMING).await;
}

async fn run_with_heartbeat(
    config: Config,
    token: String,
    state: CentralState,
    cancel: CancellationToken,
    heartbeat_timing: HeartbeatTiming,
) {
    let mut delay = config.reconnect_initial_ms;
    loop {
        let session_handle = state.begin_session().await;
        let outcome = session(&config, &token, &session_handle, &cancel, heartbeat_timing).await;
        // An I/O failure racing with revocation must not enable reconnect.
        let outcome = session_handle.finish_connection(outcome).await;

        match outcome {
            Outcome::Stopped => {
                session_handle
                    .set_status(ConnectionStatus::Disconnected)
                    .await;
                return;
            }
            Outcome::AuthFailed => {
                session_handle
                    .set_status(ConnectionStatus::AuthFailed)
                    .await;
                return;
            }
            Outcome::Replaced => {
                session_handle.set_status(ConnectionStatus::Replaced).await;
                return;
            }
            Outcome::ProtocolViolation => {
                session_handle
                    .set_status(ConnectionStatus::ProtocolViolation)
                    .await;
                return;
            }
            Outcome::InboxFailed => {
                session_handle
                    .set_status(ConnectionStatus::InboxFailed)
                    .await;
                return;
            }
            Outcome::Retry { was_connected } => {
                if was_connected {
                    delay = config.reconnect_initial_ms;
                }
                if !config.auto_reconnect {
                    session_handle
                        .set_status(ConnectionStatus::Disconnected)
                        .await;
                    return;
                }
            }
        }

        session_handle
            .set_status(ConnectionStatus::Reconnecting)
            .await;

        // 취소 토큰 감지 또는 지수 백오프 지연 대기
        tokio::select! {
            _ = cancel.cancelled() => {
                session_handle.set_status(ConnectionStatus::Disconnected).await;
                return;
            }
            _ = sleep(Duration::from_millis(delay)) => {}
        }

        // 지수 백오프: 재연결 지연 시간을 2배씩 늘려 최대 reconnect_max_ms까지 제한
        delay = delay.saturating_mul(2).min(config.reconnect_max_ms);
    }
}

/// WebSocket Close 코드를 종료 결과(`Outcome`)로 분류합니다.
fn classify_close_code(code: u16, was_connected: bool) -> Outcome {
    match code {
        // 중앙의 4001은 하트비트 만료입니다. 인증 실패와 달리 재접속 가능합니다.
        4001 => Outcome::Retry { was_connected },
        4002 => Outcome::Replaced,
        1008 => Outcome::ProtocolViolation,
        _ => Outcome::Retry { was_connected },
    }
}

/// READY 직후와 상태 변경 보고에 동일한 전송 제한·종료 처리를 적용합니다.
async fn send_presence<S: Sink<Message> + Unpin>(
    socket: &mut S,
    status: PresenceStatus,
    cancel: &CancellationToken,
    was_connected: bool,
) -> Result<(), Outcome> {
    let text = serde_json::to_string(&ClientMessage::agent_presence(status))
        .expect("serializable AGENT_PRESENCE protocol");
    tokio::select! {
        _ = cancel.cancelled() => {
            let _ = timeout(Duration::from_secs(1), socket.send(Message::Close(None))).await;
            Err(Outcome::Stopped)
        }
        sent = timeout(Duration::from_secs(2), socket.send(Message::Text(text.into()))) => {
            if matches!(sent, Ok(Ok(()))) {
                Ok(())
            } else {
                Err(Outcome::Retry { was_connected })
            }
        }
    }
}

/// 단일 WebSocket 연결 세션을 수립하고 `BROKER_READY` 확인, 확인된 `AGENT_PRESENCE` 보고 및 메시지 루프를 실행합니다.
type Receipt = Pin<Box<dyn Future<Output = Result<(u64, Vec<TaskAckPayload>)>> + Send>>;
fn receipt(inbox: Inbox, origin: String, agent: u64, messages: Vec<TaskCreatedMessage>) -> Receipt {
    Box::pin(async move {
        inbox
            .record_batch(origin, agent, messages)
            .await
            .map(|acks| (agent, acks))
    })
}

async fn session(
    config: &Config,
    token: &str,
    handle: &SessionHandle,
    cancel: &CancellationToken,
    heartbeat_timing: HeartbeatTiming,
) -> Outcome {
    // 연결 전부터 구독해 READY 수신과 구독 사이의 상태 변경을 놓치지 않습니다.
    let mut presence = handle.state.presence.subscribe();
    let mut auth_failures = handle.state.auth_failures.subscribe();
    let mut last_presence = None;
    if !handle.mark_connecting().await {
        return Outcome::Stopped;
    }

    let Ok(request) = build_upgrade_request(&config.server_url, token) else {
        return Outcome::ProtocolViolation;
    };

    // 웹소켓 프레임 및 메시지 최대 크기 제한 (64KB)
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(MAX_CENTRAL_MESSAGE_BYTES))
        .max_frame_size(Some(MAX_CENTRAL_MESSAGE_BYTES));

    // 중앙 서버에 비동기 웹소켓 Upgrade 연결 (5초 타임아웃)
    let connection = tokio::select! {
        _ = cancel.cancelled() => return Outcome::Stopped,
        connection = timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async_with_config(request, Some(ws_config), false),
        ) => connection,
    };

    let mut socket = match connection {
        Ok(Ok((socket, _response))) => socket,
        Ok(Err(WsError::Http(response)))
            if matches!(
                response.status(),
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) =>
        {
            return Outcome::AuthFailed;
        }
        Ok(Err(_)) | Err(_) => {
            return Outcome::Retry {
                was_connected: false,
            };
        }
    };

    // Upgrade 직후에는 아직 연결 완료(Connected)로 전환하지 않고 BROKER_READY를 대기
    if !handle.set_status(ConnectionStatus::Authenticating).await {
        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
        return Outcome::Stopped;
    }

    // BROKER_READY 응답 대기 타임아웃 설정 (config.auth_timeout_ms 재사용)
    let ready_deadline = sleep(Duration::from_millis(config.auth_timeout_ms));
    tokio::pin!(ready_deadline);

    let mut heartbeat = tokio::time::interval(heartbeat_timing.interval);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let pong_deadline = sleep(heartbeat_timing.pong_timeout);
    tokio::pin!(pong_deadline);
    let mut pending_ping: Option<String> = None;
    let mut ready = false;
    // 디스크 저장은 백그라운드 태스크에서 진행되어 PING/PONG 등 소켓 루프를 차단하지 않습니다.
    // 세션이 끊기면 미완료된 ACK future는 폐기되며, 완료된 디스크 기록은 안전하게 보존되어 새 연결에서 처리됩니다.
    let mut receipts: FuturesUnordered<Receipt> = FuturesUnordered::new();
    let mut pending = Vec::new();
    let batch_deadline = sleep(Duration::from_millis(5));
    tokio::pin!(batch_deadline);

    loop {
        tokio::select! {
            biased;
            changed = auth_failures.changed() => {
                if changed.is_err() { return Outcome::Stopped; }
                if *auth_failures.borrow_and_update() == handle.generation {
                    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                    return Outcome::AuthFailed;
                }
            }
            // 수신함 디스크 저장이 완료되면 중앙 WebSocket으로 ACK 회신
            saved = receipts.next(), if !receipts.is_empty() => {
                let Some(Ok((agent, acknowledgements))) = saved else {

                    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                    return Outcome::InboxFailed;
                };
                for ack in acknowledgements {
                    // Fence each send against a newer generation/identity, including during I/O.
                    let current = handle.state.inner.read().await;
                    if current.generation != handle.generation {
                        return Outcome::Stopped;
                    }
                    if current.status != ConnectionStatus::Connected || current.identity.as_ref().is_none_or(|id| id.agent_id != agent) {
                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                        return Outcome::AuthFailed;
                    }
                    let text = serde_json::to_string(&ClientMessage::Ack(ack)).expect("serializable task ACK");
                    if !matches!(timeout(Duration::from_secs(1), socket.send(Message::Text(text.into()))).await, Ok(Ok(()))) {
                        return Outcome::Retry { was_connected: ready };
                    }
                }
            }
            _ = &mut batch_deadline, if !pending.is_empty() && receipts.is_empty() => {
                let Some(inbox) = handle.state.inbox.clone() else { return Outcome::InboxFailed; };
                let current = handle.state.inner.read().await;
                if current.generation != handle.generation { return Outcome::Stopped; }
                if current.status != ConnectionStatus::Connected { return Outcome::AuthFailed; }
                let Some(identity) = &current.identity else { return Outcome::ProtocolViolation; };
                receipts.push(receipt(inbox, config.server_url.clone(), identity.agent_id, std::mem::take(&mut pending)));
            }
            // 브로커 종료 요청 시 정상적으로 웹소켓 Close 프레임 전송 후 종료
            _ = cancel.cancelled() => {
                let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                return Outcome::Stopped;
            }

            // BROKER_READY 대기 제한 시간 초과 체크
            _ = &mut ready_deadline, if !ready => {
                let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                return Outcome::Retry {
                    was_connected: false,
                };
            }

            // READY 이후 현재 연결에서 변경된 최신 상태만 전송합니다.
            changed = presence.changed(), if ready => {
                if changed.is_err() {
                    return Outcome::Stopped;
                }
                let current = *presence.borrow_and_update();
                if let Some(current) = current
                    && last_presence != Some(current)
                {
                    if let Err(outcome) = send_presence(&mut socket, current, cancel, true).await {
                        return outcome;
                    }
                    last_presence = Some(current);
                }
            }

            // 다른 JSON·제어 Ping/Pong·잘못된 ping_id는 이 대기 시간을 연장하지 않습니다.
            _ = &mut pong_deadline, if pending_ping.is_some() => {
                let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                return Outcome::Retry { was_connected: true };
            }

            // 응답 대기 중에는 새 PING으로 이전 요청과 제한 시간을 덮어쓰지 않습니다.
            _ = heartbeat.tick(), if ready && pending_ping.is_none() => {
                let ping_id = uuid::Uuid::new_v4().to_string();
                let text = serde_json::to_string(&ClientMessage::heartbeat_ping(ping_id.clone()))
                    .expect("serializable HEARTBEAT PING");
                let sent_at = Instant::now();
                if !matches!(
                    timeout(
                        Duration::from_secs(1),
                        socket.send(Message::Text(text.into())),
                    )
                    .await,
                    Ok(Ok(()))
                ) {
                    return Outcome::Retry {
                        was_connected: true,
                    };
                }
                pending_ping = Some(ping_id);
                pong_deadline.as_mut().reset(sent_at + heartbeat_timing.pong_timeout);
            }

            // 서버로부터 메시지 수신 처리
            message = socket.next() => {
                match message {
                    // 서버에서 Close 프레임 수신
                    Some(Ok(Message::Close(frame))) => {
                        return match frame {
                            Some(frame) => classify_close_code(u16::from(frame.code), ready),
                            None => Outcome::Retry {
                                was_connected: ready,
                            },
                        };
                    }
                    None | Some(Err(_)) => {
                        return Outcome::Retry {
                            was_connected: ready,
                        };
                    }

                    // 텍스트(JSON) 메시지 수신 처리
                    Some(Ok(Message::Text(text))) => {
                        if ready && crate::notifications::TaskNotification::recognizes(&text) {
                            let auth = handle.state.inner.read().await;
                            if auth.generation != handle.generation { return Outcome::Stopped; }
                            if auth.status != ConnectionStatus::Connected { return Outcome::AuthFailed; }
                            let Some(identity) = &auth.identity else { return Outcome::ProtocolViolation; };
                            // These events are sent to the authenticated requester, not the receiver
                            // in TASK_CREATED. Never ACK them or infer a local acceptance.
                            if let Some(store) = handle.state.notifications.clone() {
                                match serde_json::from_str::<crate::notifications::TaskNotification>(&text) {
                                    Ok(event) if event.validate().is_ok() => {
                                        let origin = config.server_url.clone();
                                        let agent = identity.agent_id;
                                        store.enqueue(origin, agent, event);
                                    }
                                    _ => store.reject(),
                                }
                            }
                            continue;
                        }
                        if !ready {
                            match serde_json::from_str::<ServerMessage>(&text) {
                                Ok(ServerMessage::BrokerReady(payload)) if payload.is_valid() => {
                                    let identity = CentralIdentity {
                                        connection_id: payload.connection_id,
                                        agent_id: payload.agent_id,
                                        user_id: payload.user_id,
                                        team_id: payload.team_id,
                                    };
                                    if !handle.set_identity(identity).await {
                                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                        return Outcome::Stopped;
                                    }

                                    // 신뢰할 수 있는 현재 로컬 Agent 상태가 확인된 경우(Some(Online | Offline))에만
                                    // 초기/재연결 BROKER_READY 직후 AGENT_PRESENCE를 1회 전송한다.
                                    // 로컬 Agent 상태가 미확인(None)인 경우 추측하여 ONLINE/OFFLINE을 전송하지 않는다.
                                    let current = *presence.borrow_and_update();
                                    if let Some(current) = current {
                                        if let Err(outcome) = send_presence(&mut socket, current, cancel, false).await {
                                            return outcome;
                                        }
                                        last_presence = Some(current);
                                    }

                                    ready = true;
                                    heartbeat.reset();
                                    if !handle.set_status(ConnectionStatus::Connected).await {
                                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                        return Outcome::Stopped;
                                    }
                                }
                                _ => {
                                    // 유효한 BROKER_READY가 아닌 초기 메시지는 프로토콜/정책 위반으로 처리하고 재시도 중단
                                    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                    return Outcome::ProtocolViolation;
                                }
                            }
                        } else {
                            match serde_json::from_str::<ServerMessage>(&text) {
                                Ok(ServerMessage::Heartbeat(HeartbeatPong::Pong { ping_id })) => {
                                    if pending_ping.as_deref() == Some(ping_id.as_str())
                                        && Instant::now() < pong_deadline.deadline()
                                    {
                                        pending_ping = None;
                                    }
                                }
                                Ok(ServerMessage::BrokerReady(_)) => {
                                    let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                    return Outcome::ProtocolViolation;
                                }
                                Ok(ServerMessage::TaskCreated { event_id, payload }) => {
                                    let message = TaskCreatedMessage { event_id, payload };
                                    let auth = handle.state.inner.read().await;
                                    if auth.generation != handle.generation { return Outcome::Stopped; }
                                    if auth.status != ConnectionStatus::Connected {
                                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                        return Outcome::AuthFailed;
                                    }
                                    let identity = auth.identity.as_ref();

                                    // 1. BROKER_READY 완료 여부 및 수신 Agent 일치 검증
                                    let Some(identity) = identity else { return Outcome::ProtocolViolation; };
                                    if !message.is_valid() || message.payload.receiver_agent_id != identity.agent_id
                                        || identity.team_id.is_some_and(|team| team != message.payload.team_id) {
                                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                        return Outcome::ProtocolViolation;
                                    }
                                    let Some(inbox) = handle.state.inbox.clone() else {
                                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                        return Outcome::InboxFailed;
                                    };
                                    // 2. 동시 쓰기 대기열 상한 (16건 초과 시 재연결 유도)
                                    if pending.len() >= crate::inbox::MAX_BATCH {
                                        inbox.reject_queue();
                                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                        return Outcome::Retry { was_connected: true };
                                    }
                                    // 3. 비동기 디스크 기록 작업 큐에 등록 (소켓 이벤트 루프 블로킹 방지)
                                    if pending.is_empty() { batch_deadline.as_mut().reset(Instant::now() + Duration::from_millis(5)); }
                                    pending.push(message);
                                    if pending.len() == crate::inbox::MAX_BATCH && receipts.is_empty() {
                                        receipts.push(receipt(inbox, config.server_url.clone(), identity.agent_id, std::mem::take(&mut pending)));
                                    }

                                }
                                Err(_) => {
                                    // A recognized task delivery with missing/invalid fields must
                                    // fail visibly instead of disappearing as an unknown message.
                                    // Keep unrelated message types from extending the PONG budget.
                                    if serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|message| {
                                        message.get("type").and_then(serde_json::Value::as_str) == Some("TASK_CREATED")
                                    }) {
                                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                                        return Outcome::ProtocolViolation;
                                    }
                                }
                            }
                        }
                    }

                    // 서버로부터 Binary 프레임이 BROKER_READY 이전에 오면 프로토콜 위반으로 거부
                    Some(Ok(Message::Binary(_))) if !ready => {
                        let _ = timeout(Duration::from_secs(1), socket.close(None)).await;
                        return Outcome::ProtocolViolation;
                    }

                    // 서버로부터 Ping 수신 시 Pong 응답
                    Some(Ok(Message::Ping(payload))) => {
                        if !matches!(
                            timeout(Duration::from_secs(1), socket.send(Message::Pong(payload))).await,
                            Ok(Ok(()))
                        ) {
                            return Outcome::Retry {
                                was_connected: ready,
                            };
                        }
                    }

                    // 제어 Pong은 JSON HEARTBEAT 응답으로 인정하지 않습니다.
                    Some(Ok(Message::Pong(_))) => {}
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{BrokerReadyPayload, HeartbeatPing};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::{
        net::TcpListener,
        sync::Mutex,
        task::{JoinHandle, JoinSet},
    };
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;

    #[tokio::test(start_paused = true)]
    async fn heartbeat_waiting_for_central_lock_cannot_renew_an_expired_lease() {
        use crate::sessions::{
            ClaimMode, Identifier, IdentityFuture, IdentityProvider, Principal, SessionError,
            SessionManager,
        };

        struct GatedIdentity {
            calls: AtomicUsize,
            arrived: tokio::sync::Notify,
            resume: tokio::sync::Notify,
        }

        impl IdentityProvider for GatedIdentity {
            fn requires_current_ready(&self) -> bool {
                true
            }
            fn requires_live_check(&self) -> bool {
                true
            }
            fn resolve<'a>(&'a self, _agent: Option<&'a Identifier>) -> IdentityFuture<'a> {
                Box::pin(async move {
                    if self.calls.fetch_add(1, Ordering::SeqCst) > 0 {
                        self.arrived.notify_one();
                        self.resume.notified().await;
                    }
                    Ok(Principal {
                        user_id: Identifier::parse("3").unwrap(),
                        team_id: Identifier::parse("1").unwrap(),
                        agent_id: Some(Identifier::parse("12").unwrap()),
                        tool_type: None,
                    })
                })
            }
        }

        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        let handle = state.begin_session().await;
        handle
            .set_identity(CentralIdentity {
                connection_id: "lease-lock-regression".into(),
                agent_id: 12,
                user_id: Some(3),
                team_id: Some(1),
            })
            .await;
        handle.set_status(ConnectionStatus::Connected).await;
        let provider = Arc::new(GatedIdentity {
            calls: AtomicUsize::new(0),
            arrived: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let manager = SessionManager::new(provider.clone(), state.clone());
        let owner = Identifier::parse("owner").unwrap();
        let session = manager
            .claim(
                owner.clone(),
                ClaimMode::Fresh,
                Some(Identifier::parse("12").unwrap()),
            )
            .await
            .unwrap();
        tokio::time::advance(Duration::from_secs(85)).await;
        let heartbeat = manager.heartbeat(&session.session_id, &owner);
        tokio::pin!(heartbeat);
        // Manually poll to make both wait boundaries deterministic: the local
        // precheck has finished before identity resolution waits on Notify.
        assert!(futures_util::poll!(&mut heartbeat).is_pending());
        provider.arrived.notified().await;
        let central_write = state.inner.write().await;
        provider.resume.notify_one();
        // Identity has resolved, but final validation is queued on inner.read.
        assert!(futures_util::poll!(&mut heartbeat).is_pending());
        tokio::time::advance(Duration::from_secs(6)).await;
        drop(central_write);
        assert_eq!(heartbeat.await, Err(SessionError::Expired));
        assert_eq!(state.presence().await, Some(PresenceStatus::Offline));
    }

    #[tokio::test]
    async fn stalled_receipt_storage_keeps_heartbeat_alive_without_early_ack() {
        stalled_receipt(false).await;
    }

    #[tokio::test]
    async fn completed_storage_from_an_old_generation_cannot_ack_or_replace_new_identity() {
        stalled_receipt(true).await;
    }

    async fn stalled_receipt(replace_generation: bool) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        let (release, paused) = inbox.pause_writes();
        paused.await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("ws://{}", listener.local_addr().unwrap());
        let config = Config {
            server_url: origin.clone(),
            auto_reconnect: false,
            ..Config::default()
        };
        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
            .with_inbox(inbox.clone());
        let cancel = CancellationToken::new();
        let runner = tokio::spawn(run_with_heartbeat(
            config,
            "synthetic-token".into(),
            state.clone(),
            cancel.clone(),
            HeartbeatTiming {
                interval: Duration::from_millis(40),
                pong_timeout: Duration::from_millis(500),
            },
        ));
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        for message in [
            ServerMessage::BrokerReady(BrokerReadyPayload {
                connection_id: "stalled-write".into(),
                agent_id: 12,
                user_id: None,
                team_id: None,
            }),
            ServerMessage::TaskCreated {
                event_id: "task-event".into(),
                payload: crate::protocol::TaskCreatedPayload {
                    task_id: 101,
                    team_id: 1,
                    requester_agent_id: 10,
                    receiver_agent_id: 12,
                    prompt: "synthetic slow storage".into(),
                    created_at: "2026-10-29T09:00:00".into(),
                },
            },
        ] {
            socket
                .send(Message::Text(
                    serde_json::to_string(&message).unwrap().into(),
                ))
                .await
                .unwrap();
        }
        timeout(Duration::from_secs(3), async {
            let mut pings = 0;
            // Heartbeats alone do not prove TASK_CREATED reached the storage
            // worker. Replace the generation only after its write was accepted.
            while pings < 3 || !inbox.has_pending_write() {
                let Some(Ok(Message::Text(text))) = socket.next().await else {
                    panic!("connection closed")
                };
                match serde_json::from_str::<ClientMessage>(&text).unwrap() {
                    ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }) => {
                        pings += 1;
                        let pong =
                            serde_json::to_string(&ServerMessage::heartbeat_pong(ping_id)).unwrap();
                        socket.send(Message::Text(pong.into())).await.unwrap();
                    }
                    ClientMessage::Ack(_) => panic!("ACK before durable persistence"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert!(!Inbox::path(&path).exists());
        assert_eq!(state.status().await, ConnectionStatus::Connected);
        if replace_generation {
            let next = state.begin_session().await;
            let identity = CentralIdentity {
                connection_id: "new-generation".into(),
                agent_id: 12,
                user_id: None,
                team_id: None,
            };
            assert!(next.set_identity(identity.clone()).await);
            release.send(()).unwrap();
            timeout(Duration::from_secs(5), async {
                while let Some(Ok(frame)) = socket.next().await {
                    match frame {
                        Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text)
                            .unwrap()
                        {
                            ClientMessage::Ack(_) => panic!("old-generation receipt must not ACK"),
                            ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }) => {
                                // Disk/ACL work can outlast the pong timeout under
                                // parallel load. Keep the peer alive until the
                                // completed receipt exercises the generation fence.
                                socket
                                    .send(Message::Text(
                                        serde_json::to_string(&ServerMessage::heartbeat_pong(
                                            ping_id,
                                        ))
                                        .unwrap()
                                        .into(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                            _ => {}
                        },
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
            assert_eq!(state.identity().await, Some(identity));
            assert_eq!(
                Inbox::open(&path)
                    .unwrap()
                    .list_for(&origin, 12)
                    .unwrap()
                    .len(),
                1
            );
            return;
        }
        release.send(()).unwrap();
        timeout(Duration::from_secs(5), async {
            loop {
                let Some(Ok(Message::Text(text))) = socket.next().await else {
                    panic!("connection closed")
                };
                match serde_json::from_str::<ClientMessage>(&text).unwrap() {
                    ClientMessage::Ack(ack) => {
                        assert_eq!(ack.event_id, "task-event");
                        break;
                    }
                    ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }) => {
                        socket
                            .send(Message::Text(
                                serde_json::to_string(&ServerMessage::heartbeat_pong(ping_id))
                                    .unwrap()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                    }
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            Inbox::open(&path)
                .unwrap()
                .list_for(&origin, 12)
                .unwrap()
                .len(),
            1
        );
        cancel.cancel();
        timeout(Duration::from_secs(3), runner)
            .await
            .unwrap()
            .unwrap();
    }

    #[derive(Clone, Copy)]
    enum HeartbeatReply {
        Echo,
        WrongId,
        ReplayFirst,
        ExpireFirstConnection,
    }

    struct HeartbeatFixture {
        state: CentralState,
        cancel: CancellationToken,
        runner: JoinHandle<()>,
        server: JoinHandle<()>,
        attempts: Arc<AtomicUsize>,
        expirations: Arc<AtomicUsize>,
        messages: Arc<Mutex<Vec<(usize, ClientMessage)>>>,
    }

    impl HeartbeatFixture {
        async fn start(reply: HeartbeatReply, auto_reconnect: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let attempts = Arc::new(AtomicUsize::new(0));
            let expirations = Arc::new(AtomicUsize::new(0));
            let messages = Arc::new(Mutex::new(Vec::new()));
            let attempts_copy = attempts.clone();
            let expirations_copy = expirations.clone();
            let messages_copy = messages.clone();
            let server = tokio::spawn(async move {
                let mut clients = JoinSet::new();
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let connection = attempts_copy.fetch_add(1, Ordering::SeqCst) + 1;
                    let messages = messages_copy.clone();
                    let expirations = expirations_copy.clone();
                    clients.spawn(async move {
                        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                        let ready = ServerMessage::BrokerReady(BrokerReadyPayload {
                            connection_id: format!("heartbeat-{connection}"),
                            agent_id: 11,
                            user_id: None, team_id: None,
                        });
                        socket.send(Message::Text(serde_json::to_string(&ready).unwrap().into())).await.unwrap();
                        let expiration = sleep(Duration::from_millis(250));
                        tokio::pin!(expiration);
                        let mut noise = tokio::time::interval(Duration::from_millis(20));
                        let mut first_ping = None;
                        loop {
                            let message = tokio::select! {
                                _ = &mut expiration => {
                                    expirations.fetch_add(1, Ordering::SeqCst);
                                    let _ = socket.close(Some(CloseFrame {
                                        code: 4001.into(),
                                        reason: "Heartbeat timeout".into(),
                                    })).await;
                                    return;
                                }
                                _ = noise.tick(), if matches!(reply, HeartbeatReply::WrongId | HeartbeatReply::ReplayFirst) => {
                                    // 제어 프레임·다른 JSON 수신만으로 클라이언트의 PONG 대기가 연장되면 안 됩니다.
                                    for frame in [
                                        Message::Ping(Vec::new().into()),
                                        Message::Pong(Vec::new().into()),
                                        Message::Text(r#"{"type":"OTHER"}"#.into()),
                                        Message::Text(r#"{"type":"HEARTBEAT","payload":{"kind":"PONG","ping_id":1}}"#.into()),
                                    ] {
                                        if socket.send(frame).await.is_err() { return; }
                                    }
                                    continue;
                                }
                                message = socket.next() => message,
                            };
                            let Some(Ok(message)) = message else { return };
                            match message {
                                Message::Text(text) => {
                                    let message: ClientMessage = serde_json::from_str(&text).unwrap();
                                    messages.lock().await.push((connection, message.clone()));
                                    if let ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }) = message {
                                        if matches!(reply, HeartbeatReply::ExpireFirstConnection) && connection == 1 {
                                            continue;
                                        }
                                        expiration.as_mut().reset(Instant::now() + Duration::from_millis(250));
                                        let first = first_ping.get_or_insert_with(|| ping_id.clone());
                                        let response_id = match reply {
                                            HeartbeatReply::WrongId => "unrelated-id".into(),
                                            HeartbeatReply::ReplayFirst => first.clone(),
                                            _ => ping_id,
                                        };
                                        let pong = serde_json::to_string(&ServerMessage::heartbeat_pong(response_id)).unwrap();
                                        if socket.send(Message::Text(pong.into())).await.is_err() { return; }
                                    }
                                }
                                Message::Ping(payload) => {
                                    if socket.send(Message::Pong(payload)).await.is_err() { return; }
                                }
                                Message::Close(_) => return,
                                _ => {}
                            }
                        }
                    });
                    while let Some(result) = clients.try_join_next() {
                        result.unwrap();
                    }
                }
            });
            let config = Config {
                server_url: format!("ws://{address}"),
                auto_reconnect,
                reconnect_initial_ms: 150,
                reconnect_max_ms: 150,
                ..Config::default()
            };
            let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
            if matches!(reply, HeartbeatReply::ExpireFirstConnection) {
                state.set_presence(PresenceStatus::Online);
            }
            let cancel = CancellationToken::new();
            let timing = HeartbeatTiming {
                interval: Duration::from_millis(40),
                // 서버의 4001을 클라이언트 자체 PONG 만료보다 먼저 관측합니다.
                pong_timeout: Duration::from_millis(
                    if matches!(reply, HeartbeatReply::ExpireFirstConnection) {
                        500
                    } else {
                        120
                    },
                ),
            };
            let runner = tokio::spawn(run_with_heartbeat(
                config,
                "test-heartbeat-token".into(),
                state.clone(),
                cancel.clone(),
                timing,
            ));
            Self {
                state,
                cancel,
                runner,
                server,
                attempts,
                expirations,
                messages,
            }
        }

        async fn wait_status(&self, expected: ConnectionStatus) {
            timeout(Duration::from_secs(3), async {
                while self.state.status().await != expected {
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }

        async fn wait_pings(&self, connection: usize, count: usize) -> Vec<String> {
            timeout(Duration::from_secs(3), async {
                loop {
                    let pings: Vec<_> = self
                        .messages
                        .lock()
                        .await
                        .iter()
                        .filter_map(|(seq, message)| {
                            if *seq == connection
                                && let ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }) =
                                    message
                            {
                                Some(ping_id.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    if pings.len() >= count {
                        return pings;
                    }
                    sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap()
        }

        async fn stop(self) {
            self.cancel.cancel();
            timeout(Duration::from_secs(3), self.runner)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(self.state.status().await, ConnectionStatus::Disconnected);
            assert_eq!(self.state.identity().await, None);
            self.server.abort();
            let _ = self.server.await;
        }
    }

    #[tokio::test]
    async fn json_heartbeat_keeps_connection_alive_past_server_expiration() {
        let f = HeartbeatFixture::start(HeartbeatReply::Echo, true).await;
        let pings = f.wait_pings(1, 8).await;
        let unique: std::collections::HashSet<_> = pings.iter().collect();
        assert_eq!(unique.len(), pings.len());
        assert!(pings.iter().all(|id| uuid::Uuid::parse_str(id).is_ok()));
        assert_eq!(f.state.status().await, ConnectionStatus::Connected);
        assert_eq!(f.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(f.expirations.load(Ordering::SeqCst), 0);
        assert_eq!(f.state.presence().await, None);
        assert!(
            f.messages
                .lock()
                .await
                .iter()
                .all(|(_, message)| matches!(message, ClientMessage::Heartbeat(_)))
        );
        f.stop().await;
    }

    #[tokio::test]
    async fn unmatched_replayed_or_missing_json_pong_expires_despite_other_frames() {
        for (reply, expected_pings) in [
            (HeartbeatReply::WrongId, 1),
            (HeartbeatReply::ReplayFirst, 2),
        ] {
            let f = HeartbeatFixture::start(reply, true).await;
            f.wait_status(ConnectionStatus::Reconnecting).await;
            assert_eq!(f.state.identity().await, None);
            assert_eq!(f.wait_pings(1, expected_pings).await.len(), expected_pings);
            assert_eq!(f.attempts.load(Ordering::SeqCst), 1);
            assert_eq!(f.expirations.load(Ordering::SeqCst), 0);
            f.stop().await;
        }
        let f = HeartbeatFixture::start(HeartbeatReply::WrongId, false).await;
        f.wait_status(ConnectionStatus::Disconnected).await;
        assert_eq!(f.attempts.load(Ordering::SeqCst), 1);
        assert_eq!(f.state.identity().await, None);
        f.stop().await;
    }

    #[tokio::test]
    async fn server_4001_timeout_reconnects_with_new_identity_and_presence() {
        let f = HeartbeatFixture::start(HeartbeatReply::ExpireFirstConnection, true).await;
        let old_ping = f.wait_pings(1, 1).await;
        f.wait_status(ConnectionStatus::Reconnecting).await;
        assert_eq!(f.state.identity().await, None);
        assert_eq!(f.expirations.load(Ordering::SeqCst), 1);
        let new_ping = f.wait_pings(2, 1).await;
        assert_ne!(old_ping[0], new_ping[0]);
        assert_eq!(f.state.status().await, ConnectionStatus::Connected);
        assert_eq!(
            f.state.identity().await.unwrap().connection_id,
            "heartbeat-2"
        );
        for connection in [1, 2] {
            assert!(
                f.messages
                    .lock()
                    .await
                    .iter()
                    .any(|(seq, message)| *seq == connection
                        && *message == ClientMessage::agent_presence(PresenceStatus::Online))
            );
        }
        f.stop().await;
    }

    #[test]
    fn endpoint_url_normalizes_root_and_rejects_non_broker_paths() {
        assert_eq!(
            endpoint_url("ws://localhost:8080").unwrap(),
            "ws://localhost:8080/ws/broker"
        );
        assert_eq!(
            endpoint_url("wss://example.com/").unwrap(),
            "wss://example.com/ws/broker"
        );
        assert_eq!(
            endpoint_url("ws://127.0.0.1:9000/ws/broker").unwrap(),
            "ws://127.0.0.1:9000/ws/broker"
        );
        for invalid in [
            "ws://localhost:8080/custom",
            "ws://localhost:8080/ws/broker/",
            "ws://localhost:8080/ws/broker/extra",
            "ws://localhost:8080/ws",
            "ws://localhost:8080/ws/broker?token=abc",
            "ws://localhost:8080/ws/broker#frag",
        ] {
            assert!(
                endpoint_url(invalid).is_err(),
                "endpoint_url must reject disallowed URL: {invalid}"
            );
        }
    }

    #[test]
    fn upgrade_request_sets_sensitive_bearer_header_without_origin_cookie_or_query() {
        let secret = "test-agent-token-do-not-leak";
        let req = build_upgrade_request("ws://127.0.0.1:8080", secret).unwrap();
        assert_eq!(req.uri().path(), "/ws/broker");
        assert!(req.uri().query().is_none());
        assert!(req.headers().get("origin").is_none());
        assert!(req.headers().get("cookie").is_none());

        let auth = req
            .headers()
            .get(AUTHORIZATION)
            .expect("Authorization header must be present");
        assert!(
            auth.is_sensitive(),
            "Authorization header must be marked sensitive"
        );
        assert_eq!(auth.to_str().unwrap(), format!("Bearer {secret}"));

        let debug_output = format!("{req:?}");
        assert!(
            !debug_output.contains(secret),
            "Debug formatting of upgrade request must not expose token"
        );

        assert!(build_upgrade_request("ws://127.0.0.1:8080/other", secret).is_err());
    }

    #[tokio::test]
    async fn central_state_defaults_to_unknown_local_presence() {
        let status = Arc::new(RwLock::new(ConnectionStatus::Connecting));
        let state = CentralState::new(status);
        assert_eq!(state.presence().await, None);
    }

    #[tokio::test]
    async fn stale_session_handle_cannot_overwrite_newer_session_state_or_identity() {
        let status = Arc::new(RwLock::new(ConnectionStatus::Connecting));
        let state = CentralState::new(status);

        let old_session = state.begin_session().await;
        assert!(
            old_session
                .set_identity(CentralIdentity {
                    connection_id: "conn-old".into(),
                    agent_id: 11,
                    user_id: None,
                    team_id: None,
                })
                .await
        );
        assert!(old_session.set_status(ConnectionStatus::Connected).await);

        // 새 연결 세션 시작 시 이전 세션의 식별 정보는 즉시 무효화됨
        let new_session = state.begin_session().await;
        assert_eq!(state.identity().await, None);
        assert!(
            new_session
                .set_identity(CentralIdentity {
                    connection_id: "conn-new".into(),
                    agent_id: 11,
                    user_id: None,
                    team_id: None,
                })
                .await
        );
        assert!(new_session.set_status(ConnectionStatus::Connected).await);

        // 이전 연결의 지연된 종료/오류 처리가 뒤늦게 실행되어도 새 연결 상태와 식별자를 덮어쓰지 못함
        assert!(!old_session.set_status(ConnectionStatus::Disconnected).await);
        assert!(!old_session.set_status(ConnectionStatus::Replaced).await);
        assert!(!old_session.clear_identity().await);
        assert!(
            !old_session
                .set_identity(CentralIdentity {
                    connection_id: "conn-stale".into(),
                    agent_id: 99,
                    user_id: None,
                    team_id: None,
                })
                .await
        );

        assert_eq!(state.status().await, ConnectionStatus::Connected);
        assert_eq!(
            state.identity().await,
            Some(CentralIdentity {
                connection_id: "conn-new".into(),
                agent_id: 11,
                user_id: None,
                team_id: None,
            })
        );
    }
}
