//! # DevLink Local Broker
//!
//! 로컬 환경에서 백그라운드로 실행되는 핵심 브로커 데몬 모듈입니다.
//! - 로컬 제어용 HTTP 서버(Axum)를 구동하여 상태 조회(health) 및 정상 종료(shutdown) 요청을 처리합니다.
//! - 중앙 웹소켓 서버(central)와의 연결 세션을 백그라운드 태스크로 관리합니다.

use crate::{
    central::{self, CentralState, ConnectionStatus},
    config::{self, Config, state_directory},
    local::{self, API_VERSION, Health, Runtime, SERVICE},
    sessions::{ReadyIdentityProvider, SessionManager},
};
use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{fs::File, path::Path, sync::Arc, time::Duration};
use tokio::{
    sync::RwLock,
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

mod ports;
pub use ports::PortFailure;
pub(crate) use ports::startup_allowance as port_startup_allowance;

/// 로컬 HTTP API 핸들러에서 공유하는 상태
#[derive(Clone)]
struct ApiState {
    /// 브로커 런타임 정보 (PID, 포트, 인증 토큰 등)
    runtime: Arc<Runtime>,
    /// 중앙 연결과 로컬 세션 핸들러가 함께 사용하는 상태·presence 게시 지점
    central: CentralState,
    /// 프로세스 전체에 종료 시그널을 전파하는 토큰
    cancel: CancellationToken,
    /// MCP 클라이언트용 로컬 인증 토큰
    mcp_token: Arc<String>,
}

/// 브로커 종료 시 런타임 파일(runtime.json)을 자동으로 정리(삭제)하는 RAII 가드
struct RuntimeGuard(std::path::PathBuf);
struct InboxGuard(crate::inbox::Inbox);
struct NotificationGuard(crate::notifications::Notifications);
struct TaskMessageGuard(crate::task_messages::TaskMessages);
impl Drop for TaskMessageGuard {
    fn drop(&mut self) {
        self.0.stop_accepting();
    }
}
impl Drop for NotificationGuard {
    fn drop(&mut self) {
        self.0.stop_accepting();
    }
}
impl Drop for InboxGuard {
    fn drop(&mut self) {
        self.0.stop_accepting();
    }
}
impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 브로커의 메인 실행 루프
/// - 설정 로드 및 환경변수(DEVLINK_TOKEN) 검증
/// - 로컬 루프백 TCP 리스너 바인딩 및 런타임 메타데이터 등록
/// - 중앙 서버 연동 태스크 실행 및 Axum 기반 제어 HTTP 서버 구동
pub async fn run(path: &Path) -> Result<()> {
    // 설정 로드 및 환경변수 토큰 확인
    let config = Config::load(path)?;
    // 중복 실행 방지를 위한 파일 락 획득 (broker.lock)
    let _lock = startup_lock(path).await?;
    let prepared = crate::agent_profile::prepare_broker(path).await?;
    let (token, _owner_guard) = match prepared {
        Some(identity) => (identity.token, Some(identity._owner)),
        None => (central_token()?, None),
    };

    // 로컬 제어용 루프백(127.0.0.1) TCP 포트 바인딩
    let listener = ports::bind(&config).await?;
    let bound_port = listener.local_addr()?.port();
    let broker_url = format!("http://127.0.0.1:{bound_port}");

    // MCP 자격 스냅샷 발행 또는 기존 토큰 재사용
    let mcp_creds = crate::credentials::publish(path, &broker_url)?;
    let mcp_token = Arc::new(mcp_creds.local_token);
    let inbox = crate::inbox::Inbox::open(path)?;
    let inbox_guard = InboxGuard(inbox.clone());
    let notifications = crate::notifications::Notifications::open(path)?;
    let notification_guard = NotificationGuard(notifications.clone());
    let task_messages = crate::task_messages::TaskMessages::open(path)?;
    let task_message_guard = TaskMessageGuard(task_messages.clone());
    // 작업 생성 요청 영속화 및 멱등성 로그 초기화
    let task_requests = crate::task_requests::TaskRequests::open(path)?;
    // 중앙 서버 Task API 전송을 위한 HTTP 클라이언트 초기화 (중앙 토큰 주입)
    let task_client = crate::task_client::TaskClient::new(&config.server_url, &token)?;

    // 현재 실행 인스턴스 정보 생성 및 파일로 게시(publish)
    let runtime = Arc::new(Runtime {
        api_version: API_VERSION,
        pid: std::process::id(),
        port: bound_port,
        instance_id: uuid::Uuid::new_v4().to_string(),
        control_token: uuid::Uuid::new_v4().to_string(),
    });
    local::publish(path, &runtime)?;
    let _runtime_guard = RuntimeGuard(Runtime::path(path));

    // 취소 토큰 및 중앙 서버 연결 상태 초기화
    let cancel = CancellationToken::new();
    // Git 수집기는 이 캐시를 복제해 작성자 판별에 사용
    let git_identity = crate::git_identity::IdentityCache::default();
    let git_identity_sync = tokio::spawn(crate::git_identity::run(
        crate::git_identity::IdentityClient::new(&config.server_url, &token)?,
        git_identity.clone(),
        cancel.clone(),
    ));
    let status = Arc::new(RwLock::new(ConnectionStatus::Connecting));
    // 중앙 재연결(READY)마다 보낸 작업의 놓친 최종 상태를 다시 조회하도록 깨우는 신호
    let sent_task_wake = Arc::new(tokio::sync::Notify::new());
    let central_state = CentralState::new(status)
        .with_inbox(inbox)
        .with_notifications(notifications.clone())
        .with_sent_task_wake(sent_task_wake.clone())
        .with_task_messages(task_messages.clone())
        .with_context_client(crate::agent_context::ContextClient::new(
            &config.server_url,
            &token,
        )?);
    let sessions = SessionManager::new(
        Arc::new(ReadyIdentityProvider::new(central_state.clone())),
        central_state.clone(),
    );
    let expiration = tokio::spawn(sessions.clone().run_expiration(cancel.clone()));
    let context_reader = crate::context_api::ContextReader::new(
        central_state.clone(),
        sessions.clone(),
        path.to_path_buf(),
    )?;

    // 로컬 업데이트 조회 API (/v1/updates): 저장된 수신 작업·요청자 알림만 읽음
    let updates = crate::updates::router(
        mcp_token.clone(),
        sessions.clone(),
        path.to_path_buf(),
        &config.server_url,
        cancel.clone(),
    )?;

    // 질문·답변 도착 정보 pull 동기화 (READY·알림 신호와 안전망 주기)
    let message_sync = tokio::spawn(crate::task_messages::run(
        task_messages,
        central_state.clone(),
        task_client.clone(),
        config.server_url.clone(),
        Duration::from_millis(config.task_message_sync_interval_ms),
        cancel.clone(),
    ));
    let dependency_sync = if config.dependency_remote.is_empty() {
        None
    } else {
        crate::agent_profile::inspection_binding(path)
            .ok()
            .flatten()
            .map(|binding| {
                tokio::spawn(crate::dependency_graph::run(
                    binding,
                    config.dependency_remote.clone(),
                    central_state.clone(),
                    task_client.clone(),
                    cancel.clone(),
                ))
            })
    };
    // Git 근거 수집: 작업 파일 (IDX-3-02, 3초 확인·30초 재전송·find_agent 직전),
    // 커밋 이력 (IDX-3-01, 시작 시·60초). gitCollection=false이면 둘 다 시작하지 않는다.
    let working_files = crate::working_files::WorkingFileRefresher::default();
    let git_remote = if config.dependency_remote.is_empty() {
        crate::working_files::DEFAULT_REMOTE.to_owned()
    } else {
        config.dependency_remote.clone()
    };
    let git_binding = if config.git_collection {
        crate::agent_profile::inspection_binding(path)
            .ok()
            .flatten()
    } else {
        eprintln!(
            "Git collection is off (gitCollection=false): working files and commit history are not uploaded."
        );
        None
    };
    let working_sync = git_binding.clone().map(|binding| {
        tokio::spawn(crate::working_files::run(
            binding,
            git_remote.clone(),
            central_state.clone(),
            task_client.clone(),
            working_files.clone(),
            cancel.clone(),
        ))
    });
    let commit_sync = git_binding.map(|binding| {
        tokio::spawn(crate::commit_sync::run(
            path.to_path_buf(),
            binding,
            git_remote.clone(),
            central_state.clone(),
            task_client.clone(),
            git_identity.clone(),
            cancel.clone(),
        ))
    });

    // 보낸 작업 기록: 최종 상태 재조회(시작 후 READY·재연결 READY·지연 판정 직전)와
    // 로컬 지연 알림 판정(taskOverdueMinutes, 0이면 끔)에 쓴다.
    // With the check off (0), tasks are recorded already settled for recovery only, so
    // enabling the check later does not flag old tasks at once.
    let ledger = crate::sent_tasks::Ledger::new(path, &config.server_url)?;
    let sent_tasks = Some(if config.task_overdue_minutes > 0 {
        ledger
    } else {
        ledger.without_overdue()
    });
    let recovery = crate::sent_task_recovery::Recovery::new(
        task_client.clone(),
        central_state.clone(),
        notifications,
        &config.server_url,
        sent_task_wake,
    )?;
    tokio::spawn(crate::sent_tasks::run(
        path.to_path_buf(),
        config.task_overdue_minutes,
        Some(recovery),
        cancel.clone(),
    ));

    // 중앙 서버와 통신하는 비동기 태스크 시작
    let central = tokio::spawn(central::run_with_state(
        config,
        token,
        central_state.clone(),
        cancel.clone(),
    ));

    // 로컬 제어 API 라우터 설정 (/health, /shutdown)
    let app = Router::new()
        .route("/health", get(health))
        .route("/shutdown", post(shutdown))
        .fallback(not_found)
        .with_state(ApiState {
            runtime: runtime.clone(),
            central: central_state,
            cancel: cancel.clone(),
            mcp_token: mcp_token.clone(),
        })
        // 세션 관리 API 라우터 병합 (/v1/sessions/*)
        .merge(crate::context_api::router(
            mcp_token.clone(),
            context_reader.clone(),
        ))
        .merge(crate::connection_api::router(
            mcp_token.clone(),
            broker_url.clone(),
            context_reader.clone(),
        ))
        .merge(crate::connection_api::setup_router(
            mcp_token.clone(),
            broker_url,
            context_reader.clone(),
            runtime,
        ))
        .merge(crate::session_api::router(
            mcp_token.clone(),
            sessions.clone(),
        ))
        // 작업 생성 API 라우터 병합 (/v1/tasks/create)
        .merge(crate::agent_api::router_with_working_files(
            mcp_token.clone(),
            sessions.clone(),
            task_client.clone(),
            cancel.clone(),
            context_reader,
            working_files,
        ))
        .merge(crate::task_api::router(
            mcp_token.clone(),
            sessions.clone(),
            task_client.clone(),
            task_requests,
            cancel.clone(),
            sent_tasks,
        ))
        .merge(crate::team_context_routes::router(
            mcp_token.clone(),
            sessions.clone(),
            task_client.clone(),
            cancel.clone(),
        ))
        .merge(crate::task_routes::router(
            mcp_token,
            sessions,
            task_client,
            cancel.clone(),
        ))
        .merge(updates);

    // Graceful Shutdown 처리 (취소 토큰 수신 또는 OS 종료 시그널 감지 시)
    let shutdown_token = cancel.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        tokio::select! {
            _ = shutdown_token.cancelled() => {},
            _ = termination_signal() => shutdown_token.cancel()
        }
    });

    // 클라이언트가 HTTP 연결을 계속 유지하더라도 안전하게 타임아웃(3초) 내에 종료되도록 제한
    let server = std::future::IntoFuture::into_future(server);
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => result,
        _ = cancel.cancelled() => {
            match timeout(Duration::from_secs(3), &mut server).await {
                Ok(result) => result,
                Err(_) => Ok(()),
            }
        }
    };

    // 중앙 서버 태스크 종료 요청 및 대기 (최대 2초 후 강제 취소)
    cancel.cancel();
    for task in [dependency_sync, working_sync, commit_sync]
        .into_iter()
        .flatten()
    {
        let abort = task.abort_handle();
        if timeout(Duration::from_secs(1), task).await.is_err() {
            abort.abort();
        }
    }
    let abort_git_identity = git_identity_sync.abort_handle();
    if timeout(Duration::from_secs(1), git_identity_sync)
        .await
        .is_err()
    {
        abort_git_identity.abort();
        git_identity.clear().await;
    }
    inbox_guard.0.stop_accepting();
    notification_guard.0.stop_accepting();
    task_message_guard.0.stop_accepting();
    let abort_sync = message_sync.abort_handle();
    if timeout(Duration::from_secs(1), message_sync).await.is_err() {
        abort_sync.abort();
    }
    let abort = central.abort_handle();
    if timeout(Duration::from_secs(2), central).await.is_err() {
        abort.abort();
    }
    let abort_expiration = expiration.abort_handle();
    if timeout(Duration::from_secs(1), expiration).await.is_err() {
        abort_expiration.abort();
    }
    result.context("Broker HTTP server failed")
}

/// A status probe may briefly own broker.lock while cleaning stale discovery.
/// Retry only contention; unsafe paths and I/O errors still fail immediately.
async fn startup_lock(path: &Path) -> Result<File> {
    let lock = state_directory(path).join("broker.lock");
    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        if let Some(guard) = config::try_lock(&lock)? {
            return Ok(guard);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "Another DevLink operation is running"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

/// 환경변수 `DEVLINK_TOKEN`에서 중앙 서버 인증 토큰을 읽어와 유효성을 검증합니다.
pub fn central_token() -> Result<String> {
    let token = std::env::var("DEVLINK_TOKEN")
        .map_err(|_| anyhow::anyhow!("Set DEVLINK_TOKEN before starting the broker"))?;
    anyhow::ensure!(
        !token.trim().is_empty() && token.len() <= 8192,
        "DEVLINK_TOKEN must be nonempty and at most 8192 bytes"
    );
    Ok(token)
}

pub fn central_token_for(config_path: &Path) -> Result<String> {
    match crate::agent_profile::stored_token(config_path)? {
        Some(token) => Ok(token),
        None => central_token(),
    }
}

/// 로컬 HTTP 제어(shutdown) 요청의 Authorization 헤더와 Origin을 검증합니다.
/// - 웹 브라우저에서의 무단 요청(CSRF 등)을 차단하기 위해 `Origin` 헤더가 있으면 거부합니다.
/// - shutdown 권한은 오직 CLI 런타임 제어 토큰(control_token)에만 부여되며, MCP local_token은 거부됩니다.
fn authorized_control(headers: &HeaderMap, state: &ApiState) -> bool {
    if headers.contains_key("origin") {
        return false;
    }
    headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .is_some_and(|header| {
            header.strip_prefix("Bearer ") == Some(state.runtime.control_token.as_str())
        })
}

/// 상태 조회(health) 요청의 Authorization 헤더와 Origin을 검증합니다.
/// - 웹 브라우저에서의 무단 요청(CSRF 등)을 차단하기 위해 `Origin` 헤더가 있으면 거부합니다.
/// - CLI control_token 및 MCP local_token 모두 조회가 허용됩니다.
fn authorized_health(headers: &HeaderMap, state: &ApiState) -> bool {
    if headers.contains_key("origin") {
        return false;
    }
    headers
        .get("authorization")
        .and_then(|header| header.to_str().ok())
        .is_some_and(|header| {
            let token = match header.strip_prefix("Bearer ") {
                Some(t) => t,
                None => return false,
            };
            token == state.runtime.control_token.as_str() || token == state.mcp_token.as_str()
        })
}

/// 표준 API 에러 응답 JSON을 생성합니다.
fn error(code: StatusCode, message: &'static str) -> Response {
    (
        code,
        Json(serde_json::json!({ "apiVersion": API_VERSION, "error": { "code": code.as_u16(), "message": message } })),
    )
        .into_response()
}

/// `GET /health` 핸들러: 브로커의 프로세스 정보 및 중앙 서버 연결 상태 반환
async fn health(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if !authorized_health(&headers, &state) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    Json(Health {
        notification_rejections: state.central.notification_rejections(),
        inbox: state.central.inbox_diagnostics(),
        task_messages: state.central.task_message_diagnostics(),
        service: SERVICE.into(),
        api_version: API_VERSION,
        pid: state.runtime.pid,
        instance_id: state.runtime.instance_id.clone(),
        central_server: state.central.status().await,
    })
    .into_response()
}

/// `POST /shutdown` 핸들러: 브로커 정상 종료 요청 처리 (오직 CLI control_token만 허용)
async fn shutdown(State(state): State<ApiState>, headers: HeaderMap) -> Response {
    if !authorized_control(&headers, &state) {
        return error(StatusCode::UNAUTHORIZED, "Unauthorized");
    }
    state.cancel.cancel();
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "apiVersion": API_VERSION, "status": "stopping" })),
    )
        .into_response()
}

/// 등록되지 않은 엔드포인트 요청에 대한 404 핸들러
async fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "Unknown endpoint")
}

/// OS 레벨의 프로세스 종료 시그널(Ctrl+C 또는 SIGTERM)을 대기합니다.
async fn termination_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn startup_lock_waits_for_a_short_probe_then_acquires() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        config::init(&path).unwrap();
        let probe = config::lock(&state_directory(&path).join("broker.lock")).unwrap();
        let pending = startup_lock(&path);
        tokio::pin!(pending);
        assert!(
            timeout(Duration::from_millis(30), &mut pending)
                .await
                .is_err()
        );
        drop(probe);
        let acquired = pending.await.unwrap();
        assert!(
            config::try_lock(&state_directory(&path).join("broker.lock"))
                .unwrap()
                .is_none()
        );
        drop(acquired);
    }

    #[tokio::test(start_paused = true)]
    async fn startup_lock_stays_bounded_for_persistent_contention() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        config::init(&path).unwrap();
        let _probe = config::lock(&state_directory(&path).join("broker.lock")).unwrap();
        let began = Instant::now();
        assert_eq!(
            startup_lock(&path).await.err().unwrap().to_string(),
            "Another DevLink operation is running"
        );
        assert_eq!(began.elapsed(), Duration::from_millis(200));
    }

    #[tokio::test(start_paused = true)]
    async fn startup_lock_does_not_retry_unsafe_paths() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        config::init(&path).unwrap();
        std::fs::create_dir(state_directory(&path).join("broker.lock")).unwrap();
        let began = Instant::now();
        assert!(startup_lock(&path).await.is_err());
        assert_eq!(began.elapsed(), Duration::ZERO);
    }

    #[test]
    fn runtime_guard_preserves_config_named_like_runtime_metadata() {
        for name in [
            "runtime.json",
            "Runtime.JSON",
            "broker.lock",
            "control.lock",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(name);
            config::init(&path).unwrap();
            let before = std::fs::read(&path).unwrap();
            let broker_lock = config::lock(&state_directory(&path).join("broker.lock")).unwrap();
            let runtime = Runtime {
                api_version: API_VERSION,
                pid: std::process::id(),
                port: 43127,
                instance_id: "test-instance".into(),
                control_token: "test-control-token".into(),
            };
            local::publish(&path, &runtime).unwrap();
            assert!(local::running(&path).unwrap());
            drop(RuntimeGuard(Runtime::path(&path)));
            assert!(!Runtime::path(&path).exists());
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(state_directory(&path).join("broker.lock").exists());
            drop(broker_lock);
        }
    }
}
