//! -94 provider DTOs. No local task state, file access, or mutation retries.
use crate::{
    sessions::{Identifier, Principal},
    task_client::{TaskClient, TaskError, central_error, numeric_id},
};
use reqwest::Method;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub fn validation() -> TaskError {
    TaskError::rejected("VALIDATION_FAILED", 400)
}
fn protocol() -> TaskError {
    TaskError::unknown("CENTRAL_PROTOCOL_ERROR")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version(pub u64);
impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        let version = value
            .as_u64()
            .or_else(|| {
                value.as_str().and_then(|s| {
                    let n = s.parse::<u64>().ok()?;
                    (n.to_string() == s).then_some(n)
                })
            })
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| D::Error::custom("invalid version"))?;
        Ok(Self(version))
    }
}
impl Serialize for Version {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Rejected,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Sent,
    Received,
}
impl Direction {
    fn name(self) -> &'static str {
        match self {
            Self::Sent => "sent",
            Self::Received => "received",
        }
    }
}
fn received() -> Direction {
    Direction::Received
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusArgs {
    pub task_id: String,
    pub version: Version,
    pub status: TaskStatus,
    #[serde(default, alias = "reason")]
    pub failure_reason: Option<String>,
    #[serde(default)]
    pub partially_modified: Option<bool>,
    /// Required only for REJECTED; central accepts it only from PENDING.
    #[serde(default)]
    pub rejection_reason: Option<String>,
    #[serde(default)]
    pub request_key: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileArgs {
    pub relative_path: String,
    pub change_type: ChangeType,
    #[serde(default)]
    pub diff_text: Option<String>,
}
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    Renamed,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompleteArgs {
    pub task_id: String,
    pub version: Version,
    pub result_text: String,
    #[serde(default)]
    pub task_files: Option<Vec<FileArgs>>,
    #[serde(default)]
    pub request_key: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResultArgs {
    pub task_id: String,
    #[serde(default)]
    pub request_key: Option<String>,
}
/// Central TaskMessageService limit for question and answer bodies.
const MAX_MESSAGE_CHARS: usize = 8_000;

/// S15P31D108-166: a question on a task this Agent takes part in.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionArgs {
    pub task_id: String,
    pub message: String,
    /// Central idempotency key (task + author + key). Required; MCP issues one when omitted.
    pub request_key: String,
}
/// S15P31D108-166: an answer to the other participant's question on the same task.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerArgs {
    pub task_id: String,
    pub question_event_id: String,
    pub message: String,
    pub request_key: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageKind {
    Question,
    Answer,
}
impl MessageKind {
    fn path(self) -> &'static str {
        match self {
            Self::Question => "questions",
            Self::Answer => "answers",
        }
    }
    fn event_type(self) -> &'static str {
        match self {
            Self::Question => "QUESTION",
            Self::Answer => "ANSWER",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListArgs {
    #[serde(default = "received")]
    pub direction: Direction,
    #[serde(default)]
    pub status: Option<TaskStatus>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub size: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
}

pub struct ListQuery {
    page: u32,
    size: u32,
    direction: Direction,
    status: Option<TaskStatus>,
    team: u64,
    agent: u64,
    fingerprint: String,
}
pub enum Operation {
    Status {
        id: u64,
        version: Version,
        status: TaskStatus,
        body: Value,
    },
    Complete {
        id: u64,
        version: Version,
        body: Value,
    },
    Result {
        id: u64,
        team: u64,
    },
    List(ListQuery),
    Message {
        id: u64,
        kind: MessageKind,
        author: u64,
        message: String,
        question: Option<u64>,
        body: Value,
    },
}
impl Operation {
    pub fn mutation(&self) -> bool {
        matches!(
            self,
            Self::Status { .. } | Self::Complete { .. } | Self::Message { .. }
        )
    }
    pub async fn execute(&self, client: &TaskClient) -> Result<Value, TaskError> {
        let (method, path, query, body) = match self {
            Self::Status { id, body, .. } => (
                Method::PATCH,
                format!("/api/v1/tasks/{id}/status"),
                vec![],
                Some(body.clone()),
            ),
            Self::Complete { id, body, .. } => (
                Method::POST,
                format!("/api/v1/tasks/{id}/complete"),
                vec![],
                Some(body.clone()),
            ),
            Self::Result { id, .. } => (Method::GET, format!("/api/v1/tasks/{id}"), vec![], None),
            Self::Message { id, kind, body, .. } => (
                Method::POST,
                format!("/api/v1/tasks/{id}/{}", kind.path()),
                vec![],
                Some(body.clone()),
            ),
            Self::List(q) => {
                let mut params = vec![
                    ("scope".into(), "mine".into()),
                    ("direction".into(), q.direction.name().into()),
                    ("page".into(), q.page.to_string()),
                    ("size".into(), q.size.to_string()),
                ];
                if let Some(status) = q.status {
                    params.push((
                        "status".into(),
                        serde_json::to_value(status)
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .into(),
                    ));
                }
                (
                    Method::GET,
                    format!("/api/v1/teams/{}/tasks", q.team),
                    params,
                    None,
                )
            }
        };
        let (status, envelope) = client.request_http(method, &path, &query, body).await?;
        // Central answers a new question/answer with 201 and a key replay with 200.
        let accepted = status == 200 || (status == 201 && matches!(self, Self::Message { .. }));
        if !accepted || !envelope.success {
            return Err(central_error(status, envelope.code.as_deref()));
        }
        let data = envelope.data.ok_or_else(protocol)?;
        match self {
            Self::Status {
                id,
                status,
                version,
                ..
            } => {
                let data: StatusData = decode(data)?;
                if data.task_id.0 != *id || data.status != *status || data.version.0 <= version.0 {
                    return Err(protocol());
                }
                encode(data)
            }
            Self::Complete { id, version, .. } => {
                let data: CompleteData = decode(data)?;
                if data.task_id.0 != *id
                    || data.status != TaskStatus::Completed
                    || data.version.0 <= version.0
                    || data.completed_at.is_empty()
                {
                    return Err(protocol());
                }
                encode(data)
            }
            Self::Message {
                id,
                kind,
                author,
                message,
                question,
                ..
            } => {
                let data: MessageData = decode(data)?;
                if data.task_id.0 != *id
                    || data.event_type != kind.event_type()
                    || data.author_agent.agent_id.0 != *author
                    || data.message != *message
                    || data.question_event_id.map(|q| q.0) != *question
                    || data.created_at.is_empty()
                    || data.replayed == (status == 201)
                {
                    return Err(protocol());
                }
                encode(data)
            }
            Self::Result { id, team } => {
                let data: DetailData = decode(data)?;
                if data.task_id.0 != *id || data.team_id.0 != *team {
                    return Err(protocol());
                }
                encode(data)
            }
            Self::List(q) => {
                let data: ListData = decode(data)?;
                if data.page != q.page
                    || data.size != q.size
                    || data.items.len() > q.size as usize
                    || data.total_elements > i64::MAX as u64
                    || data.total_pages > i32::MAX as u32
                    || data.has_next != (data.page.saturating_add(1) < data.total_pages)
                {
                    return Err(protocol());
                }
                let mut seen = std::collections::HashSet::new();
                for item in &data.items {
                    if item.team_id.0 != q.team
                        || !seen.insert(item.task_id.0)
                        || match q.direction {
                            Direction::Sent => item.requester_agent.agent_id.0 != q.agent,
                            Direction::Received => item.receiver_agent.agent_id.0 != q.agent,
                        }
                        || q.status.is_some_and(|s| s != item.status)
                    {
                        return Err(protocol());
                    }
                }
                let next = data
                    .has_next
                    .then(|| format!("v1.{}.{}", data.page + 1, q.fingerprint));
                let mut result = encode(data)?;
                result["nextCursor"] = json!(next);
                Ok(result)
            }
        }
    }
}

pub trait Arguments: serde::de::DeserializeOwned {
    const NAME: &'static str;
    fn prepare(&self, actor: &Principal, origin: &str) -> Result<Operation, TaskError>;
    fn task_id(&self) -> Option<u64> {
        None
    }
    fn request_key(&self) -> Option<&str> {
        None
    }
}
fn key_valid(key: Option<&str>) -> bool {
    key.is_none_or(|key| !key.trim().is_empty() && key.encode_utf16().count() <= 64)
}
/// Conversation keys reach central, so they use the shared identifier rules.
fn conversation_key(key: &str) -> bool {
    Identifier::parse(key).is_ok() && key.encode_utf16().count() <= 64
}
fn message_operation(
    actor: &Principal,
    task_id: &str,
    kind: MessageKind,
    message: &str,
    question: Option<&str>,
    key: &str,
) -> Result<Operation, TaskError> {
    let id = numeric_id(task_id).ok_or_else(validation)?;
    let author = actor
        .agent_id
        .as_ref()
        .and_then(|id| numeric_id(id.as_str()))
        .ok_or_else(validation)?;
    // Mirrors central: non-blank, at most 8,000 code points, no NUL.
    if message.trim().is_empty()
        || message.chars().count() > MAX_MESSAGE_CHARS
        || message.contains('\0')
        || !conversation_key(key)
    {
        return Err(validation());
    }
    let question = question
        .map(|value| numeric_id(value).ok_or_else(validation))
        .transpose()?;
    let mut body = json!({"requestKey":key,"message":message});
    if let Some(question) = question {
        body["questionEventId"] = json!(question.to_string());
    }
    Ok(Operation::Message {
        id,
        kind,
        author,
        message: message.into(),
        question,
        body,
    })
}
impl Arguments for QuestionArgs {
    const NAME: &'static str = "ask_question";
    fn task_id(&self) -> Option<u64> {
        numeric_id(&self.task_id)
    }
    fn request_key(&self) -> Option<&str> {
        Some(&self.request_key)
    }
    fn prepare(&self, actor: &Principal, _: &str) -> Result<Operation, TaskError> {
        message_operation(
            actor,
            &self.task_id,
            MessageKind::Question,
            &self.message,
            None,
            &self.request_key,
        )
    }
}
impl Arguments for AnswerArgs {
    const NAME: &'static str = "answer_question";
    fn task_id(&self) -> Option<u64> {
        numeric_id(&self.task_id)
    }
    fn request_key(&self) -> Option<&str> {
        Some(&self.request_key)
    }
    fn prepare(&self, actor: &Principal, _: &str) -> Result<Operation, TaskError> {
        message_operation(
            actor,
            &self.task_id,
            MessageKind::Answer,
            &self.message,
            Some(&self.question_event_id),
            &self.request_key,
        )
    }
}
impl Arguments for StatusArgs {
    const NAME: &'static str = "update_task_status";
    fn task_id(&self) -> Option<u64> {
        numeric_id(&self.task_id)
    }
    fn request_key(&self) -> Option<&str> {
        self.request_key.as_deref()
    }
    fn prepare(&self, _: &Principal, _: &str) -> Result<Operation, TaskError> {
        let id = self.task_id().ok_or_else(validation)?;
        if !key_valid(self.request_key()) {
            return Err(validation());
        }
        let text = |value: &Option<String>| value.as_ref().is_some_and(|s| !s.trim().is_empty());
        // Mirrors central ChangeTaskStatusRequest: each status carries only its own fields.
        let body = match self.status {
            TaskStatus::InProgress
                if self.failure_reason.is_none()
                    && self.partially_modified.is_none()
                    && self.rejection_reason.is_none() =>
            {
                json!({"status":self.status,"version":self.version.0})
            }
            TaskStatus::Failed
                if text(&self.failure_reason)
                    && self.partially_modified.is_some()
                    && self.rejection_reason.is_none() =>
            {
                json!({"status":self.status,"version":self.version.0,"failureReason":self.failure_reason,"partiallyModified":self.partially_modified})
            }
            TaskStatus::Rejected
                if text(&self.rejection_reason)
                    && self.failure_reason.is_none()
                    && self.partially_modified.is_none() =>
            {
                json!({"status":self.status,"version":self.version.0,"rejectionReason":self.rejection_reason})
            }
            _ => return Err(validation()),
        };
        Ok(Operation::Status {
            id,
            version: self.version,
            status: self.status,
            body,
        })
    }
}
impl Arguments for CompleteArgs {
    const NAME: &'static str = "complete_task";
    fn task_id(&self) -> Option<u64> {
        numeric_id(&self.task_id)
    }
    fn request_key(&self) -> Option<&str> {
        self.request_key.as_deref()
    }
    fn prepare(&self, _: &Principal, _: &str) -> Result<Operation, TaskError> {
        let id = self.task_id().ok_or_else(validation)?;
        if !key_valid(self.request_key())
            || self.result_text.trim().is_empty()
            || self.task_files.as_ref().is_some_and(|files| {
                files.iter().any(|f| {
                    f.relative_path.trim().is_empty()
                        || f.relative_path.encode_utf16().count() > 500
                })
            })
        {
            return Err(validation());
        }
        Ok(Operation::Complete {
            id,
            version: self.version,
            body: json!({"version":self.version.0,"resultText":self.result_text,"taskFiles":self.task_files.as_deref().unwrap_or(&[])}),
        })
    }
}
impl Arguments for ResultArgs {
    const NAME: &'static str = "get_task_result";
    fn task_id(&self) -> Option<u64> {
        numeric_id(&self.task_id)
    }
    fn request_key(&self) -> Option<&str> {
        self.request_key.as_deref()
    }
    fn prepare(&self, actor: &Principal, _: &str) -> Result<Operation, TaskError> {
        if !key_valid(self.request_key()) {
            return Err(validation());
        }
        Ok(Operation::Result {
            id: self.task_id().ok_or_else(validation)?,
            team: numeric_id(actor.team_id.as_str()).ok_or_else(validation)?,
        })
    }
}
impl Arguments for ListArgs {
    const NAME: &'static str = "get_tasks";
    fn prepare(&self, actor: &Principal, origin: &str) -> Result<Operation, TaskError> {
        if self.size.is_some() && self.limit.is_some()
            || self.cursor.is_some() && (self.page.is_some() || self.size.is_some())
        {
            return Err(validation());
        }
        let size = self.size.or(self.limit).unwrap_or(20);
        if !(1..=100).contains(&size) {
            return Err(validation());
        }
        let team = numeric_id(actor.team_id.as_str()).ok_or_else(validation)?;
        let agent = actor
            .agent_id
            .as_ref()
            .and_then(|id| numeric_id(id.as_str()))
            .ok_or_else(validation)?;
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    origin,
                    &actor.user_id,
                    &actor.team_id,
                    &actor.agent_id,
                    self.direction,
                    self.status,
                    size
                ))
                .map_err(|_| validation())?
            )
        );
        let page = if let Some(cursor) = &self.cursor {
            let parts: Vec<_> = cursor.split('.').collect();
            if cursor.len() > 512 || parts.len() != 3 || parts[0] != "v1" || parts[2] != fingerprint
            {
                return Err(validation());
            }
            let page = parts[1].parse::<u32>().map_err(|_| validation())?;
            if page == 0 || page.to_string() != parts[1] {
                return Err(validation());
            }
            page
        } else {
            self.page.unwrap_or(0)
        };
        if page > i32::MAX as u32 {
            return Err(validation());
        }
        Ok(Operation::List(ListQuery {
            page,
            size,
            direction: self.direction,
            status: self.status,
            team,
            agent,
            fingerprint,
        }))
    }
}

fn decode<T: serde::de::DeserializeOwned>(data: Value) -> Result<T, TaskError> {
    serde_json::from_value(data).map_err(|_| protocol())
}
fn encode<T: Serialize>(data: T) -> Result<Value, TaskError> {
    serde_json::to_value(data).map_err(|_| protocol())
}

#[derive(Clone, Copy, Debug)]
struct Id(u64);
impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let n = u64::deserialize(d)?;
        if n == 0 || n > i64::MAX as u64 {
            return Err(D::Error::custom("invalid ID"));
        }
        Ok(Self(n))
    }
}
impl Serialize for Id {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentRef {
    agent_id: Id,
    name: String,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SummaryData {
    task_id: Id,
    team_id: Id,
    requester_agent: AgentRef,
    receiver_agent: AgentRef,
    status: TaskStatus,
    created_at: String,
    started_at: Option<String>,
    completed_at: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListData {
    items: Vec<SummaryData>,
    page: u32,
    size: u32,
    total_elements: u64,
    total_pages: u32,
    has_next: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusData {
    task_id: Id,
    status: TaskStatus,
    #[serde(deserialize_with = "provider_version")]
    version: Version,
    rejection_reason: Option<String>,
    failure_reason: Option<String>,
    partially_modified: bool,
    started_at: Option<String>,
    completed_at: Option<String>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct CompleteData {
    task_id: Id,
    status: TaskStatus,
    #[serde(deserialize_with = "provider_version")]
    version: Version,
    completed_at: String,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileData {
    file_id: Id,
    relative_path: String,
    change_type: ChangeType,
    diff_text: Option<String>,
}
/// Central sends event IDs as canonical positive decimal strings.
#[derive(Clone, Copy, Debug)]
struct TextId(u64);
impl<'de> Deserialize<'de> for TextId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        numeric_id(&text)
            .map(Self)
            .ok_or_else(|| D::Error::custom("invalid event ID"))
    }
}
impl Serialize for TextId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0.to_string())
    }
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventData {
    event_type: String,
    message: Option<String>,
    created_at: String,
    // Kept so an Agent can find a question's eventId to answer (S15P31D108-166).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event_id: Option<TextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sequence: Option<TextId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    author_agent: Option<AgentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    question_event_id: Option<TextId>,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MessageData {
    event_id: TextId,
    task_id: Id,
    sequence: TextId,
    event_type: String,
    author_agent: AgentRef,
    message: String,
    question_event_id: Option<TextId>,
    created_at: String,
    replayed: bool,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DetailData {
    task_id: Id,
    team_id: Id,
    requester_agent: AgentRef,
    receiver_agent: AgentRef,
    prompt: String,
    status: TaskStatus,
    #[serde(deserialize_with = "provider_version")]
    version: Version,
    result_text: Option<String>,
    failure_reason: Option<String>,
    rejection_reason: Option<String>,
    partially_modified: bool,
    task_files: Vec<FileData>,
    events: Vec<EventData>,
    created_at: String,
    started_at: Option<String>,
    completed_at: Option<String>,
}

fn provider_version<'de, D: Deserializer<'de>>(d: D) -> Result<Version, D::Error> {
    let n = u64::deserialize(d)?;
    if n > i64::MAX as u64 {
        return Err(D::Error::custom("invalid provider version"));
    }
    Ok(Version(n))
}
