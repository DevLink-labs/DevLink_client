//! Safe partial diagnostics. Unknown stages cannot imply setup success.
use crate::{
    central::ConnectionStatus,
    context_api::{ContextFailure, ContextReader},
    sessions::{Identifier, IdentityError},
};
use axum::{
    Json, Router,
    extract::{Request, State},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct Api {
    token: Arc<String>,
    broker_url: String,
    reader: ContextReader,
}
pub(crate) fn router(token: Arc<String>, broker_url: String, reader: ContextReader) -> Router {
    Router::new()
        .route("/v1/connection", get(connection))
        .with_state(Api {
            token,
            broker_url,
            reader,
        })
}

#[derive(Clone)]
struct SetupApi {
    local: Api,
    runtime: Arc<crate::local::Runtime>,
}

pub(crate) fn setup_router(
    token: Arc<String>,
    broker_url: String,
    reader: ContextReader,
    runtime: Arc<crate::local::Runtime>,
) -> Router {
    Router::new()
        .route("/v1/setup-connection", get(setup_connection))
        .with_state(SetupApi {
            local: Api {
                token,
                broker_url,
                reader,
            },
            runtime,
        })
}

async fn setup_connection(State(api): State<SetupApi>, request: Request) -> Response {
    let (id, instance) =
        match crate::context_api::authenticate(&api.runtime.control_token, request).await {
            Ok(value) => value,
            Err(response) => return *response,
        };
    if instance.as_str() != api.runtime.instance_id {
        return axum::http::StatusCode::CONFLICT.into_response();
    }
    let data = observe(api.local, &id, None).await;
    Json(json!({"requestId": id, "data": data})).into_response()
}
fn credentials(api: &Api) -> &'static str {
    match std::fs::symlink_metadata(crate::credentials::path(api.reader.config_path())) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "missing",
        Err(_) => "unknown",
        Ok(_) => match crate::credentials::load(api.reader.config_path()) {
            Ok(value) if value.local_token == *api.token && value.broker_url == api.broker_url => {
                "valid"
            }
            _ => "invalid",
        },
    }
}
fn snapshot(
    status: ConnectionStatus,
    context: Result<Value, ContextFailure>,
    credentials: &str,
    id: &Identifier,
) -> Value {
    let mut identity = "unknown";
    let mut session = "not_checked";
    let mut issue = None;
    let mut actions = vec![];
    match context {
        Ok(value) if value["identity"] == "verified" && status == ConnectionStatus::Connected => {
            identity = "verified";
            session = match value["hostSession"].as_str() {
                Some("bound") => "bound",
                Some("unbound") => "unbound",
                Some("conflict") => "conflict",
                Some("not_checked") => "not_checked",
                _ => "unknown",
            };
        }
        Err(ContextFailure::Identity(IdentityError::Unauthenticated)) => {
            identity = "unauthenticated";
            issue = Some(
                json!({"code":"CENTRAL_UNAUTHENTICATED","scope":"domain","outcome":"rejected","requestId":id}),
            );
            actions.push("stop_then_reauthenticate_agent");
        }
        Err(ContextFailure::Identity(IdentityError::Forbidden)) => {
            identity = "mismatch";
            issue = Some(
                json!({"code":"BROKER_FORBIDDEN","scope":"domain","outcome":"rejected","requestId":id}),
            );
            actions.push("inspect_profile_then_rebind");
        }
        Err(ContextFailure::Identity(IdentityError::Unavailable)) => {
            issue = Some(
                json!({"code":"CENTRAL_UNAVAILABLE","scope":"transport","outcome":"unknown","requestId":id}),
            );
            actions.push("check_network_then_retry");
        }
        Err(ContextFailure::LocalUnavailable) => {
            issue = Some(
                json!({"code":"BROKER_CONTEXT_UNAVAILABLE","scope":"domain","outcome":"unknown","requestId":id}),
            );
            actions.push("inspect_profile_then_rebind");
        }
        _ => {}
    }
    match status {
        ConnectionStatus::AuthFailed => {
            identity = "unauthenticated";
            issue = Some(
                json!({"code":"CENTRAL_UNAUTHENTICATED","scope":"domain","outcome":"rejected","requestId":id}),
            );
            if !actions.contains(&"stop_then_reauthenticate_agent") {
                actions.push("stop_then_reauthenticate_agent");
            }
        }
        ConnectionStatus::Replaced => actions.push("inspect_connection_owner"),
        ConnectionStatus::ProtocolViolation => actions.push("check_provider_contract"),
        ConnectionStatus::InboxFailed => actions.push("repair_inbox_then_restart"),
        ConnectionStatus::Connected => {}
        _ => {
            if !actions.contains(&"check_network_then_retry") {
                actions.push("check_network_then_retry");
            }
        }
    }
    match credentials {
        "missing" => actions.push("prepare_cli_profile"),
        "invalid" => actions.push("repair_private_file"),
        "unknown" => actions.push("retry_diagnosis"),
        _ => {}
    }
    match session {
        "unbound" => actions.push("register_agent"),
        "conflict" => actions.push("inspect_connection_owner"),
        _ => {}
    }
    // CLI Connection.inspect can add McpConfig.inspect evidence. This endpoint
    // cannot know which Host config the caller loaded and must not invent it.
    actions.push("review_mcp_config");
    actions.dedup();
    let mut data = json!({"observedAt":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).expect("UTC timestamp"),
        "broker":"running","central":status,"identity":identity,"credentials":credentials,"mcpConfig":"unknown",
        "hostSession":session,"setupReady":false,"collaborationReady":false,"nextActions":actions});
    if let Some(issue) = issue {
        data["issue"] = issue;
    }
    data
}
async fn connection(State(api): State<Api>, request: Request) -> Response {
    let (id, instance) = match crate::context_api::authenticate(&api.token, request).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let data = observe(api, &id, Some(&instance)).await;
    Json(json!({"requestId":id,"data":data})).into_response()
}

async fn observe(api: Api, id: &Identifier, instance: Option<&Identifier>) -> Value {
    let before = api.reader.central.context_snapshot().await;
    let mut context = api.reader.diagnose_for(instance).await;
    let credential_api = api.clone();
    let credential_state = tokio::task::spawn_blocking(move || credentials(&credential_api))
        .await
        .unwrap_or("unknown");
    let after = api.reader.central.context_snapshot().await;
    if before.0 != after.0 || before.1 != after.1 {
        context = Err(IdentityError::Unavailable.into());
    }
    let principal = instance
        .is_none()
        .then(|| {
            context
                .as_ref()
                .ok()
                .and_then(|value| value.get("principal"))
                .cloned()
        })
        .flatten();
    let mut data = snapshot(after.1, context, credential_state, id);
    if data["identity"] == "verified"
        && let Some(principal) = principal
    {
        data["principal"] = principal;
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id() -> Identifier {
        Identifier::parse("diagnostic").unwrap()
    }
    #[tokio::test]
    async fn handler_auth_correlation_and_current_snapshot_match_the_wire_contract() {
        use axum::{body::Body, http::StatusCode};
        let (_dir, reader, _reply, task) = crate::context_api::tests::fixture().await;
        let url = "http://127.0.0.1:43127";
        let published = crate::credentials::publish(reader.config_path(), url).unwrap();
        let api = Api {
            token: Arc::new(published.local_token),
            broker_url: url.into(),
            reader,
        };
        let request = |token: &str| {
            Request::builder()
                .header("x-request-id", "diagnostic-1")
                .header("x-instance-id", "host")
                .header("authorization", format!("Bearer {token}"))
        };
        let response = connection(
            State(api.clone()),
            request("control").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = connection(
            State(api.clone()),
            request(&api.token)
                .header("origin", "null")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let response = connection(
            State(api.clone()),
            request(&api.token).body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["requestId"], "diagnostic-1");
        assert_eq!(value["data"]["credentials"], "valid");
        assert_eq!(value["data"]["identity"], "verified");
        assert_eq!(value["data"]["hostSession"], "unbound");
        assert_eq!(value["data"]["setupReady"], false);
        assert!(
            !String::from_utf8(bytes.to_vec())
                .unwrap()
                .contains(api.token.as_str())
        );
        assert_eq!(api.reader.central.presence().await, None);
        task.abort();
    }
    #[test]
    fn credentials_require_the_private_current_broker_snapshot() {
        use crate::{
            central::CentralState,
            sessions::{SessionManager, UnconfiguredIdentityProvider},
        };
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        crate::config::init(&config).unwrap();
        let central = CentralState::new(Arc::new(tokio::sync::RwLock::new(
            ConnectionStatus::Connecting,
        )));
        let sessions = SessionManager::new(Arc::new(UnconfiguredIdentityProvider), central.clone());
        let reader = ContextReader::new(central, sessions, config.clone()).unwrap();
        let mut api = Api {
            token: Arc::new("synthetic-token".into()),
            broker_url: "http://127.0.0.1:43127".into(),
            reader,
        };
        assert_eq!(credentials(&api), "missing");
        let published = crate::credentials::publish(&config, &api.broker_url).unwrap();
        assert_eq!(credentials(&api), "invalid");
        api.token = Arc::new(published.local_token);
        assert_eq!(credentials(&api), "valid");
        api.broker_url = "http://127.0.0.1:43128".into();
        assert_eq!(credentials(&api), "invalid");
    }
    #[test]
    fn connected_and_bound_cannot_hide_unknown_host_config() {
        let value = snapshot(
            ConnectionStatus::Connected,
            Ok(json!({"identity":"verified","hostSession":"bound"})),
            "valid",
            &id(),
        );
        assert_eq!(value["identity"], "verified");
        assert_eq!(value["hostSession"], "bound");
        assert_eq!(value["mcpConfig"], "unknown");
        assert_eq!(value["setupReady"], false);
        assert_eq!(value["collaborationReady"], false);
    }
    #[test]
    fn failures_preserve_safe_reason_and_no_success_or_secret() {
        let local = snapshot(
            ConnectionStatus::Connected,
            Err(ContextFailure::LocalUnavailable),
            "unknown",
            &id(),
        );
        assert_eq!(local["identity"], "unknown");
        assert_eq!(local["issue"]["code"], "BROKER_CONTEXT_UNAVAILABLE");
        assert!(
            local["nextActions"]
                .as_array()
                .unwrap()
                .contains(&json!("inspect_profile_then_rebind"))
        );
        assert!(
            local["nextActions"]
                .as_array()
                .unwrap()
                .contains(&json!("retry_diagnosis"))
        );
        for (error, identity, code) in [
            (IdentityError::Unavailable, "unknown", "CENTRAL_UNAVAILABLE"),
            (IdentityError::Forbidden, "mismatch", "BROKER_FORBIDDEN"),
            (
                IdentityError::Unauthenticated,
                "unauthenticated",
                "CENTRAL_UNAUTHENTICATED",
            ),
        ] {
            let value = snapshot(
                ConnectionStatus::Connected,
                Err(error.into()),
                "valid",
                &id(),
            );
            assert_eq!(value["identity"], identity);
            assert_eq!(value["issue"]["code"], code);
            assert_eq!(value["issue"]["requestId"], "diagnostic");
            assert_eq!(value["setupReady"], false);
            assert_eq!(value["hostSession"], "not_checked");
        }
    }
    #[test]
    fn disconnected_and_conflicting_sessions_have_explicit_actions() {
        let value = snapshot(
            ConnectionStatus::Reconnecting,
            Ok(json!({"identity":"unavailable"})),
            "missing",
            &id(),
        );
        assert_eq!(value["identity"], "unknown");
        assert_eq!(value["hostSession"], "not_checked");
        assert!(
            value["nextActions"]
                .as_array()
                .unwrap()
                .contains(&json!("prepare_cli_profile"))
        );
        let value = snapshot(
            ConnectionStatus::Connected,
            Ok(json!({"identity":"verified","hostSession":"conflict"})),
            "valid",
            &id(),
        );
        assert!(
            value["nextActions"]
                .as_array()
                .unwrap()
                .contains(&json!("inspect_connection_owner"))
        );
    }
}
