//! Local advisory update feed over the durable inbox and requester notifications.
//! Read-only: never acknowledges, deletes or marks events read, and never calls central.
//! Central task APIs stay authoritative; a missing local update is not evidence of no change.
use crate::{
    business_api::{self, BusinessRequest, LOCAL_BUDGET, session_error},
    central::endpoint_url,
    inbox::Inbox,
    notifications::{Notifications, TaskNotification},
    protocol::TaskCreatedMessage,
    session_api,
    sessions::{Principal, SessionError, SessionManager},
    task_client::TaskError,
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

/// Positions in the two append-only, actor-scoped sources.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Position {
    received: usize,
    sent: usize,
}

/// One actor's view of both stores, in persisted order.
pub(crate) struct Feed {
    scope: String,
    received: Vec<TaskCreatedMessage>,
    sent: Vec<TaskNotification>,
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
        Ok(Self {
            // Binding the cursor to the full actor scope keeps it from widening another view.
            scope: format!(
                "{origin}\n{}\n{}\n{agent_id}",
                actor.user_id.as_str(),
                actor.team_id.as_str()
            ),
            received,
            sent,
        })
    }

    /// Digest of the actor scope and every consumed eventId, so a cleared store that
    /// later re-delivers the same last event at the same position cannot pass.
    fn tag(&self, at: Position) -> String {
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
        digest.finalize()[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn cursor(&self, at: Position) -> String {
        format!("v1.{}.{}.{}", at.received, at.sent, self.tag(at))
    }

    /// A cursor from a cleared store, another scope or a replaced history restarts from
    /// the beginning instead of skipping or inventing events.
    fn resolve(&self, cursor: Option<&Cursor>) -> (Position, bool) {
        let Some(cursor) = cursor else {
            return (Position::default(), false);
        };
        let at = cursor.at;
        if at.received <= self.received.len()
            && at.sent <= self.sent.len()
            && self.tag(at) == cursor.tag
        {
            (at, false)
        } else {
            (Position::default(), true)
        }
    }

    pub(crate) fn page(&self, cursor: Option<&Cursor>, limit: usize) -> Value {
        let (mut at, reset) = self.resolve(cursor);
        let mut updates = Vec::with_capacity(limit);
        for message in self.received[at.received..].iter().take(limit) {
            updates.push(json!({
                "eventId": message.event_id,
                "kind": "TASK_CREATED",
                "direction": "received",
                "taskId": message.payload.task_id.to_string(),
                "requesterAgentId": message.payload.requester_agent_id.to_string(),
            }));
            at.received += 1;
        }
        for event in self.sent[at.sent..].iter().take(limit - updates.len()) {
            let kind = match event {
                TaskNotification::TaskStatusChanged { .. } => "TASK_STATUS_CHANGED",
                TaskNotification::TaskCompleted { .. } => "TASK_COMPLETED",
            };
            updates.push(json!({
                "eventId": event.event_id(),
                "kind": kind,
                "direction": "sent",
                "taskId": event.task_id().to_string(),
                "taskVersion": event.version().to_string(),
                "status": event.status(),
            }));
            at.sent += 1;
        }
        json!({
            "updates": updates,
            "nextCursor": self.cursor(at),
            "hasMore": at.received < self.received.len() || at.sent < self.sent.len(),
            "cursorReset": reset,
            "coverage": "local",
        })
    }
}

/// A syntactically valid cursor; whether it still matches the stores is decided by Feed.
pub(crate) struct Cursor {
    at: Position,
    tag: String,
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
        let mut parts = value.split('.');
        let (Some("v1"), Some(received), Some(sent), Some(tag), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return None;
        };
        let tag_valid = tag.len() == 32
            && tag
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        Some(Self {
            at: Position {
                received: position(received)?,
                sent: position(sent)?,
            },
            tag: tag_valid.then(|| tag.to_owned())?,
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
                sent: 12
            }
        );
        for invalid in [
            String::new(),
            format!("v2.0.0.{tag}"),
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
}
