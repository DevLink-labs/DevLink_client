//! Four direct HTTP business adapters; MCP's final DTOs belong to -97.
use crate::{
    business_api::{self, BusinessRequest, LOCAL_BUDGET, session_error},
    session_api,
    sessions::SessionManager,
    task_client::{TaskClient, TaskError},
    task_operations::{Arguments, CompleteArgs, ListArgs, ResultArgs, StatusArgs},
};
use axum::{
    Router,
    extract::{Request, State},
    response::Response,
    routing::post,
};
use std::sync::Arc;
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

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
        .route("/v1/tasks/status", post(status))
        .route("/v1/tasks/complete", post(complete))
        .route("/v1/tasks/list", post(list))
        .route("/v1/tasks/result", post(result))
        .with_state(Api {
            token,
            sessions,
            central,
            cancel,
        })
}
async fn status(State(api): State<Api>, req: Request) -> Response {
    handle::<StatusArgs>(api, req).await
}
async fn complete(State(api): State<Api>, req: Request) -> Response {
    handle::<CompleteArgs>(api, req).await
}
async fn list(State(api): State<Api>, req: Request) -> Response {
    handle::<ListArgs>(api, req).await
}
async fn result(State(api): State<Api>, req: Request) -> Response {
    handle::<ResultArgs>(api, req).await
}
async fn handle<T: Arguments>(api: Api, request: Request) -> Response {
    let deadline = Instant::now() + LOCAL_BUDGET;
    let body: BusinessRequest<T> = match session_api::read_authenticated(&api.token, request).await
    {
        Ok(body) => body,
        Err(response) => return *response,
    };
    let known = body.arguments.task_id();
    let key = body
        .arguments
        .request_key()
        .filter(|key| !key.trim().is_empty() && key.encode_utf16().count() <= 64)
        .map(str::to_owned);
    let work = async {
        body.validate(T::NAME)?;
        let expected = body.context.principal();
        let actor = api
            .sessions
            .authorize(&body.context.session_id, &expected)
            .await
            .map_err(session_error)?;
        let operation = body.arguments.prepare(&actor, &api.central.origin())?;
        api.central.confirm_agent(&actor).await?;
        api.sessions
            .authorize(&body.context.session_id, &actor)
            .await
            .map_err(session_error)?;
        let data = operation.execute(&api.central).await?;
        // Reads may not publish data to an owner replaced during network IO.
        // A dispatched mutation's known result remains known after replacement.
        if !operation.mutation() {
            api.sessions
                .authorize(&body.context.session_id, &actor)
                .await
                .map_err(session_error)?;
        }
        if !serde_json::to_vec(&data).is_ok_and(|bytes| bytes.len() + 512 <= 256 * 1024) {
            return Err(TaskError::unknown("CENTRAL_PROTOCOL_ERROR"));
        }
        Ok(data)
    };
    let result=tokio::select!{
        _=api.cancel.cancelled()=>Err(TaskError::unknown("CENTRAL_UNAVAILABLE")),
        result=timeout_at(deadline,work)=>result.unwrap_or_else(|_|Err(TaskError::unknown("CENTRAL_TIMEOUT"))),
    }.map_err(|mut error:TaskError|{error.task_id=known;error});
    business_api::response(&body.request_id, key, result)
}
