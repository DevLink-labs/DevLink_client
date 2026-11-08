//! Team-part context read provider. Read-only; never changes a task or an Agent.
use crate::{
    business_api::{self, BusinessRequest, LOCAL_BUDGET, session_error},
    session_api,
    sessions::SessionManager,
    task_client::{TaskClient, TaskError},
};
use axum::{
    Router,
    extract::{Request, State},
    response::Response,
    routing::post,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

/// Central `AgentPart` values. The broker validates only the shape; central owns the policy.
pub const PARTS: [&str; 4] = ["FRONTEND", "BACKEND", "INFRA", "OTHER"];

#[derive(Clone)]
struct Api {
    token: Arc<String>,
    sessions: SessionManager,
    central: TaskClient,
    cancel: CancellationToken,
}
pub fn router(
    token: Arc<String>,
    sessions: SessionManager,
    central: TaskClient,
    cancel: CancellationToken,
) -> Router {
    Router::new()
        .route("/v1/teams/context", post(context))
        .with_state(Api {
            token,
            sessions,
            central,
            cancel,
        })
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContextArguments {
    part: String,
    #[serde(default, deserialize_with = "crate::sessions::present")]
    counterpart: Option<String>,
}

async fn context(State(api): State<Api>, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    let body: BusinessRequest<ContextArguments> =
        match session_api::read_authenticated(&api.token, request).await {
            Ok(body) => body,
            Err(response) => return *response,
        };
    let work = async {
        body.validate("get_team_context")?;
        let args = &body.arguments;
        if !PARTS.contains(&args.part.as_str())
            || args
                .counterpart
                .as_deref()
                .is_some_and(|part| !PARTS.contains(&part))
        {
            return Err(TaskError::rejected("VALIDATION_FAILED", 400));
        }
        let actor = api
            .sessions
            .authorize(&body.context.session_id, &body.context.principal())
            .await
            .map_err(session_error)?;
        let data = api
            .central
            .team_context(&actor, &args.part, args.counterpart.as_deref())
            .await?;
        // Reads may not publish data to an owner replaced during network IO.
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
