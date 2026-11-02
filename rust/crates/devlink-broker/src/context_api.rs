//! Read-only, live-fenced onboarding context. No session lifecycle operations.
use crate::{
    agent_profile::{self, InspectionBinding},
    central::{CentralState, ConnectionStatus},
    sessions::{Identifier, IdentityError, SessionError, SessionManager},
};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Clone)]
pub(crate) struct ContextReader {
    pub central: CentralState,
    sessions: SessionManager,
    config: PathBuf,
    binding: Option<InspectionBinding>,
    origin: String,
}
impl ContextReader {
    pub fn new(
        central: CentralState,
        sessions: SessionManager,
        config: PathBuf,
    ) -> anyhow::Result<Self> {
        let binding = agent_profile::inspection_binding(&config)?;
        let origin = crate::user_auth::origin(&crate::config::Config::load(&config)?)?.to_string();
        Ok(Self {
            central,
            sessions,
            config,
            binding,
            origin,
        })
    }
    fn check_binding(&self) -> Result<(), IdentityError> {
        let current = agent_profile::inspection_binding(&self.config)
            .map_err(|_| IdentityError::Forbidden)?;
        let config =
            crate::config::Config::load(&self.config).map_err(|_| IdentityError::Forbidden)?;
        if current != self.binding
            || crate::user_auth::origin(&config)
                .map_err(|_| IdentityError::Forbidden)?
                .as_str()
                != self.origin
        {
            return Err(IdentityError::Forbidden);
        }
        Ok(())
    }
    pub async fn read(&self, instance: &Identifier) -> Result<Value, IdentityError> {
        self.check_binding()?;
        let (generation, status, identity) = self.central.context_snapshot().await;
        if status == ConnectionStatus::AuthFailed {
            return Err(IdentityError::Unauthenticated);
        }
        if status != ConnectionStatus::Connected {
            return Ok(
                json!({"identity":"unavailable","principal":null,"agent":null,
                "project":{"path":null,"source":"unavailable"},"hostSession":"not_checked"}),
            );
        }
        let identity = identity.ok_or(IdentityError::Unavailable)?;
        let client = self
            .central
            .context_client
            .as_ref()
            .ok_or(IdentityError::Unavailable)?;
        let context = client.get().await?;
        if identity.agent_id != context.agent_id
            || identity
                .user_id
                .is_some_and(|id| id != context.owner_user_id)
            || identity.team_id.is_some_and(|id| id != context.team_id)
            || self.binding.as_ref().is_some_and(|b| {
                b.user_id != context.owner_user_id
                    || b.team_id != context.team_id
                    || b.agent_id != context.agent_id
            })
        {
            return Err(IdentityError::Forbidden);
        }
        let session = self.sessions.inspect(instance, &context).await?;
        self.check_binding()?;
        let (current_generation, current_status, current_identity) =
            self.central.context_snapshot().await;
        if current_generation != generation
            || current_status != status
            || current_identity.as_ref().is_none_or(|current| {
                current.connection_id != identity.connection_id
                    || current.agent_id != context.agent_id
                    || current
                        .user_id
                        .is_some_and(|id| id != context.owner_user_id)
                    || current.team_id.is_some_and(|id| id != context.team_id)
            })
        {
            return Err(IdentityError::Unavailable);
        }
        let tool = context.tool_type.map(|tool| match tool {
            agent_profile::Tool::Codex => "codex",
            agent_profile::Tool::ClaudeCode => "claude_code",
        });
        Ok(json!({"identity":"verified",
            "principal":{"userId":context.owner_user_id.to_string(),"teamId":context.team_id.to_string(),
                "agentId":context.agent_id.to_string(),"toolType":tool},
            "agent":{"name":context.name,"roleDescription":context.role_description,
                "toolType":tool,"projectName":context.project_name,"source":"central_agents_me"},
            "project":{"path":self.binding.as_ref().map(|b| &b.project_root),
                "source":if self.binding.is_some(){"bound_profile"}else{"unavailable"}},
            "hostSession":session}))
    }
}

#[derive(Clone)]
struct Api {
    token: Arc<String>,
    reader: ContextReader,
}
pub(crate) fn router(token: Arc<String>, reader: ContextReader) -> Router {
    Router::new()
        .route("/v1/context", get(context))
        .with_state(Api { token, reader })
}
fn failure(id: &str, status: StatusCode, code: &str, scope: &str) -> Response {
    (
        status,
        Json(json!({"requestId":id,"error":{"code":code,"scope":scope,"outcome":"rejected"}})),
    )
        .into_response()
}
/// GET correlation/selectors are headers, never a JSON body or caller principal.
pub(crate) async fn authenticate(
    token: &str,
    request: Request,
) -> Result<(Identifier, Identifier), Box<Response>> {
    let (id, instance) = {
        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| Identifier::parse(v).ok())
        };
        (
            header("x-request-id")
                .unwrap_or_else(|| Identifier::parse(uuid::Uuid::new_v4().to_string()).unwrap()),
            header("x-instance-id"),
        )
    };
    if request.headers().contains_key("origin") {
        return Err(Box::new(failure(
            id.as_str(),
            StatusCode::FORBIDDEN,
            "LOCAL_AUTH_FORBIDDEN",
            "local_auth",
        )));
    }
    if !request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.strip_prefix("Bearer ") == Some(token))
    {
        return Err(Box::new(failure(
            id.as_str(),
            StatusCode::UNAUTHORIZED,
            "BROKER_UNAUTHORIZED",
            "local_auth",
        )));
    }
    if instance.is_none() || request.uri().query().is_some() {
        return Err(Box::new(failure(
            id.as_str(),
            StatusCode::BAD_REQUEST,
            "VALIDATION_FAILED",
            "domain",
        )));
    }
    if !matches!(
        tokio::time::timeout(
            Duration::from_secs(1),
            axum::body::to_bytes(request.into_body(), 0)
        )
        .await,
        Ok(Ok(_))
    ) {
        return Err(Box::new(failure(
            id.as_str(),
            StatusCode::BAD_REQUEST,
            "VALIDATION_FAILED",
            "domain",
        )));
    }
    Ok((id, instance.unwrap()))
}
async fn context(State(api): State<Api>, request: Request) -> Response {
    let (id, instance) = match authenticate(&api.token, request).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match api.reader.read(&instance).await {
        Ok(data) => Json(json!({"requestId":id,"data":data})).into_response(),
        // HTTP 403 on local control reads is reserved for local credential/Origin failures.
        Err(IdentityError::Forbidden) => failure(
            id.as_str(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "BROKER_FORBIDDEN",
            "domain",
        ),
        Err(e) => crate::session_api::session_failure(id.as_str(), SessionError::Identity(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_context::ContextClient, central::CentralIdentity, sessions::ReadyIdentityProvider,
    };
    use tokio::{net::TcpListener, sync::RwLock};

    async fn fixture() -> (
        tempfile::TempDir,
        ContextReader,
        Arc<RwLock<Value>>,
        tokio::task::JoinHandle<()>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        crate::config::init(&config).unwrap();
        let reply = Arc::new(RwLock::new(
            json!({"success":true,"data":{"agentId":12,"ownerUserId":3,"teamId":1}}),
        ));
        let shared = reply.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = ContextClient::new(
            &format!("ws://{}", listener.local_addr().unwrap()),
            "synthetic-central-token",
        )
        .unwrap();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route(
                    "/api/v1/agents/me",
                    get(move || {
                        let r = shared.clone();
                        async move { Json(r.read().await.clone()) }
                    }),
                ),
            )
            .await
            .unwrap();
        });
        let central = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
            .with_context_client(client);
        let handle = central.begin_session().await;
        handle
            .set_identity(CentralIdentity {
                connection_id: "connection".into(),
                agent_id: 12,
                user_id: Some(3),
                team_id: Some(1),
            })
            .await;
        handle.set_status(ConnectionStatus::Connected).await;
        let sessions = SessionManager::new(
            Arc::new(ReadyIdentityProvider::new(central.clone())),
            central.clone(),
        );
        (
            dir,
            ContextReader::new(central, sessions, config).unwrap(),
            reply,
            task,
        )
    }
    fn host() -> Identifier {
        Identifier::parse("host").unwrap()
    }
    #[tokio::test]
    async fn auth_failed_is_not_an_ordinary_disconnect() {
        let (_dir, reader, _reply, task) = fixture().await;
        reader
            .central
            .begin_session()
            .await
            .set_status(ConnectionStatus::AuthFailed)
            .await;
        assert_eq!(
            reader.read(&host()).await.unwrap_err(),
            IdentityError::Unauthenticated
        );
        task.abort();
    }
    #[tokio::test]
    async fn late_identity_reply_from_an_old_generation_is_rejected() {
        let (_dir, mut reader, _reply, old_task) = fixture().await;
        let central = reader.central.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        reader.central.context_client = Some(
            ContextClient::new(
                &format!("ws://{}", listener.local_addr().unwrap()),
                "synthetic-token",
            )
            .unwrap(),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener,Router::new().route("/api/v1/agents/me",get(move || {
                let central=central.clone(); async move {
                    let handle=central.begin_session().await;
                    handle.set_identity(CentralIdentity{connection_id:"connection".into(),agent_id:12,user_id:Some(3),team_id:Some(1)}).await;
                    handle.set_status(ConnectionStatus::Connected).await;
                    Json(json!({"success":true,"data":{"agentId":12,"ownerUserId":3,"teamId":1}}))
                }
            }))).await.unwrap();
        });
        assert_eq!(
            reader.read(&host()).await.unwrap_err(),
            IdentityError::Unavailable
        );
        old_task.abort();
        task.abort();
    }
    #[tokio::test]
    async fn domain_mismatch_uses_422_with_safe_correlation() {
        use axum::body::Body;
        let (_dir, reader, reply, task) = fixture().await;
        reply.write().await["data"]["teamId"] = json!(99);
        let response = context(
            State(Api {
                token: Arc::new("local".into()),
                reader,
            }),
            Request::builder()
                .header("authorization", "Bearer local")
                .header("x-request-id", "request-1")
                .header("x-instance-id", "host")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["requestId"], "request-1");
        assert_eq!(value["error"]["code"], "BROKER_FORBIDDEN");
        assert!(!String::from_utf8(body.to_vec()).unwrap().contains("token"));
        task.abort();
    }
    #[tokio::test]
    async fn reads_live_identity_with_missing_metadata_without_claim_or_presence() {
        let (_dir, reader, _reply, task) = fixture().await;
        for _ in 0..2 {
            let result = reader.read(&host()).await.unwrap();
            assert_eq!(result["principal"]["agentId"], "12");
            assert_eq!(result["principal"]["toolType"], Value::Null);
            assert_eq!(result["agent"]["name"], Value::Null);
            assert_eq!(result["hostSession"], "unbound");
            assert_eq!(result["project"]["source"], "unavailable");
            assert_eq!(reader.central.presence().await, None);
        }
        task.abort();
    }
    #[tokio::test]
    async fn rejects_changed_central_subject_and_config_origin() {
        let (_dir, reader, reply, task) = fixture().await;
        for field in ["ownerUserId", "teamId", "agentId"] {
            let original = reply.read().await.clone();
            reply.write().await["data"][field] = json!(99);
            assert_eq!(
                reader.read(&host()).await.unwrap_err(),
                IdentityError::Forbidden
            );
            *reply.write().await = original;
        }
        crate::config::set(&reader.config, "serverUrl", "wss://other.example").unwrap();
        assert_eq!(
            reader.read(&host()).await.unwrap_err(),
            IdentityError::Forbidden
        );
        task.abort();
    }
    #[tokio::test]
    async fn disconnected_read_does_not_report_cached_principal() {
        let (_dir, reader, _reply, task) = fixture().await;
        reader
            .central
            .begin_session()
            .await
            .set_status(ConnectionStatus::Reconnecting)
            .await;
        let result = reader.read(&host()).await.unwrap();
        assert_eq!(result["identity"], "unavailable");
        assert_eq!(result["principal"], Value::Null);
        task.abort();
    }
    #[tokio::test]
    async fn selected_project_provenance_is_fenced_when_profile_changes() {
        let (dir, reader, _reply, task) = fixture().await;
        let root = dir
            .path()
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let origin = reader.origin.clone();
        let mut profile = json!({"schemaVersion":1,"origin":origin,"userId":3,"teamId":1,
            "projectRoot":root,"state":{"kind":"ready","agent":{"agentId":12,"teamId":1,
                "ownerUserId":3,"name":"fixture","roleDescription":null,"toolType":"CODEX",
                "projectName":"project","status":"OFFLINE","lastSeenAt":null,"createdAt":"synthetic"},
                "token":"synthetic-token"}});
        crate::private_file::write(
            &crate::teams::selection_path(&reader.config),
            &json!({"schemaVersion":1,"origin":origin,"userId":3,"teamId":1}),
            16384,
        )
        .unwrap();
        crate::private_file::write(&agent_profile::path(&reader.config), &profile, 16384).unwrap();
        let reader = ContextReader::new(
            reader.central.clone(),
            reader.sessions.clone(),
            reader.config.clone(),
        )
        .unwrap();
        assert_eq!(reader.read(&host()).await.unwrap()["project"]["path"], root);
        let other = dir.path().join("other");
        std::fs::create_dir(&other).unwrap();
        profile["projectRoot"] = json!(other.canonicalize().unwrap().to_str().unwrap());
        crate::private_file::write(&agent_profile::path(&reader.config), &profile, 16384).unwrap();
        assert_eq!(
            reader.read(&host()).await.unwrap_err(),
            IdentityError::Forbidden
        );
        task.abort();
    }
    #[tokio::test]
    async fn get_auth_rejects_control_token_origin_body_and_query() {
        use axum::body::Body;
        let build = || {
            Request::builder()
                .header("x-request-id", "req")
                .header("x-instance-id", "host")
        };
        for req in [
            build()
                .header("authorization", "Bearer control")
                .body(Body::empty())
                .unwrap(),
            build()
                .header("authorization", "Bearer local")
                .header("origin", "null")
                .body(Body::empty())
                .unwrap(),
            build()
                .header("authorization", "Bearer local")
                .body(Body::from("{}"))
                .unwrap(),
            build()
                .uri("/v1/context?userId=99")
                .header("authorization", "Bearer local")
                .body(Body::empty())
                .unwrap(),
        ] {
            assert!(authenticate("local", req).await.is_err());
        }
        assert!(
            authenticate(
                "local",
                build()
                    .header("authorization", "Bearer local")
                    .body(Body::empty())
                    .unwrap()
            )
            .await
            .is_ok()
        );
    }
}
