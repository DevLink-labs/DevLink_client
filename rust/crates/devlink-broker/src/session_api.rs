//! MCP 호환 로컬 세션 HTTP 엔드포인트 핸들러 (/v1/sessions/*)
//!
//! - MCP 클라이언트(AI 에이전트 도구 등)와의 세션 수명주기(claim, bind, heartbeat, release)를 처리합니다.
//! - CLI 관리용 controlToken은 접근 권한이 없으며, 오직 MCP용 localToken으로만 인증합니다.
//! - 브라우저를 통한 악의적 호출을 막기 위해 Origin 헤더가 존재하면 403으로 거부합니다.

use crate::sessions::{ClaimMode, Identifier, IdentityError, SessionError, SessionManager};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

/// 세션 요청 바디 최대 허용 크기 (128 KiB)
const REQUEST_LIMIT: usize = 128 * 1024;

/// 세션 API 핸들러 상태
#[derive(Clone)]
struct Api {
    /// MCP 인증용 로컬 Bearer 토큰
    token: Arc<String>,
    /// 세션 수명주기 및 레지스트리 관리자
    sessions: SessionManager,
}

/// 로컬 세션 API 라우터 등록
pub fn router(token: Arc<String>, sessions: SessionManager) -> Router {
    Router::new()
        .route("/v1/sessions", post(claim))
        .route("/v1/sessions/{id}/bind", post(bind))
        .route("/v1/sessions/{id}/heartbeat", post(heartbeat))
        .route("/v1/sessions/{id}", delete(release))
        .with_state(Api { token, sessions })
}

/// POST /v1/sessions 요청 바디
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaimRequest {
    /// 클라이언트가 보낸 요청 고유 식별자 (응답 시 에코)
    request_id: Identifier,
    /// 클라이언트(Host/MCP) 프로세스 인스턴스 식별자
    instance_id: Identifier,
    /// 세션 획득 모드 (fresh: 신규 대체 / recover: 기존 세션 복구)
    mode: ClaimMode,
    /// 선택적 대상 Agent 식별자
    #[serde(default, deserialize_with = "crate::sessions::present")]
    agent_id: Option<Identifier>,
}

/// POST /v1/sessions/:id/bind 요청 바디
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BindRequest {
    request_id: Identifier,
    instance_id: Identifier,
    /// 바인딩할 검증된 Agent 식별자
    agent_id: Identifier,
}

/// POST /v1/sessions/:id/heartbeat 및 DELETE /v1/sessions/:id 요청 바디
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseRequest {
    request_id: Identifier,
    instance_id: Identifier,
}

/// MCP 표준 에러 포맷 생성 ({ requestId, error: { code, scope, outcome } })
fn failure(id: &str, status: StatusCode, code: &str, scope: &str) -> Response {
    (status, Json(json!({ "requestId": id, "error": { "code": code, "scope": scope, "outcome": if scope == "transport" { "unknown" } else { "rejected" } } }))).into_response()
}

/// 유효성 검증 실패 에러 응답
fn validation(id: &str, status: StatusCode) -> Response {
    failure(id, status, "VALIDATION_FAILED", "domain")
}

/// SessionError를 MCP 표준 HTTP 상태 코드 및 에러 코드로 변환
pub(crate) fn session_failure(id: &str, error: SessionError) -> Response {
    let (status, code, scope) = session_error(error);
    failure(id, status, code, scope)
}

fn control_session_failure(id: &str, error: SessionError) -> Response {
    // MCP reserves control-route HTTP 403 for terminal local credential errors.
    // Keep the existing business-route status mapping unchanged.
    if matches!(error, SessionError::Identity(IdentityError::Forbidden)) {
        return failure(id, StatusCode::UNPROCESSABLE_ENTITY, "FORBIDDEN", "domain");
    }
    session_failure(id, error)
}

pub(crate) fn session_error(error: SessionError) -> (StatusCode, &'static str, &'static str) {
    match error {
        SessionError::Expired => (StatusCode::CONFLICT, "SESSION_EXPIRED", "session"),
        SessionError::Replaced => (StatusCode::CONFLICT, "SESSION_REPLACED", "session"),
        SessionError::Conflict => (StatusCode::CONFLICT, "SESSION_CONFLICT", "session"),
        SessionError::AgentUnregistered => (StatusCode::CONFLICT, "AGENT_UNREGISTERED", "session"),
        SessionError::Identity(IdentityError::Unauthenticated) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "CENTRAL_UNAUTHENTICATED",
            "domain",
        ),
        SessionError::Identity(IdentityError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "CENTRAL_UNAVAILABLE",
            "transport",
        ),
        SessionError::Identity(IdentityError::Forbidden) => {
            (StatusCode::FORBIDDEN, "FORBIDDEN", "domain")
        }
    }
}

/// 공통 HTTP 요청 파싱 및 인증/보안 헤더 검증
pub(crate) async fn read_authenticated<T: serde::de::DeserializeOwned>(
    token: &str,
    request: Request,
) -> Result<T, Box<Response>> {
    let fallback_id = uuid::Uuid::new_v4().to_string();
    // 1. 브라우저 CSRF 차단: Origin 헤더가 존재하면 403 거부
    if request.headers().contains_key("origin") {
        return Err(Box::new(failure(
            &fallback_id,
            StatusCode::FORBIDDEN,
            "LOCAL_AUTH_FORBIDDEN",
            "local_auth",
        )));
    }
    // 2. Bearer 토큰 검증: MCP용 localToken과 일치하는지 확인
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.strip_prefix("Bearer ") == Some(token));
    if !authorized {
        return Err(Box::new(failure(
            &fallback_id,
            StatusCode::UNAUTHORIZED,
            "BROKER_UNAUTHORIZED",
            "local_auth",
        )));
    }
    // 3. 미디어 타입 검증: application/json 확인
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok());
    if !content_type.is_some_and(|v| {
        v.split(';')
            .next()
            .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
    }) {
        return Err(Box::new(validation(
            &fallback_id,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )));
    }
    // 4. 요청 바디 크기 제한(128KiB) 및 타임아웃(5초) 읽기
    let bytes = match tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(request.into_body(), REQUEST_LIMIT),
    )
    .await
    {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) => {
            return Err(Box::new(validation(
                &fallback_id,
                StatusCode::PAYLOAD_TOO_LARGE,
            )));
        }
        Err(_) => {
            return Err(Box::new(validation(
                &fallback_id,
                StatusCode::REQUEST_TIMEOUT,
            )));
        }
    };
    // 5. 검증된 requestId만 에코 응답에 사용 (비정상 페이로드는 원문 유출 방지)
    let request_id = serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get("requestId").cloned())
        .and_then(|v| serde_json::from_value::<Identifier>(v).ok());
    serde_json::from_slice(&bytes).map_err(|_| {
        Box::new(validation(
            request_id.as_ref().map_or(&fallback_id, Identifier::as_str),
            StatusCode::BAD_REQUEST,
        ))
    })
}

/// POST /v1/sessions: 신규 세션 발급 또는 기존 세션 복구
async fn claim(State(api): State<Api>, request: Request) -> Response {
    let body: ClaimRequest = match read_authenticated(&api.token, request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    match api
        .sessions
        .claim(body.instance_id, body.mode, body.agent_id)
        .await
    {
        Ok(session) => {
            Json(json!({ "requestId": body.request_id, "data": session })).into_response()
        }
        Err(error) => control_session_failure(body.request_id.as_str(), error),
    }
}

/// POST /v1/sessions/:id/bind: 세션에 Agent 식별자 바인딩 (활성화 및 중앙 ONLINE 전파)
async fn bind(State(api): State<Api>, Path(id): Path<String>, request: Request) -> Response {
    let body: BindRequest = match read_authenticated(&api.token, request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let id = match Identifier::parse(id) {
        Ok(id) => id,
        Err(_) => return validation(body.request_id.as_str(), StatusCode::BAD_REQUEST),
    };
    match api
        .sessions
        .bind(&id, &body.instance_id, body.agent_id)
        .await
    {
        Ok(session) => {
            Json(json!({ "requestId": body.request_id, "data": session })).into_response()
        }
        Err(error) => control_session_failure(body.request_id.as_str(), error),
    }
}

/// POST /v1/sessions/:id/heartbeat: 세션 임대 기간(TTL 90초) 갱신
async fn heartbeat(State(api): State<Api>, Path(id): Path<String>, request: Request) -> Response {
    let body: LeaseRequest = match read_authenticated(&api.token, request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let id = match Identifier::parse(id) {
        Ok(id) => id,
        Err(_) => return validation(body.request_id.as_str(), StatusCode::BAD_REQUEST),
    };
    match api.sessions.heartbeat(&id, &body.instance_id).await {
        Ok(()) => Json(json!({ "requestId": body.request_id, "data": {} })).into_response(),
        Err(error) => control_session_failure(body.request_id.as_str(), error),
    }
}

/// DELETE /v1/sessions/:id: 세션 명시적 해제 (소유권 정리 및 중앙 OFFLINE 전파)
async fn release(State(api): State<Api>, Path(id): Path<String>, request: Request) -> Response {
    let body: LeaseRequest = match read_authenticated(&api.token, request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let id = match Identifier::parse(id) {
        Ok(id) => id,
        Err(_) => return validation(body.request_id.as_str(), StatusCode::BAD_REQUEST),
    };
    match api.sessions.release(&id, &body.instance_id).await {
        Ok(()) => Json(json!({ "requestId": body.request_id, "data": {} })).into_response(),
        Err(error) => control_session_failure(body.request_id.as_str(), error),
    }
}
