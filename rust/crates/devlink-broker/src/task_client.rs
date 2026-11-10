//! 중앙 서버 Task REST API 전송 클라이언트
//!
//! WebSocket 접속 origin으로부터 HTTP/HTTPS 엔드포인트를 도출하며,
//! Bearer 인증 토큰을 안전하게 주입하고 로그, URL, 에러 메시지에 인증 정보가 누출되지 않도록 보장합니다.

use crate::{
    agent_metadata::AgentPart, central::endpoint_url, protocol::MAX_CENTRAL_MESSAGE_BYTES,
    sessions::Principal,
};
use anyhow::{Context, Result, ensure};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// 요청 본문 최대 크기 제한 (128 KB)
pub const REQUEST_LIMIT: usize = 128 * 1024;
/// 응답 본문 최대 수신 제한 (256 KB)
const RESPONSE_LIMIT: usize = 256 * 1024;

/// 중앙 서버로 전달되는 작업 생성 커맨드 DTO
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateTaskCommand {
    /// 멱등성 보장을 위한 클라이언트 고유 요청 키
    pub request_key: String,
    /// 요청자(발신) 에이전트 ID
    pub requester_agent_id: u64,
    /// 대상(수신) 에이전트 ID
    pub receiver_agent_id: u64,
    /// 중앙에 전달할 불투명 prompt 원문 (기존 MCP 입력은 별도 호환 변환)
    pub prompt: String,
}

/// 문자열 형태의 ID가 유효한 양의 64비트 정수(Java Long 호환)인지 검증 및 파싱
pub fn numeric_id(value: &str) -> Option<u64> {
    let id = value.parse::<u64>().ok()?;
    (id > 0 && id <= i64::MAX as u64 && id.to_string() == value).then_some(id)
}

impl CreateTaskCommand {
    /// 메시지가 중앙 서버 WebSocket/HTTP 전송 제한 내에 들어가는지 검증
    ///
    /// TASK_CREATED 엔벨로프, 식별자, 타임스탬프 등을 위한 여유 공간(1024바이트)을 확보합니다.
    pub fn fits_delivery(&self) -> bool {
        serde_json::to_vec(&self.prompt)
            .is_ok_and(|bytes| bytes.len() <= MAX_CENTRAL_MESSAGE_BYTES - 1024)
    }

    /// 커맨드 필드의 필수값 및 크기 제약 조건 검증
    pub fn valid(&self) -> bool {
        !self.request_key.trim().is_empty()
            && self.request_key.encode_utf16().count() <= 64
            && numeric_id(&self.requester_agent_id.to_string()).is_some()
            && numeric_id(&self.receiver_agent_id.to_string()).is_some()
            && !self.prompt.trim().is_empty()
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= REQUEST_LIMIT)
    }
}

/// 작업 생성 과정에서 발생하는 에러 표현 구조체
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskError {
    pub code: &'static str,
    pub status: u16,
    pub scope: &'static str,
    pub outcome: &'static str,
    pub task_id: Option<u64>,
}

impl TaskError {
    /// 도메인 검증 실패 또는 명시적 거부 에러 생성
    pub fn rejected(code: &'static str, status: u16) -> Self {
        Self {
            code,
            status,
            scope: "domain",
            outcome: "rejected",
            task_id: None,
        }
    }
    /// 네트워크 타임아웃 또는 중앙 서버 불가 시 unknown 에러 생성
    pub fn unknown(code: &'static str) -> Self {
        Self {
            code,
            status: 502,
            scope: "transport",
            outcome: "unknown",
            task_id: None,
        }
    }
}

/// 중앙 서버 Task API와 통신하는 HTTP 클라이언트
#[derive(Clone)]
pub struct TaskClient {
    client: Client,
    origin: url::Url,
    authorization: HeaderValue,
    timeout: Duration,
}

impl std::fmt::Debug for TaskClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskClient")
            .field("authorization", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl TaskClient {
    /// 중앙 서버 origin URL 문자열 반환
    pub fn origin(&self) -> String {
        self.origin.to_string()
    }
    /// 기본 타임아웃(5초)으로 TaskClient 초기화
    pub fn new(server_url: &str, token: &str) -> Result<Self> {
        Self::with_timeout(server_url, token, Duration::from_secs(5))
    }

    /// 지정된 타임아웃과 함께 TaskClient 초기화 및 보안 헤더 설정
    pub fn with_timeout(server_url: &str, token: &str, timeout: Duration) -> Result<Self> {
        ensure!(
            !token.trim().is_empty() && token.len() <= 8192,
            "Invalid central Agent token"
        );
        ensure!(
            !timeout.is_zero() && timeout <= Duration::from_secs(300),
            "Invalid central REST timeout"
        );
        let mut origin =
            url::Url::parse(&endpoint_url(server_url)?).context("Invalid central origin")?;
        let scheme = if origin.scheme() == "wss" {
            "https"
        } else {
            "http"
        };
        origin
            .set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("Invalid central REST scheme"))?;
        origin.set_path("/");
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("Invalid central Agent token header"))?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|_| anyhow::anyhow!("Cannot initialize central REST client"))?;
        Ok(Self {
            client,
            origin,
            authorization,
            timeout,
        })
    }

    /// 중앙 서버에 작업 생성 요청을 전송하고 결과를 반환
    pub async fn create(
        &self,
        team_id: u64,
        command: &CreateTaskCommand,
    ) -> Result<Value, TaskError> {
        if numeric_id(&team_id.to_string()).is_none()
            || !command.valid()
            || !command.fits_delivery()
        {
            return Err(TaskError::rejected("VALIDATION_FAILED", 400));
        }
        tokio::time::timeout(self.timeout, self.exchange(team_id, command))
            .await
            .unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT")))
    }

    /// HTTP POST 통신 및 중앙 서버 응답 엔벨로프 검증 수행
    async fn exchange(
        &self,
        team_id: u64,
        command: &CreateTaskCommand,
    ) -> Result<Value, TaskError> {
        // The caller verified this team against the Agent context. The server
        // authenticates the requester from the Bearer token, never body/query IDs.
        let body = json!({"requestKey":command.request_key,"receiverAgentId":command.receiver_agent_id,"prompt":command.prompt});
        let (status, envelope) = self
            .request(&format!("/api/v1/tasks/{team_id}"), Some(body))
            .await?;
        self.map_created(status, envelope, command)
    }

    async fn request(
        &self,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, CentralEnvelope), TaskError> {
        let method = if body.is_some() {
            reqwest::Method::POST
        } else {
            reqwest::Method::GET
        };
        self.request_http(method, path, &[], body).await
    }

    pub(crate) async fn request_http(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<(u16, CentralEnvelope), TaskError> {
        self.request_with_limit(method, path, query, body, REQUEST_LIMIT)
            .await
    }

    pub(crate) async fn request_graph_http(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, CentralEnvelope), TaskError> {
        self.request_with_limit(method, path, &[], body, 2 * 1024 * 1024)
            .await
    }

    async fn request_with_limit(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
        limit: usize,
    ) -> Result<(u16, CentralEnvelope), TaskError> {
        if body
            .as_ref()
            .is_some_and(|value| !serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= limit))
        {
            return Err(TaskError::rejected("VALIDATION_FAILED", 400));
        }
        tokio::time::timeout(self.timeout, self.exchange_http(method, path, query, body))
            .await
            .unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT")))
    }

    async fn exchange_http(
        &self,
        method: reqwest::Method,
        path: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<(u16, CentralEnvelope), TaskError> {
        let mut url = self.origin.clone();
        url.set_path(path);
        let builder = self.client.request(method, url).query(query);
        let builder = if let Some(body) = body {
            builder.json(&body)
        } else {
            builder
        };
        let mut response = builder
            .header(AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status().as_u16();
        // These denials remain definitive even if the error body is malformed.
        if status == 401 {
            return Err(TaskError::rejected("CENTRAL_UNAUTHENTICATED", 503));
        }
        if status == 403 {
            return Err(TaskError::rejected("FORBIDDEN", 403));
        }
        if (300..400).contains(&status) {
            return Err(TaskError::unknown("CENTRAL_PROTOCOL_ERROR"));
        }
        if response
            .content_length()
            .is_some_and(|length| length > RESPONSE_LIMIT as u64)
        {
            return Err(TaskError::unknown("CENTRAL_PROTOCOL_ERROR"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if bytes.len() + chunk.len() > RESPONSE_LIMIT {
                return Err(TaskError::unknown("CENTRAL_PROTOCOL_ERROR"));
            }
            bytes.extend_from_slice(&chunk);
        }
        let envelope: CentralEnvelope = serde_json::from_slice(&bytes)
            .map_err(|_| TaskError::unknown("CENTRAL_PROTOCOL_ERROR"))?;
        Ok((status, envelope))
    }

    fn map_created(
        &self,
        status: u16,
        envelope: CentralEnvelope,
        command: &CreateTaskCommand,
    ) -> Result<Value, TaskError> {
        let known_id = envelope
            .data
            .as_ref()
            .and_then(|data| known_task_id(data, command.receiver_agent_id));
        let protocol_error = || TaskError {
            task_id: known_id,
            ..TaskError::unknown("CENTRAL_PROTOCOL_ERROR")
        };
        if !(200..300).contains(&status) || !envelope.success {
            if let Some(id) = known_id {
                let mut error = TaskError::unknown("CENTRAL_RESULT_UNKNOWN");
                error.task_id = Some(id);
                return Err(error);
            }
            return Err(central_error(status, envelope.code.as_deref()));
        }
        if !matches!(status, 200 | 201) {
            return Err(protocol_error());
        }
        let data: CreatedTask = serde_json::from_value(envelope.data.unwrap_or(Value::Null))
            .map_err(|_| protocol_error())?;
        if numeric_id(&data.task_id.to_string()).is_none()
            || data.receiver_agent_id != command.receiver_agent_id
            || data.version > i64::MAX as u64
            || data.replayed != (status == 200)
            || !matches!(
                data.status.as_str(),
                "PENDING" | "IN_PROGRESS" | "COMPLETED" | "FAILED" | "REJECTED"
            )
        {
            return Err(protocol_error());
        }
        if let Some(delivery) = &data.delivery {
            delivery.validate().map_err(|_| protocol_error())?;
        }
        Ok(
            json!({"taskId":data.task_id.to_string(),"receiverAgentId":data.receiver_agent_id.to_string(),
            "status":data.status,"version":data.version.to_string(),"replayed":data.replayed,
            "requestKey":command.request_key,"delivery":data.delivery}),
        )
    }

    /// Confirm only the Agent already authenticated by the broker credential.
    pub async fn confirm_agent(&self, actor: &Principal) -> Result<Value, TaskError> {
        tokio::time::timeout(self.timeout, async {
            let data = self.read("/api/v1/agents/me").await?;
            let agent = PublicAgent::decode(data)?;
            if agent.owner_user_id.to_string() != actor.user_id.as_str()
                || agent.team_id.to_string() != actor.team_id.as_str()
                || actor
                    .agent_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() != agent.agent_id.to_string())
            {
                return Err(TaskError::rejected("FORBIDDEN", 403));
            }
            Ok(agent.local())
        })
        .await
        .unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT")))
    }

    pub async fn list_agents(&self, actor: &Principal) -> Result<Value, TaskError> {
        tokio::time::timeout(self.timeout, async {
            // Verify the credential still represents the local actor before using
            // selecting its verified team in the central list path.
            self.confirm_agent(actor).await?;
            let team_id = numeric_id(actor.team_id.as_str())
                .ok_or_else(|| TaskError::rejected("VALIDATION_FAILED", 400))?;
            let data = self
                .read(&format!("/api/v1/agents/teams/{team_id}"))
                .await?;
            let rows = data.as_array().ok_or_else(protocol_error)?;
            let mut ids = std::collections::HashSet::new();
            let mut agents = Vec::with_capacity(rows.len());
            for row in rows {
                let agent = PublicAgent::decode(row.clone())?;
                if agent.team_id.to_string() != actor.team_id.as_str()
                    || !ids.insert(agent.agent_id)
                {
                    return Err(protocol_error());
                }
                let mut view = agent.local();
                if let Some(part) = row["part"]
                    .as_str()
                    .and_then(|part| AgentPart::parse(part).ok())
                {
                    view["part"] = json!(part);
                }
                agents.push(view);
            }
            Ok(json!({"agents":agents}))
        })
        .await
        .unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT")))
    }

    /// Read the latest team-part context summary for the actor's own team.
    pub async fn team_context(
        &self,
        actor: &Principal,
        part: &str,
        counterpart: Option<&str>,
    ) -> Result<Value, TaskError> {
        tokio::time::timeout(self.timeout, async {
            self.confirm_agent(actor).await?;
            let team_id = numeric_id(actor.team_id.as_str())
                .ok_or_else(|| TaskError::rejected("VALIDATION_FAILED", 400))?;
            let mut query = vec![("part".to_string(), part.to_string())];
            if let Some(counterpart) = counterpart {
                query.push(("counterpart".to_string(), counterpart.to_string()));
            }
            let (status, envelope) = self
                .request_http(
                    reqwest::Method::GET,
                    &format!("/api/v1/teams/{team_id}/context"),
                    &query,
                    None,
                )
                .await?;
            if status != 200 || !envelope.success {
                return Err(central_error(status, envelope.code.as_deref()));
            }
            envelope.data.ok_or_else(protocol_error)
        })
        .await
        .unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT")))
    }

    /// S15P31D108-188: read-only central Agent recommendation for the verified team.
    /// `question` and `repo_key` must already satisfy `agent_recommendations`.
    pub async fn find_agents(
        &self,
        actor: &Principal,
        question: &str,
        repo_key: Option<&str>,
    ) -> Result<Value, TaskError> {
        tokio::time::timeout(self.timeout, async {
            // As in list_agents: the credential must still be this actor's Agent
            // before its team selects the central path.
            let agent = PublicAgent::decode(self.read("/api/v1/agents/me").await?)?;
            if agent.owner_user_id.to_string() != actor.user_id.as_str()
                || agent.team_id.to_string() != actor.team_id.as_str()
                || actor
                    .agent_id
                    .as_ref()
                    .is_none_or(|id| id.as_str() != agent.agent_id.to_string())
            {
                return Err(TaskError::rejected("FORBIDDEN", 403));
            }
            let body = crate::agent_recommendations::request_body(question, repo_key);
            let (status, envelope) = self
                .request(
                    &format!("/api/v1/teams/{}/agents/recommendations", agent.team_id),
                    Some(body),
                )
                .await?;
            crate::agent_recommendations::decode(status, envelope, agent.agent_id)
        })
        .await
        .unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT")))
    }

    async fn read(&self, path: &str) -> Result<Value, TaskError> {
        let (status, envelope) = self.request(path, None).await?;
        if status != 200 || !envelope.success {
            return Err(central_error(status, envelope.code.as_deref()));
        }
        envelope.data.ok_or_else(protocol_error)
    }
}

fn protocol_error() -> TaskError {
    TaskError::unknown("CENTRAL_PROTOCOL_ERROR")
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct PublicAgent {
    agent_id: u64,
    team_id: u64,
    owner_user_id: u64,
    name: String,
    role_description: Option<String>,
    tool_type: String,
    project_name: String,
    status: String,
    last_seen_at: Option<String>,
    created_at: String,
}
impl PublicAgent {
    fn decode(data: Value) -> Result<Self, TaskError> {
        let agent: Self = serde_json::from_value(data).map_err(|_| protocol_error())?;
        if [agent.agent_id, agent.team_id, agent.owner_user_id]
            .into_iter()
            .any(|id| id == 0 || id > i64::MAX as u64)
            || !matches!(agent.tool_type.as_str(), "CODEX" | "CLAUDE_CODE")
            || !matches!(agent.status.as_str(), "ONLINE" | "OFFLINE")
            || agent.name.trim().is_empty()
            || agent.created_at.is_empty()
        {
            return Err(protocol_error());
        }
        Ok(agent)
    }
    fn local(&self) -> Value {
        let mut data = serde_json::to_value(self).expect("public Agent serialization");
        data["agentId"] = json!(self.agent_id.to_string());
        data["teamId"] = json!(self.team_id.to_string());
        data["ownerUserId"] = json!(self.owner_user_id.to_string());
        data
    }
}

/// HTTP 전송 오류를 표준 TaskError로 변환
fn transport_error(error: reqwest::Error) -> TaskError {
    TaskError::unknown(if error.is_timeout() {
        "CENTRAL_TIMEOUT"
    } else {
        "CENTRAL_UNAVAILABLE"
    })
}

/// 응답 본문에서 기발급된 taskId 및 receiverAgentId가 존재하는지 추출
fn known_task_id(value: &Value, receiver: u64) -> Option<u64> {
    let id = value.get("taskId")?.as_u64()?;
    (numeric_id(&id.to_string()).is_some() && value.get("receiverAgentId")?.as_u64()? == receiver)
        .then_some(id)
}

/// 중앙 서버가 반환한 HTTP 상태 코드 및 비즈니스 에러 코드를 로컬 TaskError로 매핑
pub(crate) fn central_error(status: u16, code: Option<&str>) -> TaskError {
    if status >= 500 {
        return TaskError::unknown("CENTRAL_UNAVAILABLE");
    }
    match (status, code) {
        (400, Some("VALIDATION_FAILED")) => TaskError::rejected("VALIDATION_FAILED", 400),
        (404, Some("RESOURCE_NOT_FOUND")) => TaskError::rejected("RESOURCE_NOT_FOUND", 404),
        (409, Some("RECEIVER_OFFLINE")) => TaskError::rejected("RECEIVER_OFFLINE", 409),
        (409, Some("TASK_VERSION_CONFLICT")) => TaskError::rejected("TASK_VERSION_CONFLICT", 409),
        (409, Some("TASK_STATE_CONFLICT")) => TaskError::rejected("TASK_STATE_CONFLICT", 409),
        // Question/answer rules (S15P31D108-166).
        (409, Some("TASK_CONVERSATION_UNAVAILABLE")) => {
            TaskError::rejected("TASK_CONVERSATION_UNAVAILABLE", 409)
        }
        (400, Some("INVALID_QUESTION_REFERENCE")) => {
            TaskError::rejected("INVALID_QUESTION_REFERENCE", 400)
        }
        // Identical concurrent inserts can also produce this provider code.
        // Preserve the conflict without claiming no transaction committed.
        (409, Some("REQUEST_KEY_CONFLICT")) => TaskError {
            outcome: "unknown",
            ..TaskError::rejected("REQUEST_KEY_CONFLICT", 409)
        },
        // A concurrent insert conflict may refer to a task already committed.
        (409, Some("DUPLICATE_REQUEST")) => TaskError::unknown("CENTRAL_RESULT_UNKNOWN"),
        _ => TaskError::unknown("CENTRAL_PROTOCOL_ERROR"),
    }
}

#[derive(Deserialize)]
pub(crate) struct CentralEnvelope {
    pub success: bool,
    pub data: Option<Value>,
    pub code: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatedTask {
    task_id: u64,
    receiver_agent_id: u64,
    status: String,
    version: u64,
    replayed: bool,
    delivery: Option<Delivery>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Delivery {
    status: String,
    reason_code: Option<String>,
}

impl Delivery {
    fn validate(&self) -> Result<(), TaskError> {
        let valid = match self.status.as_str() {
            "ACK_PENDING" | "ACK_RECEIVED" => self.reason_code.is_none(),
            "DELIVERY_NOT_SENT" | "DELIVERY_UNKNOWN" => {
                self.reason_code.as_deref().is_some_and(|code| {
                    matches!(
                        code,
                        "RECEIVER_OFFLINE"
                            | "CONNECTION_UNAVAILABLE"
                            | "CONNECTION_REPLACED"
                            | "TRANSPORT_ERROR"
                            | "TASK_NOT_PENDING"
                            | "ACK_TIMEOUT"
                            | "CONNECTION_CLOSED"
                            | "HISTORY_RECORD_FAILED"
                            | "ROUTING_ERROR"
                            | "HISTORY_READ_FAILED"
                    )
                })
            }
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(TaskError::unknown("CENTRAL_PROTOCOL_ERROR"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_prompt_boundary_fits_the_actual_task_created_wire() {
        use crate::protocol::{ServerMessage, TaskCreatedPayload};
        let mut command = CreateTaskCommand {
            request_key: "request".into(),
            requester_agent_id: i64::MAX as u64,
            receiver_agent_id: i64::MAX as u64,
            prompt: "x".repeat(MAX_CENTRAL_MESSAGE_BYTES - 1024 - 2),
        };
        assert!(command.fits_delivery());
        let wire = ServerMessage::TaskCreated {
            event_id: uuid::Uuid::new_v4().to_string(),
            payload: TaskCreatedPayload {
                task_id: i64::MAX as u64,
                team_id: i64::MAX as u64,
                requester_agent_id: command.requester_agent_id,
                receiver_agent_id: command.receiver_agent_id,
                prompt: command.prompt.clone(),
                created_at: "+999999999-12-31T23:59:59.999999999".into(),
            },
        };
        assert!(serde_json::to_vec(&wire).unwrap().len() <= MAX_CENTRAL_MESSAGE_BYTES);
        command.prompt.push('x');
        assert!(!command.fits_delivery());
    }

    #[test]
    fn task_origin_and_header_keep_transport_credentials_private() {
        let secret = "synthetic-secret-agent-token";
        let client = TaskClient::new("wss://central.example:8443/ws/broker", secret).unwrap();
        assert_eq!(client.origin(), "https://central.example:8443/");
        assert!(client.authorization.is_sensitive());
        assert!(!format!("{client:?}").contains(secret));
        for invalid in [
            "https://central.example",
            "ws://user:password@central.example",
            "ws://central.example/other",
            "ws://central.example/?token=secret",
        ] {
            assert!(TaskClient::new(invalid, secret).is_err());
        }
        assert!(TaskClient::new("ws://central.example", "bad\r\nheader").is_err());
        assert!(TaskClient::with_timeout("ws://central.example", secret, Duration::ZERO).is_err());
    }

    #[test]
    fn numeric_ids_are_canonical_positive_java_longs() {
        for invalid in [
            "0",
            "-1",
            "+1",
            "01",
            " 1",
            "9223372036854775808",
            "agent-1",
        ] {
            assert_eq!(numeric_id(invalid), None);
        }
        assert_eq!(numeric_id("9223372036854775807"), Some(i64::MAX as u64));
    }
}
