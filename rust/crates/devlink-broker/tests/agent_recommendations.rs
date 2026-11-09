//! S15P31D108-188: `find_agent` broker route against a mock central recommendation API.
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use devlink_broker::{
    central::{CentralIdentity, CentralState, ConnectionStatus},
    session_api,
    sessions::{ReadyIdentityProvider, SessionManager},
    task_client::TaskClient,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const LOCAL: &str = "synthetic-local-mcp-token-5678";
const AGENT: &str = "synthetic-central-agent-token-5678";
const SECRET: &str = "untrusted-central-message-do-not-expose";

#[derive(Clone, Default)]
struct Mock {
    mode: Arc<AtomicU8>,
    bodies: Arc<Mutex<Vec<(u64, Value)>>>,
}

async fn me(headers: HeaderMap) -> Response {
    assert_eq!(headers["authorization"], format!("Bearer {AGENT}"));
    Json(
        json!({"success":true,"data":{"agentId":12,"teamId":1,"ownerUserId":3,"name":"Requester",
        "roleDescription":"백엔드","toolType":"CODEX","projectName":"DevLink","status":"ONLINE",
        "lastSeenAt":null,"createdAt":"2026-10-29T00:00:00"}}),
    )
    .into_response()
}

fn candidate(id: u64) -> Value {
    json!({"agentId":id,"name":"로그인 담당","part":"BACKEND","connectionStatus":"OFFLINE","score":38.25,
        "scores":{"role":20.0,"git":18.25,"working":15.0,"frequency":3.0,"recency":0.25},
        "matchedRoles":["로그인"],"relatedFiles":[{"repoId":4,"relativePath":"src/auth/Login.java",
            "sources":["COMMIT"],"observedAt":"2026-11-08T09:00:00Z"}],
        "latestCommitAt":"2026-11-08T09:00:00Z","workingObservedAt":null,"insufficientReason":null,
        "credential":SECRET})
}

async fn recommend(
    Path(team): Path<u64>,
    State(mock): State<Mock>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], format!("Bearer {AGENT}"));
    mock.bodies.lock().await.push((team, body));
    let failure = |status: StatusCode, code: &str| {
        (
            status,
            Json(json!({"success":false,"code":code,"message":SECRET})),
        )
            .into_response()
    };
    match mock.mode.load(Ordering::SeqCst) {
        1 => failure(StatusCode::BAD_REQUEST, "VALIDATION_FAILED"),
        2 => failure(StatusCode::FORBIDDEN, "ACCESS_DENIED"),
        3 => failure(StatusCode::UNAUTHORIZED, "AUTHENTICATION_REQUIRED"),
        4 => failure(StatusCode::SERVICE_UNAVAILABLE, "GIT_ROUTING_UNAVAILABLE"),
        5 => failure(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL_ERROR"),
        6 => Json(json!({"success":true,"data":{"status":"MATCHED",
            "gitCoverage":{"state":"AVAILABLE","reason":null,"workingState":"STALE"},
            "evaluatedAt":"2026-11-09T02:00:00Z","candidates":[candidate(12)],"insufficientReason":null}}))
        .into_response(),
        _ => Json(json!({"success":true,"data":{"status":"MULTIPLE_CANDIDATES",
            "gitCoverage":{"state":"AVAILABLE","reason":null,"workingState":"STALE"},
            "evaluatedAt":"2026-11-09T02:00:00Z",
            "candidates":[candidate(i64::MAX as u64),candidate(13)],"insufficientReason":null}}))
        .into_response(),
    }
}

struct Broker {
    url: String,
    client: reqwest::Client,
    mock: Mock,
    cancel: CancellationToken,
    servers: Vec<JoinHandle<()>>,
}

impl Broker {
    async fn start(path: &std::path::Path) -> Self {
        let mock = Mock::default();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let central_address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/v1/agents/me", get(me))
            .route(
                "/api/v1/teams/{team}/agents/recommendations",
                post(recommend),
            )
            .with_state(mock.clone());
        let central_server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        devlink_broker::config::init(path).unwrap();
        let central = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        let handle = central.begin_session().await;
        handle
            .set_identity(CentralIdentity {
                connection_id: "verified-test-connection".into(),
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
        let client = TaskClient::with_timeout(
            &format!("ws://{central_address}"),
            AGENT,
            Duration::from_secs(2),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        let context = devlink_broker::context_api::ContextReader::new(
            central.clone(),
            sessions.clone(),
            path.to_path_buf(),
        )
        .unwrap();
        let app = session_api::router(Arc::new(LOCAL.into()), sessions.clone()).merge(
            devlink_broker::agent_api::router(
                Arc::new(LOCAL.into()),
                sessions.clone(),
                client,
                cancel.clone(),
                context,
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shutdown = cancel.clone();
        let broker_server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        Self {
            url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            mock,
            cancel,
            servers: vec![central_server, broker_server],
        }
    }

    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let response = self
            .client
            .post(format!("{}{path}", self.url))
            .bearer_auth(LOCAL)
            .json(body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let result: Value = response.json().await.unwrap();
        let text = result.to_string();
        assert!(!text.contains(AGENT) && !text.contains(LOCAL) && !text.contains(SECRET));
        (status, result)
    }

    async fn session(&self, bound: bool) -> Value {
        let mut body = json!({"requestId":"claim","instanceId":"host","mode":"fresh"});
        if bound {
            body["agentId"] = json!("12");
        }
        let (status, response) = self.post("/v1/sessions", &body).await;
        assert_eq!(status, 200, "{response}");
        response["data"].clone()
    }

    async fn find(&self, session: &Value, arguments: Value) -> (u16, Value) {
        let mut context = session["principal"].clone();
        context["sessionId"] = session["sessionId"].clone();
        let body = json!({"requestId":uuid::Uuid::new_v4().to_string(),"mcpRequestId":7,
            "operation":"find_agent","arguments":arguments,"context":context});
        self.post("/v1/agents/find", &body).await
    }

    async fn calls(&self) -> usize {
        self.mock.bodies.lock().await.len()
    }

    async fn stop(self) {
        self.cancel.cancel();
        for server in self.servers {
            server.abort();
            let _ = timeout(Duration::from_secs(3), server).await;
        }
    }
}

#[tokio::test]
async fn find_agent_relays_the_verified_team_recommendation_with_string_ids() {
    let dir = tempfile::tempdir().unwrap();
    let broker = Broker::start(&dir.path().join("config.json")).await;
    let unbound = broker.session(false).await;
    let (_, result) = broker
        .find(&unbound, json!({"question":"로그인 오류"}))
        .await;
    assert_eq!(result["error"]["code"], "AGENT_UNREGISTERED");
    assert_eq!(broker.calls().await, 0);

    let (status, bound) = broker
        .post(
            &format!(
                "/v1/sessions/{}/bind",
                unbound["sessionId"].as_str().unwrap()
            ),
            &json!({"requestId":"bind","instanceId":"host","agentId":"12"}),
        )
        .await;
    assert_eq!(status, 200, "{bound}");
    let session = bound["data"].clone();
    let key = "ab".repeat(32);
    let (status, result) = broker
        .find(
            &session,
            json!({"question":"  로그인 오류 고쳐줄 사람\n","repoKey":key}),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    let data = &result["data"];
    assert_eq!(data["status"], "MULTIPLE_CANDIDATES");
    assert_eq!(data["candidates"][0]["agentId"], "9223372036854775807");
    assert_eq!(data["candidates"][1]["relatedFiles"][0]["repoId"], "4");
    assert_eq!(data["candidates"][1]["connectionStatus"], "OFFLINE");
    assert!(data["candidates"][0].get("credential").is_none());
    assert_eq!(
        broker.mock.bodies.lock().await.as_slice(),
        &[(
            1,
            json!({"question":"로그인 오류 고쳐줄 사람","repoKey":key})
        )]
    );
    broker.stop().await;
}

#[tokio::test]
async fn find_agent_rejects_invalid_input_and_spoofed_context_before_central() {
    let dir = tempfile::tempdir().unwrap();
    let broker = Broker::start(&dir.path().join("config.json")).await;
    let session = broker.session(true).await;
    for arguments in [
        json!({}),
        json!({"question":" \t "}),
        json!({"question":"가".repeat(1_001)}),
        json!({"question":"q","repoKey":"AB".repeat(32)}),
        json!({"question":"q","repoKey":null}),
        json!({"question":"q","teamId":"1"}),
        json!({"question":1}),
    ] {
        let (status, result) = broker.find(&session, arguments.clone()).await;
        assert_eq!(status, 400, "{arguments}: {result}");
    }
    let mut spoofed = session.clone();
    spoofed["principal"]["teamId"] = json!("999");
    assert_eq!(broker.find(&spoofed, json!({"question":"q"})).await.0, 403);
    assert_eq!(broker.calls().await, 0);
    broker.stop().await;
}

#[tokio::test]
async fn find_agent_maps_central_failures_without_claiming_success() {
    let dir = tempfile::tempdir().unwrap();
    let broker = Broker::start(&dir.path().join("config.json")).await;
    let session = broker.session(true).await;
    for (mode, status, code, outcome) in [
        (1, 400, "VALIDATION_FAILED", "rejected"),
        (2, 403, "FORBIDDEN", "rejected"),
        (3, 503, "CENTRAL_UNAUTHENTICATED", "rejected"),
        (4, 503, "GIT_ROUTING_UNAVAILABLE", "rejected"),
        (5, 502, "CENTRAL_UNAVAILABLE", "unknown"),
        (6, 502, "CENTRAL_PROTOCOL_ERROR", "unknown"),
    ] {
        broker.mock.mode.store(mode, Ordering::SeqCst);
        let (actual, result) = broker.find(&session, json!({"question":"로그인"})).await;
        assert_eq!(result["error"]["code"], code, "{mode}: {result}");
        assert_eq!(result["error"]["outcome"], outcome, "{mode}: {result}");
        assert_eq!(actual, status, "{mode}: {result}");
    }
    // A denial does not end the local session.
    broker.mock.mode.store(0, Ordering::SeqCst);
    assert_eq!(
        broker.find(&session, json!({"question":"로그인"})).await.0,
        200
    );
    broker.stop().await;
}
