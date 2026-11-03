//! Shared local business request, envelope, and end-to-end budget.
use crate::{
    session_api,
    sessions::{Identifier, Principal, SessionError, ToolType},
    task_client::TaskError,
};
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

pub const LOCAL_BUDGET: Duration = Duration::from_secs(8);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BusinessRequest<T> {
    pub request_id: Identifier,
    pub operation: String,
    pub arguments: T,
    pub context: ActorContext,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    pub mcp_request_id: Option<Value>,
}
impl<T> BusinessRequest<T> {
    pub fn validate(&self, operation: &str) -> Result<(), TaskError> {
        if self.operation != operation
            || self.mcp_request_id.as_ref().is_some_and(|id| {
                !id.is_number() && !id.as_str().is_some_and(|s| s.chars().count() <= 128)
            })
        {
            return Err(TaskError::rejected("VALIDATION_FAILED", 400));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActorContext {
    pub session_id: Identifier,
    pub user_id: Identifier,
    pub team_id: Identifier,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    pub agent_id: Option<Identifier>,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    pub tool_type: Option<ToolType>,
}
impl ActorContext {
    pub fn principal(&self) -> Principal {
        Principal {
            user_id: self.user_id.clone(),
            team_id: self.team_id.clone(),
            agent_id: self.agent_id.clone(),
            tool_type: self.tool_type,
        }
    }
}

pub fn session_error(error: SessionError) -> TaskError {
    let (status, code, scope) = session_api::session_error(error);
    TaskError {
        code,
        status: status.as_u16(),
        scope,
        outcome: if scope == "transport" {
            "unknown"
        } else {
            "rejected"
        },
        task_id: None,
    }
}

pub fn response(
    id: &Identifier,
    key: Option<String>,
    result: Result<Value, TaskError>,
) -> Response {
    let result = result.and_then(|data| {
        if serde_json::to_vec(&data).is_ok_and(|bytes| bytes.len() + 512 <= 256 * 1024) {
            Ok(data)
        } else {
            Err(TaskError::unknown("CENTRAL_PROTOCOL_ERROR"))
        }
    });
    let (status, mut value) = match result {
        Ok(data) => (200, json!({"requestId":id,"data":data})),
        Err(error) => {
            let mut value = json!({"requestId":id,"error":{"code":error.code,"scope":error.scope,"outcome":error.outcome}});
            if let Some(task) = error.task_id {
                value["error"]["taskId"] = json!(task.to_string());
            }
            (error.status, value)
        }
    };
    if let Some(key) = key {
        value["requestKey"] = json!(key);
    }
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(value),
    )
        .into_response()
}
