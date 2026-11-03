//! -94 provider DTOs. No local task state, file access, or mutation retries.
use crate::{
    sessions::Principal,
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
}
impl Operation {
    pub fn mutation(&self) -> bool {
        matches!(self, Self::Status { .. } | Self::Complete { .. })
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
        if status != 200 || !envelope.success {
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
        let body = match self.status {
            TaskStatus::InProgress
                if self.failure_reason.is_none() && self.partially_modified.is_none() =>
            {
                json!({"status":self.status,"version":self.version.0})
            }
            TaskStatus::Failed
                if self
                    .failure_reason
                    .as_ref()
                    .is_some_and(|s| !s.trim().is_empty())
                    && self.partially_modified.is_some() =>
            {
                json!({"status":self.status,"version":self.version.0,"failureReason":self.failure_reason,"partiallyModified":self.partially_modified})
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
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventData {
    event_type: String,
    message: Option<String>,
    created_at: String,
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
