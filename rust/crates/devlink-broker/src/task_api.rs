//! MCP create_task -> 중앙 서버 인증 기반 작업 생성 API
//!
//! MCP 클라이언트의 작업 생성 요청을 받아 로컬 세션 및 인가를 검증하고,
//! 멱등성을 보장하며 중앙 서버의 Task REST API로 안전하게 전달합니다.

use crate::{
    business_api::{self, BusinessRequest, LOCAL_BUDGET, session_error},
    session_api,
    sessions::{Principal, SessionManager},
    task_client::{CreateTaskCommand, TaskClient, TaskError, numeric_id},
    task_requests::TaskRequests,
};
use axum::{
    Router,
    extract::{Request, State},
    response::Response,
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

/// 작업 생성 API 핸들러의 공유 상태 구조체
#[derive(Clone)]
struct Api {
    token: Arc<String>,
    sessions: SessionManager,
    central: TaskClient,
    requests: TaskRequests,
    cancel: CancellationToken,
}

/// 작업 생성 API 라우터 생성 (`POST /v1/tasks/create`)
pub fn router(
    token: Arc<String>,
    sessions: SessionManager,
    central: TaskClient,
    requests: TaskRequests,
    cancel: CancellationToken,
) -> Router {
    Router::new()
        .route("/v1/tasks/create", post(create))
        .with_state(Api {
            token,
            sessions,
            central,
            requests,
            cancel,
        })
}

type CreateRequest = BusinessRequest<CreateArguments>;

/// 작업 생성 인자 (수신 에이전트, 제목, 설명, 컨텍스트)
#[derive(Deserialize)]
#[serde(untagged)]
enum CreateArguments {
    Canonical(CanonicalArguments),
    Legacy(LegacyArguments),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CanonicalArguments {
    receiver_agent_id: String,
    prompt: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyArguments {
    recipient_agent_id: String,
    title: String,
    description: String,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    task_context: Option<String>,
}

/// 중앙 서버로 전송할 프롬프트 JSON 페이로드
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Prompt<'a> {
    title: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_context: Option<&'a str>,
}

impl CreateArguments {
    fn command(&self, principal: &Principal) -> Result<CreateTaskCommand, TaskError> {
        match self {
            Self::Legacy(args) => args.command(principal),
            Self::Canonical(args) => {
                let command = CreateTaskCommand {
                    request_key: String::new(),
                    requester_agent_id: principal
                        .agent_id
                        .as_ref()
                        .and_then(|id| numeric_id(id.as_str()))
                        .ok_or_else(validation)?,
                    receiver_agent_id: numeric_id(&args.receiver_agent_id)
                        .ok_or_else(validation)?,
                    prompt: args.prompt.clone(),
                };
                if command.prompt.trim().is_empty() || !command.fits_delivery() {
                    return Err(validation());
                }
                Ok(command)
            }
        }
    }
}

impl LegacyArguments {
    /// 입력값 제약 조건을 검증하고 중앙 서버 전송용 CreateTaskCommand를 빌드
    fn command(&self, principal: &Principal) -> Result<CreateTaskCommand, TaskError> {
        let text = |value: &str, limit| !value.trim().is_empty() && value.chars().count() <= limit;
        // 제목 200자, 설명 및 컨텍스트 16,000자 제한
        if !text(&self.title, 200)
            || !text(&self.description, 16_000)
            || self
                .task_context
                .as_deref()
                .is_some_and(|value| !text(value, 16_000))
        {
            return Err(validation());
        }
        let command = CreateTaskCommand {
            request_key: String::new(),
            requester_agent_id: principal
                .agent_id
                .as_ref()
                .and_then(|id| numeric_id(id.as_str()))
                .ok_or_else(validation)?,
            receiver_agent_id: numeric_id(&self.recipient_agent_id).ok_or_else(validation)?,
            // MCP 입력 필드를 중앙 서버 규격의 prompt JSON 문자열로 직렬화
            prompt: serde_json::to_string(&Prompt {
                title: &self.title,
                description: &self.description,
                task_context: self.task_context.as_deref(),
            })
            .map_err(|_| validation())?,
        };
        // 전체 메시지 크기가 중앙 WebSocket 전송 한도(64 KiB) 내인지 검증
        if !command.fits_delivery() {
            return Err(validation());
        }
        Ok(command)
    }
}

/// `POST /v1/tasks/create` 엔드포인트 핸들러
///
/// localToken 인증, 타임아웃 관리 및 응답 포맷 매핑을 수행합니다.
async fn create(State(api): State<Api>, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    // localToken 검증 및 바디 역직렬화
    let body: CreateRequest = match session_api::read_authenticated(&api.token, request).await {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let mut request_key = None;
    // 브로커 종료 신호 또는 8초 타임아웃 제한 내에서 비즈니스 로직 수행
    let result = tokio::select! {
        _ = api.cancel.cancelled() => Err(TaskError::unknown("CENTRAL_UNAVAILABLE")),
        result = timeout_at(deadline, process(&api, &body, &mut request_key)) => {
            result.unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT")))
        }
    };
    business_api::response(&body.request_id, request_key, result)
}

/// 작업 생성의 세부 처리 흐름 (인가 -> 멱등키 준비 -> 동시성 락 -> 재검증 -> 중앙 서버 전송)
async fn process(
    api: &Api,
    body: &CreateRequest,
    request_key: &mut Option<String>,
) -> Result<Value, TaskError> {
    body.validate("create_task")?;
    let expected = body.context.principal();
    // 세션 레지스트리에서 호출자의 활성 세션 상태 및 에이전트 바인딩 인가 확인
    let actor = api
        .sessions
        .authorize(&body.context.session_id, &expected)
        .await
        .map_err(session_error)?;
    let command = body.arguments.command(&actor)?;
    // 멱등성 로그에 기록하고 멱등키(UUID) 발급 및 게이트 획득
    let prepared = match api
        .requests
        .prepare(
            api.central.origin(),
            actor.clone(),
            body.request_id.clone(),
            command,
        )
        .await
    {
        Ok(prepared) => prepared,
        Err(failure) => {
            *request_key = failure.request_key;
            return Err(failure.error);
        }
    };
    *request_key = Some(prepared.command.request_key.clone());
    // 동일 요청에 대한 중복 동시 전송 방지를 위한 게이트 락 획득
    let _gate = prepared.gate.lock().await;
    // 디스크 I/O 또는 다른 호출 대기 시간 동안 세션이 만료되지 않았는지 재확인
    api.sessions
        .authorize(&body.context.session_id, &actor)
        .await
        .map_err(session_error)?;
    let team_id = numeric_id(actor.team_id.as_str()).ok_or_else(validation)?;
    // Caller/credential agreement is checked on the actual Agent provider.
    api.central.confirm_agent(&actor).await?;
    api.sessions
        .authorize(&body.context.session_id, &actor)
        .await
        .map_err(session_error)?;
    api.central.create(team_id, &prepared.command).await
}

/// 입력 검증 실패 시 400 에러 생성 헬퍼
fn validation() -> TaskError {
    TaskError::rejected("VALIDATION_FAILED", 400)
}
