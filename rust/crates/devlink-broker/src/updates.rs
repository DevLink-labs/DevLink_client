//! Local advisory update feed over the durable inbox, requester notifications and
//! question/answer arrivals synchronized from central task-messages.
//! Read-only: never acknowledges, deletes or marks events read, and never calls central.
//! Central task APIs stay authoritative; a missing local update is not evidence of no change.
use crate::{
    business_api::{self, BusinessRequest, LOCAL_BUDGET, session_error},
    central::endpoint_url,
    inbox::Inbox,
    notifications::{Notifications, TaskNotification},
    protocol::{TaskCreatedMessage, TaskMessageType},
    sent_tasks::{self, OverdueEvent},
    session_api,
    sessions::{Principal, SessionError, SessionManager},
    task_client::TaskError,
    task_messages::{TaskMessage, TaskMessages},
};
use anyhow::Result;
use axum::{
    Router,
    extract::{Request, State},
    response::Response,
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{path::PathBuf, sync::Arc};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

pub const NAME: &str = "get_updates";
pub const MAX_LIMIT: usize = 50;
const DEFAULT_LIMIT: usize = 20;
const MAX_CURSOR: usize = 512;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdatesArgs {
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u64>,
}
impl UpdatesArgs {
    fn limit(&self) -> Option<usize> {
        match self.limit {
            None => Some(DEFAULT_LIMIT),
            Some(limit) => usize::try_from(limit)
                .ok()
                .filter(|l| (1..=MAX_LIMIT).contains(l)),
        }
    }
}

/// Positions in the four append-only, actor-scoped sources.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Position {
    received: usize,
    sent: usize,
    messages: usize,
    overdue: usize,
}

/// Cursor format. Older versions digest fewer sources and resume with the newer
/// sources unconsumed: v1 had no question/answer arrivals, v2 no overdue advisories.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorVersion {
    V1,
    V2,
    V3,
}

/// One actor's view of all stores, in persisted order.
pub(crate) struct Feed {
    scope: String,
    received: Vec<TaskCreatedMessage>,
    sent: Vec<TaskNotification>,
    messages: Vec<TaskMessage>,
    overdue: Vec<OverdueEvent>,
}
impl Feed {
    pub(crate) fn load(
        config_path: &std::path::Path,
        origin: &str,
        actor: &Principal,
    ) -> Result<Self> {
        let parse = |id: &str| id.parse::<u64>().ok().filter(|v| *v > 0);
        let agent = actor.agent_id.as_ref().map(|id| id.as_str());
        let (Some(agent_id), Some(team_id)) =
            (agent.and_then(parse), parse(actor.team_id.as_str()))
        else {
            anyhow::bail!("Unsupported actor identifiers");
        };
        let origin = endpoint_url(origin)?.to_string();
        let received = Inbox::received_config(config_path, &origin, agent_id)?
            .into_iter()
            .filter(|m| m.payload.team_id == team_id)
            .collect();
        let sent = Notifications::events_config(config_path, &origin, agent_id)?;
        let messages = TaskMessages::events_config(config_path, &origin, agent_id)?;
        let overdue = sent_tasks::events_config(config_path, &origin, agent_id, team_id)?;
        Ok(Self {
            // Binding the cursor to the full actor scope keeps it from widening another view.
            scope: format!(
                "{origin}\n{}\n{}\n{agent_id}",
                actor.user_id.as_str(),
                actor.team_id.as_str()
            ),
            received,
            sent,
            messages,
            overdue,
        })
    }

    /// Digest of the actor scope and every consumed eventId, so a cleared store that
    /// later re-delivers the same last event at the same position cannot pass.
    /// Older versions reproduce their digest, which covered fewer sources.
    fn tag(&self, at: Position, version: CursorVersion) -> String {
        let mut digest = Sha256::new();
        let mut field = |value: &str| {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        };
        field(&self.scope);
        field(&at.received.to_string());
        for message in &self.received[..at.received] {
            field(&message.event_id);
        }
        field(&at.sent.to_string());
        for event in &self.sent[..at.sent] {
            field(event.event_id());
        }
        if version != CursorVersion::V1 {
            field(&at.messages.to_string());
            for message in &self.messages[..at.messages] {
                field(&message.event_id);
            }
        }
        if version == CursorVersion::V3 {
            field(&at.overdue.to_string());
            for event in &self.overdue[..at.overdue] {
                field(&event.event_id);
            }
        }
        digest.finalize()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn cursor(&self, at: Position) -> String {
        format!(
            "v3.{}.{}.{}.{}.{}",
            at.received,
            at.sent,
            at.messages,
            at.overdue,
            self.tag(at, CursorVersion::V3)
        )
    }

    /// A cursor from a cleared store, another scope or a replaced history restarts from
    /// the beginning instead of skipping or inventing events.
    fn resolve(&self, cursor: Option<&Cursor>) -> (Position, bool) {
        let Some(cursor) = cursor else {
            return (Position::default(), false);
        };
        let at = cursor.at;
        // Older cursors predate later sources: they resume with none of those consumed.
        if at.received <= self.received.len()
            && at.sent <= self.sent.len()
            && at.messages <= self.messages.len()
            && at.overdue <= self.overdue.len()
            && self.tag(at, cursor.version) == cursor.tag
        {
            (at, false)
        } else {
            (Position::default(), true)
        }
    }

    /// Next entries after `cursor`: remaining received tasks first, then sent-task events,
    /// then question/answer arrivals, then overdue advisories.
    pub(crate) fn take(&self, cursor: Option<&Cursor>, limit: usize) -> Batch<'_> {
        let (mut at, reset) = self.resolve(cursor);
        let mut updates = Vec::with_capacity(limit.min(self.len()));
        for message in self.received[at.received..].iter().take(limit) {
            updates.push(Update::Received(message));
            at.received += 1;
        }
        for event in self.sent[at.sent..].iter().take(limit - updates.len()) {
            updates.push(Update::Sent(event));
            at.sent += 1;
        }
        for message in self.messages[at.messages..]
            .iter()
            .take(limit - updates.len())
        {
            updates.push(Update::Message(message));
            at.messages += 1;
        }
        for event in self.overdue[at.overdue..]
            .iter()
            .take(limit - updates.len())
        {
            updates.push(Update::Overdue(event));
            at.overdue += 1;
        }
        Batch {
            updates,
            next: self.cursor(at),
            has_more: at.received < self.received.len()
                || at.sent < self.sent.len()
                || at.messages < self.messages.len()
                || at.overdue < self.overdue.len(),
            reset,
        }
    }

    /// Highest retained version of a sent task (later entry wins a tie). Notifications keep
    /// late lower versions in history, so the newest batch alone can be stale.
    pub(crate) fn latest_sent(&self, task_id: u64) -> Option<&TaskNotification> {
        self.sent
            .iter()
            .filter(|event| event.task_id() == task_id)
            .reduce(|best, event| {
                if event.version() >= best.version() {
                    event
                } else {
                    best
                }
            })
    }

    /// Every retained entry; all stores are bounded, so one batch drains the feed.
    pub(crate) fn len(&self) -> usize {
        self.received.len() + self.sent.len() + self.messages.len() + self.overdue.len()
    }

    pub(crate) fn page(&self, cursor: Option<&Cursor>, limit: usize) -> Value {
        let batch = self.take(cursor, limit);
        let updates: Vec<Value> = batch
            .updates
            .iter()
            .map(|update| match update {
                Update::Received(message) => json!({
                    "eventId": message.event_id,
                    "kind": "TASK_CREATED",
                    "direction": "received",
                    "taskId": message.payload.task_id.to_string(),
                    "requesterAgentId": message.payload.requester_agent_id.to_string(),
                }),
                Update::Sent(event) => json!({
                    "eventId": event.event_id(),
                    "kind": match event {
                        TaskNotification::TaskStatusChanged { .. } => "TASK_STATUS_CHANGED",
                        TaskNotification::TaskCompleted { .. } => "TASK_COMPLETED",
                    },
                    "direction": "sent",
                    "taskId": event.task_id().to_string(),
                    "taskVersion": event.version().to_string(),
                    "status": event.status(),
                }),
                // Arrival facts only: the question/answer text stays behind get_task_result.
                Update::Message(message) => json!({
                    "eventId": message.event_id,
                    "kind": message_kind(message.event_type),
                    "direction": "received",
                    "taskId": message.task_id.to_string(),
                    "sequence": message.sequence,
                }),
                // Local advisory only: central status is unchanged and stays authoritative.
                Update::Overdue(event) => json!({
                    "eventId": event.event_id,
                    "kind": "TASK_OVERDUE",
                    "direction": "sent",
                    "taskId": event.task_id.to_string(),
                    "minutes": event.minutes,
                }),
            })
            .collect();
        json!({
            "updates": updates,
            "nextCursor": batch.next,
            "hasMore": batch.has_more,
            "cursorReset": batch.reset,
            "coverage": "local",
        })
    }
}

pub(crate) enum Update<'a> {
    Received(&'a TaskCreatedMessage),
    Sent(&'a TaskNotification),
    Message(&'a TaskMessage),
    Overdue(&'a OverdueEvent),
}

pub(crate) fn message_kind(kind: TaskMessageType) -> &'static str {
    match kind {
        TaskMessageType::Question => "TASK_QUESTION_CREATED",
        TaskMessageType::Answer => "TASK_ANSWER_CREATED",
    }
}

pub(crate) struct Batch<'a> {
    pub(crate) updates: Vec<Update<'a>>,
    pub(crate) next: String,
    pub(crate) has_more: bool,
    pub(crate) reset: bool,
}

/// A syntactically valid cursor; whether it still matches the stores is decided by Feed.
pub(crate) struct Cursor {
    at: Position,
    tag: String,
    version: CursorVersion,
}
impl Cursor {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        if value.len() > MAX_CURSOR {
            return None;
        }
        let position = |part: &str| {
            (!part.is_empty()
                && part.len() <= 10
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0')))
            .then(|| part.parse::<usize>().ok())
            .flatten()
        };
        let parts: Vec<&str> = value.split('.').collect();
        let (version, received, sent, messages, overdue, tag) = match parts.as_slice() {
            ["v1", received, sent, tag] => (CursorVersion::V1, *received, *sent, "0", "0", *tag),
            ["v2", received, sent, messages, tag] => {
                (CursorVersion::V2, *received, *sent, *messages, "0", *tag)
            }
            ["v3", received, sent, messages, overdue, tag] => (
                CursorVersion::V3,
                *received,
                *sent,
                *messages,
                *overdue,
                *tag,
            ),
            _ => return None,
        };
        let tag_valid = tag.len() == 32
            && tag
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        Some(Self {
            at: Position {
                received: position(received)?,
                sent: position(sent)?,
                messages: position(messages)?,
                overdue: position(overdue)?,
            },
            tag: tag_valid.then(|| tag.to_owned())?,
            version,
        })
    }
}

#[derive(Clone)]
struct Api {
    token: Arc<String>,
    sessions: SessionManager,
    config_path: Arc<PathBuf>,
    origin: Arc<String>,
    cancel: CancellationToken,
}
pub fn router(
    token: Arc<String>,
    sessions: SessionManager,
    config_path: PathBuf,
    server_url: &str,
    cancel: CancellationToken,
) -> Result<Router> {
    Ok(Router::new()
        .route("/v1/updates", post(updates))
        .with_state(Api {
            token,
            sessions,
            config_path: Arc::new(config_path),
            origin: Arc::new(endpoint_url(server_url)?.to_string()),
            cancel,
        }))
}

async fn updates(State(api): State<Api>, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    let body: BusinessRequest<UpdatesArgs> =
        match session_api::read_authenticated(&api.token, request).await {
            Ok(body) => body,
            Err(response) => return *response,
        };
    let work = async {
        body.validate(NAME)?;
        let limit = body.arguments.limit().ok_or_else(validation)?;
        let cursor = match body.arguments.cursor.as_deref() {
            Some(value) => Some(Cursor::parse(value).ok_or_else(validation)?),
            None => None,
        };
        let expected = body.context.principal();
        // Local data only: never requires a live central connection or central IO.
        let actor = api
            .sessions
            .authorize_local_read(&body.context.session_id, &expected)
            .await
            .map_err(session_error)?;
        if actor.agent_id.is_none() {
            return Err(session_error(SessionError::AgentUnregistered));
        }
        let (path, origin, reader) = (api.config_path.clone(), api.origin.clone(), actor.clone());
        let data = tokio::task::spawn_blocking(move || {
            Feed::load(&path, &origin, &reader).map(|feed| feed.page(cursor.as_ref(), limit))
        })
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or_else(|| TaskError::rejected("LOCAL_UPDATES_UNAVAILABLE", 503))?;
        // Reads may not publish data to an owner replaced during storage IO.
        api.sessions
            .authorize_local_read(&body.context.session_id, &actor)
            .await
            .map_err(session_error)?;
        Ok(data)
    };
    let result = tokio::select! {
        _ = api.cancel.cancelled() => Err(TaskError::unknown("BROKER_UNAVAILABLE")),
        result = timeout_at(deadline, work) => result.unwrap_or_else(|_| Err(TaskError::unknown("BROKER_TIMEOUT"))),
    };
    business_api::response(&body.request_id, None, result)
}

fn validation() -> TaskError {
    TaskError::rejected("VALIDATION_FAILED", 400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_syntax_is_strict_and_bounded() {
        let tag = "0123456789abcdef0123456789abcdef";
        let valid = Cursor::parse(&format!("v1.0.12.{tag}")).unwrap();
        assert_eq!(
            valid.at,
            Position {
                received: 0,
                sent: 12,
                messages: 0,
                overdue: 0,
            }
        );
        assert_eq!(valid.version, CursorVersion::V1);
        let v2 = Cursor::parse(&format!("v2.1.2.3.{tag}")).unwrap();
        assert_eq!(
            v2.at,
            Position {
                received: 1,
                sent: 2,
                messages: 3,
                overdue: 0,
            }
        );
        assert_eq!(v2.version, CursorVersion::V2);
        let current = Cursor::parse(&format!("v3.1.2.3.4.{tag}")).unwrap();
        assert_eq!(
            current.at,
            Position {
                received: 1,
                sent: 2,
                messages: 3,
                overdue: 4,
            }
        );
        assert_eq!(current.version, CursorVersion::V3);
        for invalid in [
            String::new(),
            format!("v2.0.0.{tag}"),
            format!("v1.0.0.0.{tag}"),
            format!("v3.0.0.0.{tag}"),
            format!("v4.0.0.0.0.{tag}"),
            format!("v2.0.0.01.{tag}"),
            format!("v3.0.0.0.01.{tag}"),
            format!("v1.00.0.{tag}"),
            format!("v1.-1.0.{tag}"),
            format!("v1.0.0.{}", tag.to_uppercase()),
            format!("v1.0.0.{tag}0"),
            format!("v1.0.0.{tag}.extra"),
            format!("v1.99999999999.0.{tag}"),
            "x".repeat(MAX_CURSOR + 1),
        ] {
            assert!(Cursor::parse(&invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn limit_defaults_and_rejects_out_of_range_values() {
        let args = |limit| UpdatesArgs {
            cursor: None,
            limit,
        };
        assert_eq!(args(None).limit(), Some(DEFAULT_LIMIT));
        assert_eq!(args(Some(MAX_LIMIT as u64)).limit(), Some(MAX_LIMIT));
        assert_eq!(args(Some(0)).limit(), None);
        assert_eq!(args(Some(MAX_LIMIT as u64 + 1)).limit(), None);
    }

    fn message(id: u64) -> TaskMessage {
        TaskMessage {
            event_id: id.to_string(),
            task_id: 7,
            sequence: id.to_string(),
            event_type: TaskMessageType::Question,
            created_at: "2026-11-05T10:00:00Z".into(),
        }
    }
    fn overdue(task: u64) -> OverdueEvent {
        OverdueEvent {
            event_id: format!("overdue:{task}"),
            origin: "ws://127.0.0.1:9/".into(),
            requester_agent_id: 3,
            team_id: 1,
            task_id: task,
            minutes: 30,
            observed_at_ms: 0,
        }
    }

    #[test]
    fn v1_cursor_resumes_old_sources_and_lists_every_arrival() {
        let feed = Feed {
            scope: "scope".into(),
            received: Vec::new(),
            sent: Vec::new(),
            messages: vec![message(1), message(2)],
            overdue: Vec::new(),
        };
        // A cursor saved before this upgrade digests only the two original sources.
        let legacy = Cursor::parse(&format!(
            "v1.0.0.{}",
            feed.tag(Position::default(), CursorVersion::V1)
        ))
        .unwrap();
        let batch = feed.take(Some(&legacy), 10);
        assert!(!batch.reset);
        assert_eq!(batch.updates.len(), 2);
        assert!(batch.next.starts_with("v3.0.0.2.0."));
        let next = Cursor::parse(&batch.next).unwrap();
        assert!(feed.take(Some(&next), 10).updates.is_empty());
        // A v1 tag is not accepted as a v2 position and vice versa.
        let tag = feed.tag(Position::default(), CursorVersion::V1);
        assert!(
            feed.take(Cursor::parse(&format!("v2.0.0.0.{tag}")).as_ref(), 10)
                .reset
        );
    }

    #[test]
    fn v2_cursor_resumes_and_then_lists_overdue_advisories() {
        let feed = Feed {
            scope: "scope".into(),
            received: Vec::new(),
            sent: Vec::new(),
            messages: vec![message(1)],
            overdue: vec![overdue(40), overdue(41)],
        };
        let at = Position {
            messages: 1,
            ..Position::default()
        };
        // A v2 cursor that consumed the message predates the overdue source.
        let v2 = Cursor::parse(&format!("v2.0.0.1.{}", feed.tag(at, CursorVersion::V2))).unwrap();
        let batch = feed.take(Some(&v2), 1);
        assert!(!batch.reset && batch.has_more);
        assert!(matches!(batch.updates[..], [Update::Overdue(e)] if e.task_id == 40));
        let next = Cursor::parse(&batch.next).unwrap();
        let rest = feed.take(Some(&next), 10);
        assert!(matches!(rest.updates[..], [Update::Overdue(e)] if e.task_id == 41));
        assert!(!rest.has_more);
        let page = feed.page(None, 10);
        assert_eq!(
            page["updates"][1],
            json!({"eventId": "overdue:40", "kind": "TASK_OVERDUE", "direction": "sent", "taskId": "40", "minutes": 30})
        );
        // A v3 cursor whose overdue part no longer matches restarts from the beginning.
        let stale = format!("v3.0.0.1.2.{}", feed.tag(at, CursorVersion::V3));
        assert!(feed.take(Cursor::parse(&stale).as_ref(), 10).reset);
    }
}
