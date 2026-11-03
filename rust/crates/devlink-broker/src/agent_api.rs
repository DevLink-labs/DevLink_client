//! Agent confirmation/list providers. This router never creates or updates Agents.
use crate::{
    business_api::{self, BusinessRequest, LOCAL_BUDGET, session_error},
    session_api,
    sessions::{Identifier, SessionManager},
    task_client::{TaskClient, TaskError},
};
use axum::{
    Router,
    extract::{Request, State},
    response::Response,
    routing::post,
};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Api {
    token: Arc<String>,
    sessions: SessionManager,
    central: TaskClient,
    cancel: CancellationToken,
    context: crate::context_api::ContextReader,
}
pub fn router(
    token: Arc<String>,
    sessions: SessionManager,
    central: TaskClient,
    cancel: CancellationToken,
    context: crate::context_api::ContextReader,
) -> Router {
    Router::new()
        .route("/v1/agents/register", post(register))
        .route("/v1/agents/list", post(list))
        .with_state(Api {
            token,
            sessions,
            central,
            cancel,
            context,
        })
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfirmArguments {
    #[serde(default, deserialize_with = "crate::sessions::present")]
    name: Option<String>,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    tool_type: Option<String>,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    role: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArguments {}

async fn register(State(api): State<Api>, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    let body: BusinessRequest<ConfirmArguments> =
        match session_api::read_authenticated(&api.token, request).await {
            Ok(body) => body,
            Err(response) => return *response,
        };
    let result = tokio::select! {
        _ = api.cancel.cancelled() => Err(TaskError::unknown("CENTRAL_UNAVAILABLE")),
        result = timeout_at(deadline, confirm(&api, &body)) => result.unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT"))),
    };
    business_api::response(&body.request_id, None, result)
}
async fn confirm(api: &Api, body: &BusinessRequest<ConfirmArguments>) -> Result<Value, TaskError> {
    body.validate("register_agent")?;
    guard_profile(api).await?;
    let args = &body.arguments;
    if args
        .tool_type
        .as_deref()
        .is_some_and(|tool| !matches!(tool, "codex" | "claude_code"))
    {
        return Err(TaskError::rejected("VALIDATION_FAILED", 400));
    }
    let actor = api
        .sessions
        .authorize_registration(&body.context.session_id, &body.context.principal())
        .await
        .map_err(session_error)?;
    let data = api.central.confirm_agent(&actor).await?;
    let agent = Identifier::parse(data["agentId"].as_str().unwrap())
        .map_err(|_| TaskError::unknown("CENTRAL_PROTOCOL_ERROR"))?;
    api.sessions
        .verify_registration_agent(&body.context.session_id, &actor, &agent)
        .await
        .map_err(session_error)?;
    api.context
        .verify_registered_agent(&actor, &agent)
        .map_err(context_error)?;
    guard_profile(api).await?;
    if args
        .name
        .as_ref()
        .is_some_and(|name| data["name"].as_str() != Some(name))
        || args
            .role
            .as_ref()
            .is_some_and(|role| data["roleDescription"].as_str() != Some(role))
        || args.tool_type.as_deref().is_some_and(|tool| match tool {
            "codex" => data["toolType"] != "CODEX",
            "claude_code" => data["toolType"] != "CLAUDE_CODE",
            _ => true,
        })
    {
        return Err(TaskError::rejected("VALIDATION_FAILED", 400));
    }
    Ok(data)
}
async fn guard_profile(api: &Api) -> Result<(), TaskError> {
    api.context.validate_binding().await.map_err(context_error)
}
fn context_error(error: crate::context_api::ContextFailure) -> TaskError {
    match error {
        crate::context_api::ContextFailure::Identity(error) => {
            session_error(crate::sessions::SessionError::Identity(error))
        }
        crate::context_api::ContextFailure::LocalUnavailable => TaskError {
            code: "BROKER_CONTEXT_UNAVAILABLE",
            status: 503,
            scope: "domain",
            outcome: "unknown",
            task_id: None,
        },
    }
}
async fn list(State(api): State<Api>, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    let body: BusinessRequest<ListArguments> =
        match session_api::read_authenticated(&api.token, request).await {
            Ok(body) => body,
            Err(response) => return *response,
        };
    let work = async {
        body.validate("list_agents")?;
        let actor = api
            .sessions
            .authorize(&body.context.session_id, &body.context.principal())
            .await
            .map_err(session_error)?;
        let data = api.central.list_agents(&actor).await?;
        api.sessions
            .authorize(&body.context.session_id, &actor)
            .await
            .map_err(session_error)?;
        Ok(data)
    };
    let result = tokio::select! {
        _ = api.cancel.cancelled() => Err(TaskError::unknown("CENTRAL_UNAVAILABLE")),
        result = timeout_at(deadline, work) => result.unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT"))),
    };
    business_api::response(&body.request_id, None, result)
}
