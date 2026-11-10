//! Agent confirmation/list/recommendation providers. This router never creates Agents or
//! credentials; `update_agent` changes only the bound Agent's project name, role, part and
//! role tags (S15P31D108-146/-183), and `find_agent` only reads central recommendations
//! (S15P31D108-188).
use crate::{
    agent_metadata::AgentPart,
    agent_profile::{self, Tool, UpdateFault, UpdateFaultKind, UpdateOptions},
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
use serde_json::{Value, json};
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
        .route("/v1/agents/update", post(update))
        .route("/v1/agents/find", post(find))
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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FindArguments {
    question: String,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    repo_key: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateArguments {
    #[serde(default, deserialize_with = "crate::sessions::present")]
    project_name: Option<String>,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    role_description: Option<String>,
    // S15P31D108-183: like central, a missing or null part/roles keeps the stored value.
    #[serde(default)]
    part: Option<String>,
    #[serde(default)]
    roles: Option<Vec<String>>,
}

/// The central PATCH accepts only the owner's user session, so this route uses the
/// CLI login on this machine. The verified session Agent must be the bound Agent.
async fn update(State(api): State<Api>, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    let body: BusinessRequest<UpdateArguments> =
        match session_api::read_authenticated(&api.token, request).await {
            Ok(body) => body,
            Err(response) => return *response,
        };
    let result = tokio::select! {
        _ = api.cancel.cancelled() => Err(TaskError::unknown("CENTRAL_UNAVAILABLE")),
        result = timeout_at(deadline, change(&api, &body)) => result.unwrap_or_else(|_| Err(TaskError::unknown("CENTRAL_TIMEOUT"))),
    };
    business_api::response(&body.request_id, None, result)
}
async fn change(api: &Api, body: &BusinessRequest<UpdateArguments>) -> Result<Value, TaskError> {
    body.validate("update_agent")?;
    guard_profile(api).await?;
    let actor = api
        .sessions
        .authorize(&body.context.session_id, &body.context.principal())
        .await
        .map_err(session_error)?;
    let agent = actor
        .agent_id
        .clone()
        .ok_or(TaskError::rejected("FORBIDDEN", 403))?;
    api.context
        .verify_registered_agent(&actor, &agent)
        .map_err(context_error)?;
    let expected = agent
        .as_str()
        .parse::<u64>()
        .map_err(|_| TaskError::rejected("FORBIDDEN", 403))?;
    let part = body
        .arguments
        .part
        .as_deref()
        .map(AgentPart::parse)
        .transpose()
        .map_err(|_| TaskError::rejected("VALIDATION_FAILED", 400))?;
    let options = UpdateOptions {
        project_name: body.arguments.project_name.clone(),
        role_description: body.arguments.role_description.clone(),
        part,
        roles: body.arguments.roles.clone(),
    };
    let prepared =
        agent_profile::prepare_update(api.context.config_path(), options, Some(expected))
            .await
            .map_err(update_error)?;
    // User/team/Agent/CSRF preflight may have yielded to a fresh claim, release,
    // or binding change. Fence again before sending any mutation (also for no-op
    // reads). Do not fence after dispatch: a confirmed mutation stays confirmed.
    api.central.confirm_agent(&actor).await?;
    guard_profile(api).await?;
    api.sessions
        .authorize(&body.context.session_id, &actor)
        .await
        .map_err(session_error)?;
    let result = prepared.commit().await.map_err(update_error)?;
    let agent = &result.agent;
    let tool = match agent.tool_type {
        Tool::Codex => "codex",
        Tool::ClaudeCode => "claude_code",
    };
    let mut shown = json!({"agentId":agent.agent_id.to_string(),"teamId":agent.team_id.to_string(),
        "name":agent.name,"roleDescription":agent.role_description,"toolType":tool,
        "projectName":agent.project_name});
    // Only what central reported: a reply without part/roles must not read as OTHER / no tags.
    if let Some(part) = result.part() {
        shown["part"] = json!(part);
    }
    if let Some(roles) = result.roles() {
        shown["roles"] = json!(roles);
    }
    Ok(json!({"agent":shown,
        "updated":!result.changed.is_empty(),"changedFields":result.changed,
        "credentialsRotated":false,"localProfileSynced":result.local_profile_synced}))
}
/// Value-free classification. A failure after the PATCH was sent stays unknown.
fn update_error(error: anyhow::Error) -> TaskError {
    use crate::user_auth::{ApiRejection, AuthFault, AuthFaultKind as K};
    let login = || TaskError::rejected("USER_LOGIN_REQUIRED", 412);
    if let Some(fault) = error.chain().find_map(|e| e.downcast_ref::<UpdateFault>()) {
        return match fault.kind {
            UpdateFaultKind::Invalid => TaskError::rejected("VALIDATION_FAILED", 400),
            UpdateFaultKind::Binding => TaskError::rejected("FORBIDDEN", 403),
            UpdateFaultKind::Unconfirmed => TaskError::unknown("CENTRAL_PROTOCOL_ERROR"),
        };
    }
    if let Some(rejection) = error.chain().find_map(|e| e.downcast_ref::<ApiRejection>()) {
        return match rejection.status {
            401 => login(),
            400 => TaskError::rejected("VALIDATION_FAILED", 400),
            403 => TaskError::rejected("FORBIDDEN", 403),
            404 => TaskError::rejected("RESOURCE_NOT_FOUND", 404),
            status @ 402..=499 => TaskError::rejected("CENTRAL_REQUEST_REJECTED", status),
            _ => TaskError::unknown("CENTRAL_UNAVAILABLE"),
        };
    }
    if let Some(fault) = error.chain().find_map(|e| e.downcast_ref::<AuthFault>()) {
        return match fault.kind {
            K::Busy | K::ConfigBusy | K::BrokerBusy => {
                TaskError::rejected("LOCAL_ACCOUNT_BUSY", 409)
            }
            K::Unavailable => TaskError::unknown("CENTRAL_UNAVAILABLE"),
            K::Protocol => TaskError::unknown("CENTRAL_PROTOCOL_ERROR"),
            K::IdentityMismatch | K::SessionConflict => login(),
            _ => context_error(crate::context_api::ContextFailure::LocalUnavailable),
        };
    }
    let text = error.to_string();
    if [
        "USER_LOGIN_REQUIRED",
        "USER_SESSION_EXPIRED",
        "Local user session cannot be used",
    ]
    .iter()
    .any(|marker| text.contains(marker))
    {
        return login();
    }
    if text.contains("TEAM_SELECTION_REQUIRED") || text.contains("ACCESS_DENIED") {
        return TaskError::rejected("FORBIDDEN", 403);
    }
    context_error(crate::context_api::ContextFailure::LocalUnavailable)
}

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
/// S15P31D108-188: relay the central recommendation for the bound Agent's team.
/// Read-only; the result never selects a recipient or creates a task.
async fn find(State(api): State<Api>, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    let body: BusinessRequest<FindArguments> =
        match session_api::read_authenticated(&api.token, request).await {
            Ok(body) => body,
            Err(response) => return *response,
        };
    let work = async {
        body.validate("find_agent")?;
        let question = crate::agent_recommendations::question(&body.arguments.question)
            .ok_or(TaskError::rejected("VALIDATION_FAILED", 400))?;
        let repo_key = body.arguments.repo_key.as_deref();
        if repo_key.is_some_and(|key| !crate::agent_recommendations::repo_key(key)) {
            return Err(TaskError::rejected("VALIDATION_FAILED", 400));
        }
        let actor = api
            .sessions
            .authorize(&body.context.session_id, &body.context.principal())
            .await
            .map_err(session_error)?;
        let data = api.central.find_agents(&actor, question, repo_key).await?;
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
