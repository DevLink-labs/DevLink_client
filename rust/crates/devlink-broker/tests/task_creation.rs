use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use devlink_broker::{
    central::{CentralIdentity, CentralState, ConnectionStatus},
    session_api,
    sessions::{ReadyIdentityProvider, SessionManager},
    task_api,
    task_client::TaskClient,
    task_requests::TaskRequests,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

const LOCAL: &str = "synthetic-local-mcp-token-1234";
const AGENT: &str = "synthetic-central-agent-token-1234";
const SECRET: &str = "untrusted-central-message-do-not-expose";

#[derive(Clone)]
struct MockState {
    mode: Arc<AtomicU8>,
    agent_mode: Arc<AtomicU8>,
    agent_calls: Arc<AtomicUsize>,
    context_mode: Arc<AtomicU8>,
    context_calls: Arc<AtomicUsize>,
    calls: Arc<Mutex<Vec<Value>>>,
    saved: Arc<Mutex<HashMap<String, u64>>>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
    redirect: String,
    log: std::path::PathBuf,
}

async fn central_create(
    State(state): State<MockState>,
    Path(team_id): Path<u64>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(team_id, 1, "creation must use the verified actor's team");
    assert_eq!(
        headers.get("authorization").unwrap(),
        &format!("Bearer {AGENT}")
    );
    assert!(!headers.contains_key("cookie") && !headers.contains_key("origin"));
    let disk: Value = serde_json::from_slice(&std::fs::read(&state.log).unwrap()).unwrap();
    assert!(
        disk["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["command"]["requestKey"] == body["requestKey"]),
        "key was not persisted before HTTP"
    );
    assert!(body.get("requesterAgentId").is_none());
    assert!(body.get("teamId").is_none());
    state.calls.lock().await.push(body.clone());
    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_active.fetch_max(active, Ordering::SeqCst);
    let mode = state.mode.load(Ordering::SeqCst);
    let key = body["requestKey"].as_str().unwrap().to_string();
    let mut saved = state.saved.lock().await;
    let replayed = saved.contains_key(&key);
    let next = saved.len() as u64 + 101;
    let task_id = *saved.entry(key).or_insert(next);
    drop(saved);
    if mode == 1 {
        sleep(Duration::from_millis(500)).await;
    }
    state.active.fetch_sub(1, Ordering::SeqCst);
    match mode {
        13 => (StatusCode::CONFLICT, Json(json!({"success":false,"code":"DUPLICATE_REQUEST","message":SECRET}))).into_response(),
        16 => (StatusCode::CONFLICT, Json(json!({"success":false,"code":"REQUEST_KEY_CONFLICT","message":SECRET}))).into_response(),
        14 => (StatusCode::OK, Json(json!({"success":false,"code":"RECEIVER_OFFLINE","message":SECRET}))).into_response(),
        15 => {
            let stream = futures_util::stream::unfold(0, |part| async move {
                match part {
                    0 => Some((Ok::<_, std::io::Error>("{\"success\":"), 1)),
                    1 => { sleep(Duration::from_millis(500)).await; Some((Ok("true,\"data\":null}"), 2)) },
                    _ => None,
                }
            });
            Response::builder().status(201).header("content-type", "application/json")
                .body(axum::body::Body::from_stream(stream)).unwrap()
        },
        11 => (StatusCode::CREATED, Json(json!({"success":true,"data":{
            "taskId":task_id,"receiverAgentId":body["receiverAgentId"]},"code":null,"message":null}))).into_response(),
        9 => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success":false,"data":{
            "taskId":task_id,"receiverAgentId":body["receiverAgentId"]},"code":"INTERNAL_SERVER_ERROR","message":SECRET}))).into_response(),
        2 => (StatusCode::UNAUTHORIZED, SECRET).into_response(),
        3 => (StatusCode::FORBIDDEN, SECRET).into_response(),
        4 => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"success":false,"code":"INTERNAL_SERVER_ERROR","message":SECRET}))).into_response(),
        5 => (StatusCode::FOUND, [("location", state.redirect)], SECRET).into_response(),
        6 => (StatusCode::CREATED, SECRET).into_response(),
        7 => (StatusCode::CREATED, "x".repeat(256 * 1024 + 1)).into_response(),
        _ => (if replayed {StatusCode::OK} else {StatusCode::CREATED}, Json(json!({
            "success":true,"data":{"taskId":task_id,"receiverAgentId":body["receiverAgentId"],
            "status":if mode == 12 {"FUTURE_STATUS"} else if mode == 17 {"REJECTED"} else {"PENDING"},"version":if mode == 18 {9_007_199_254_740_993_u64} else {1},"replayed":replayed,
            "delivery": if mode == 10 {json!({"status":"ACK_RECEIVED","reasonCode":SECRET})}
                else if mode == 8 {json!({"status":"DELIVERY_UNKNOWN","reasonCode":"ACK_TIMEOUT"})} else {json!({"status":"ACK_PENDING","reasonCode":null})}},
            "code":null,"message":null
        }))).into_response(),
    }
}

fn public_agent(id: u64) -> Value {
    json!({"agentId":id,"teamId":1,"ownerUserId":3,"name":"Existing Agent",
        "roleDescription":"Developer","toolType":"CODEX","projectName":"DevLink",
        "status":"ONLINE","lastSeenAt":"2026-10-30T01:00:00","createdAt":"2026-10-29T00:00:00",
        "unexpectedCredential":SECRET})
}
async fn central_agent(State(state): State<MockState>, headers: HeaderMap) -> Response {
    assert_eq!(headers["authorization"], format!("Bearer {AGENT}"));
    assert!(!headers.contains_key("cookie"));
    state.agent_calls.fetch_add(1, Ordering::SeqCst);
    let mode = state.agent_mode.load(Ordering::SeqCst);
    if mode == 1 {
        sleep(Duration::from_millis(500)).await;
    }
    if mode == 2 {
        return (StatusCode::UNAUTHORIZED, SECRET).into_response();
    }
    if mode == 3 {
        return (StatusCode::FORBIDDEN, SECRET).into_response();
    }
    if mode == 6 {
        return (StatusCode::FOUND, [("location", state.redirect)], SECRET).into_response();
    }
    let mut agent = public_agent(12);
    if mode == 4 {
        agent["ownerUserId"] = json!(999);
    }
    if mode == 5 {
        agent["agentId"] = json!(99);
    }
    if mode == 9 {
        agent["agentId"] = json!(99);
        agent["name"] = json!("Other Agent");
    }
    if mode == 10 {
        agent["teamId"] = json!(999);
    }
    Json(json!({"success":true,"data":agent})).into_response()
}
async fn central_agents(
    Path(team_id): Path<u64>,
    State(state): State<MockState>,
    headers: HeaderMap,
) -> Response {
    assert_eq!(team_id, 1, "Agent list must use the verified team");
    assert_eq!(headers["authorization"], format!("Bearer {AGENT}"));
    state.agent_calls.fetch_add(1, Ordering::SeqCst);
    let mut agent = public_agent(i64::MAX as u64);
    match state.agent_mode.load(Ordering::SeqCst) {
        7 => agent["teamId"] = json!(999),
        8 => agent["agentId"] = json!(9223372036854775808_u64),
        _ => {}
    }
    Json(json!({"success":true,"data":[public_agent(12),agent]})).into_response()
}

async fn central_context(
    Path(team_id): Path<u64>,
    Query(query): Query<HashMap<String, String>>,
    State(state): State<MockState>,
    headers: HeaderMap,
) -> Response {
    assert_eq!(
        team_id, 1,
        "team context must use the verified actor's team"
    );
    assert_eq!(headers["authorization"], format!("Bearer {AGENT}"));
    assert!(!headers.contains_key("cookie"));
    state.context_calls.fetch_add(1, Ordering::SeqCst);
    match state.context_mode.load(Ordering::SeqCst) {
        1 => (StatusCode::UNAUTHORIZED, SECRET).into_response(),
        2 => (StatusCode::FOUND, [("location", state.redirect)], SECRET).into_response(),
        _ => Json(
            json!({"success":true,"data":{"teamId":team_id,"query":query,"version":3,
            "common":[{"statement":"작업 상세는 /api/v1/tasks/{id}","evidence":["101:2"]}],
            "changed":[],"issues":[{"topic":"404 vs empty 200","evidence":["102:5"]}]}}),
        )
        .into_response(),
    }
}

struct Central {
    state: MockState,
    address: std::net::SocketAddr,
    server: JoinHandle<()>,
    trap: JoinHandle<()>,
    trapped: Arc<AtomicUsize>,
}

impl Central {
    async fn start(config_path: &std::path::Path) -> Self {
        let trapped = Arc::new(AtomicUsize::new(0));
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let redirect = format!("http://{}/leak", target.local_addr().unwrap());
        let count = trapped.clone();
        let trap = tokio::spawn(async move {
            loop {
                if target.accept().await.is_ok() {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let state = MockState {
            mode: Arc::new(AtomicU8::new(0)),
            agent_mode: Arc::default(),
            agent_calls: Arc::default(),
            context_mode: Arc::default(),
            context_calls: Arc::default(),
            calls: Arc::default(),
            saved: Arc::default(),
            active: Arc::default(),
            max_active: Arc::default(),
            redirect,
            log: TaskRequests::path(config_path),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/v1/tasks/{team_id}", post(central_create))
            .route("/api/v1/agents/me", get(central_agent))
            .route("/api/v1/agents/teams/{team_id}", get(central_agents))
            .route("/api/v1/teams/{team_id}/context", get(central_context))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            state,
            address,
            server,
            trap,
            trapped,
        }
    }
    async fn wait_calls(&self, count: usize) {
        timeout(Duration::from_secs(5), async {
            loop {
                if self.state.calls.lock().await.len() >= count {
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}
impl Drop for Central {
    fn drop(&mut self) {
        self.server.abort();
        self.trap.abort();
    }
}

struct Broker {
    url: String,
    client: reqwest::Client,
    central: CentralState,
    cancel: CancellationToken,
    server: JoinHandle<()>,
    expiration: JoinHandle<()>,
}
impl Broker {
    async fn start(path: &std::path::Path, remote: &Central, request_timeout: Duration) -> Self {
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
        let requests = TaskRequests::open(path).unwrap();
        let task_client =
            TaskClient::with_timeout(&format!("ws://{}", remote.address), AGENT, request_timeout)
                .unwrap();
        let cancel = CancellationToken::new();
        let context = devlink_broker::context_api::ContextReader::new(
            central.clone(),
            sessions.clone(),
            path.to_path_buf(),
        )
        .unwrap();
        let app = session_api::router(Arc::new(LOCAL.into()), sessions.clone())
            .merge(task_api::router(
                Arc::new(LOCAL.into()),
                sessions.clone(),
                task_client.clone(),
                requests,
                cancel.clone(),
                Some(
                    devlink_broker::sent_tasks::Ledger::new(
                        path,
                        &format!("ws://{}", remote.address),
                    )
                    .unwrap(),
                ),
            ))
            .merge(devlink_broker::team_context_routes::router(
                Arc::new(LOCAL.into()),
                sessions.clone(),
                task_client.clone(),
                cancel.clone(),
            ))
            .merge(devlink_broker::agent_api::router(
                Arc::new(LOCAL.into()),
                sessions.clone(),
                task_client,
                cancel.clone(),
                context,
            ));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shutdown = cancel.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
                .unwrap();
        });
        let expiration = tokio::spawn(sessions.run_expiration(cancel.clone()));
        Self {
            url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            central,
            cancel,
            server,
            expiration,
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
        assert!(
            !result.to_string().contains(AGENT)
                && !result.to_string().contains(LOCAL)
                && !result.to_string().contains(SECRET)
        );
        (status, result)
    }
    async fn claim(&self, instance: &str, bound: bool) -> Value {
        let mut body =
            json!({"requestId":format!("claim-{instance}"),"instanceId":instance,"mode":"fresh"});
        if bound {
            body["agentId"] = json!("12");
        }
        let (status, response) = self.post("/v1/sessions", &body).await;
        assert_eq!(status, 200);
        response["data"].clone()
    }
    async fn stop(self) {
        self.cancel.cancel();
        timeout(Duration::from_secs(3), self.server)
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(3), self.expiration)
            .await
            .unwrap()
            .unwrap();
    }
}

fn request(session: &Value, id: &str) -> Value {
    let mut context = session["principal"].clone();
    context["sessionId"] = session["sessionId"].clone();
    json!({"requestId":id,"mcpRequestId":27,"operation":"create_task",
        "arguments":{"recipientAgentId":"13","title":"로그인 수정","description":"오류 메시지 수정","taskContext":"작업 맥락"},"context":context})
}

fn agent_request(session: &Value, operation: &str, args: Value) -> Value {
    let mut body = request(session, &uuid::Uuid::new_v4().to_string());
    body["operation"] = json!(operation);
    body["arguments"] = args;
    body
}

#[tokio::test]
async fn changed_central_team_blocks_creation_before_any_task_post() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    remote.state.agent_mode.store(10, Ordering::SeqCst);

    let (status, result) = broker
        .post(
            "/v1/tasks/create",
            &request(&session, "changed-central-team"),
        )
        .await;
    assert_eq!(status, 403, "{result}");
    assert_eq!(result["error"]["code"], "FORBIDDEN");
    assert_eq!(remote.state.agent_calls.load(Ordering::SeqCst), 1);
    assert!(remote.state.calls.lock().await.is_empty());
    assert!(remote.state.saved.lock().await.is_empty());
    broker.stop().await;
}

#[tokio::test]
async fn creation_preserves_versions_above_javascript_safe_integer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    remote.state.mode.store(18, Ordering::SeqCst);
    let (status, response) = broker
        .post("/v1/tasks/create", &request(&session, "long-version"))
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["version"], "9007199254740993");
    broker.stop().await;
}

#[tokio::test]
async fn verified_team_creation_replays_a_rejected_task_without_creating_another() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    let body = request(&session, "rejected-replay");
    let (status, created) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["data"]["status"], "PENDING");

    remote.state.mode.store(17, Ordering::SeqCst);
    let (status, replayed) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(replayed["data"]["status"], "REJECTED");
    assert_eq!(replayed["data"]["replayed"], true);
    assert_eq!(replayed["data"]["taskId"], created["data"]["taskId"]);
    assert_eq!(replayed["requestKey"], created["requestKey"]);
    assert_eq!(remote.state.saved.lock().await.len(), 1);
    let calls = remote.state.calls.lock().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0]["requestKey"], calls[1]["requestKey"]);
    drop(calls);
    broker.stop().await;
    // The ledger write is detached from the response; wait for it, then confirm the
    // replayed create did not record the task twice for the overdue advisory.
    let mut recorded = 0;
    for _ in 0..100 {
        recorded += devlink_broker::sent_tasks::scan(&path, 1, u64::MAX).unwrap();
        if recorded > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    recorded += devlink_broker::sent_tasks::scan(&path, 1, u64::MAX).unwrap();
    assert_eq!(recorded, 1);
}

#[tokio::test]
async fn register_confirms_existing_agent_and_list_preserves_long_ids_without_creation() {
    let dir = tempfile::tempdir().unwrap();
    let remote = Central::start(&dir.path().join("config.json")).await;
    let broker = Broker::start(
        &dir.path().join("config.json"),
        &remote,
        Duration::from_secs(1),
    )
    .await;
    let session = broker.claim("host", false).await;
    let body = agent_request(
        &session,
        "register_agent",
        json!({"name":"Existing Agent","toolType":"codex","role":"Developer"}),
    );
    for _ in 0..2 {
        let (status, result) = broker.post("/v1/agents/register", &body).await;
        assert_eq!(status, 200, "{result}");
        assert_eq!(result["data"]["agentId"], "12");
        assert_eq!(result["data"]["ownerUserId"], "3");
        assert!(result["data"].get("unexpectedCredential").is_none());
    }
    let list = agent_request(&session, "list_agents", json!({}));
    assert_eq!(
        broker.post("/v1/agents/list", &list).await.1["error"]["code"],
        "AGENT_UNREGISTERED"
    );
    let (_, bound) = broker
        .post(
            &format!(
                "/v1/sessions/{}/bind",
                session["sessionId"].as_str().unwrap()
            ),
            &json!({"requestId":"bind","instanceId":"host","agentId":"12"}),
        )
        .await;
    let list = agent_request(&bound["data"], "list_agents", json!({}));
    let (status, result) = broker.post("/v1/agents/list", &list).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(
        result["data"]["agents"][1]["agentId"],
        "9223372036854775807"
    );
    assert_eq!(result["data"]["agents"][0]["toolType"], "CODEX");
    assert!(remote.state.calls.lock().await.is_empty());
    assert!(!TaskRequests::path(&dir.path().join("config.json")).exists());
    broker.stop().await;
}

#[tokio::test]
async fn agent_providers_reject_spoofing_metadata_mismatch_and_central_denials() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_millis(100)).await;
    let session = broker.claim("host", true).await;
    let before = remote.state.agent_calls.load(Ordering::SeqCst);
    for field in ["userId", "teamId", "agentId"] {
        let mut body = agent_request(&session, "register_agent", json!({}));
        body["context"][field] = json!("999");
        assert_eq!(broker.post("/v1/agents/register", &body).await.0, 403);
    }
    assert_eq!(before, remote.state.agent_calls.load(Ordering::SeqCst));
    for args in [
        json!({"name":"new name"}),
        json!({"role":"new role"}),
        json!({"toolType":"claude_code"}),
        json!({"teamId":"999"}),
    ] {
        assert_eq!(
            broker
                .post(
                    "/v1/agents/register",
                    &agent_request(&session, "register_agent", args)
                )
                .await
                .0,
            400
        );
    }
    for (mode, code) in [
        (1, "CENTRAL_TIMEOUT"),
        (2, "CENTRAL_UNAUTHENTICATED"),
        (3, "FORBIDDEN"),
        (4, "FORBIDDEN"),
        (5, "FORBIDDEN"),
        (6, "CENTRAL_PROTOCOL_ERROR"),
    ] {
        remote.state.agent_mode.store(mode, Ordering::SeqCst);
        let (status, result) = broker
            .post(
                "/v1/agents/register",
                &agent_request(&session, "register_agent", json!({})),
            )
            .await;
        assert_ne!(status, 401);
        assert_eq!(result["error"]["code"], code, "{mode}: {result}");
    }
    for mode in [7, 8] {
        remote.state.agent_mode.store(mode, Ordering::SeqCst);
        let (_, result) = broker
            .post(
                "/v1/agents/list",
                &agent_request(&session, "list_agents", json!({})),
            )
            .await;
        assert_eq!(result["error"]["code"], "CENTRAL_PROTOCOL_ERROR");
    }
    assert_eq!(remote.trapped.load(Ordering::SeqCst), 0);
    assert!(remote.state.calls.lock().await.is_empty());
    broker.stop().await;
}

#[tokio::test]
async fn canonical_prompt_is_opaque_actor_fields_are_forbidden_and_conflicts_keep_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    let prompt = "  Exact prompt\n한글\n\"quoted\"  ";
    let mut body = request(&session, "canonical");
    body["arguments"] = json!({"receiverAgentId":"9223372036854775807","prompt":prompt});
    let (status, result) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["data"]["receiverAgentId"], "9223372036854775807");
    assert_eq!(remote.state.calls.lock().await[0]["prompt"], prompt);
    body["arguments"]["prompt"] = json!("changed");
    let (status, conflict) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(status, 409);
    assert_eq!(conflict["requestKey"], result["requestKey"]);
    for id in ["0", "01", "+1", " 1", "9223372036854775808"] {
        body["arguments"]["receiverAgentId"] = json!(id);
        assert_eq!(broker.post("/v1/tasks/create", &body).await.0, 400);
    }
    body["arguments"] = json!({"receiverAgentId":"13","prompt":"prompt","requesterAgentId":"999"});
    assert_eq!(broker.post("/v1/tasks/create", &body).await.0, 400);
    assert_eq!(remote.state.calls.lock().await.len(), 1);
    broker.stop().await;
}

#[tokio::test]
async fn concurrent_different_payloads_preserve_the_winning_request_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    for index in 0..4 {
        let first = request(&session, &format!("same-id-{index}"));
        let mut second = first.clone();
        second["arguments"]["description"] = json!("different concurrent input");
        let (left, right) = tokio::join!(
            broker.post("/v1/tasks/create", &first),
            broker.post("/v1/tasks/create", &second),
        );
        let (success, conflict) = if left.0 == 200 {
            (left, right)
        } else {
            (right, left)
        };
        assert_eq!(success.0, 200, "{:?}", success.1);
        assert_eq!(conflict.0, 409, "{:?}", conflict.1);
        assert_eq!(conflict.1["error"]["code"], "REQUEST_KEY_CONFLICT");
        assert!(success.1["requestKey"].is_string());
        assert_eq!(conflict.1["requestKey"], success.1["requestKey"]);
    }
    assert_eq!(remote.state.calls.lock().await.len(), 4);
    assert_eq!(remote.state.saved.lock().await.len(), 4);
    broker.stop().await;
}

#[tokio::test]
async fn registration_rejects_invalid_tool_before_central_io() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", false).await;
    for tool in ["invalid", "CODEX", "", "claude-code"] {
        let (_, response) = broker
            .post(
                "/v1/agents/register",
                &agent_request(&session, "register_agent", json!({"toolType":tool})),
            )
            .await;
        assert_eq!(response["error"]["code"], "VALIDATION_FAILED");
    }
    assert_eq!(remote.state.agent_calls.load(Ordering::SeqCst), 0);
    broker.stop().await;
}

#[tokio::test]
async fn registration_fences_changed_profile_before_confirmation_and_preserves_pending() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", false).await;
    let original = devlink_broker::config::Config::load(&path)
        .unwrap()
        .server_url;
    devlink_broker::config::set(&path, "serverUrl", "wss://other.example").unwrap();
    let (status, result) = broker
        .post(
            "/v1/agents/register",
            &agent_request(&session, "register_agent", json!({})),
        )
        .await;
    assert_eq!(status, 403, "{result}");
    assert_eq!(result["error"]["code"], "FORBIDDEN");
    assert_eq!(remote.state.agent_calls.load(Ordering::SeqCst), 0);
    devlink_broker::config::set(&path, "serverUrl", &original).unwrap();
    let pending = json!({"schemaVersion":1,"origin":"https://api.devlink.ai.kr","userId":3,"teamId":1,
        "projectRoot":dir.path().canonicalize().unwrap().to_str().unwrap(),
        "state":{"kind":"create_pending","name":"pending","tool":"CODEX","project_name":"project"}});
    let profile = devlink_broker::agent_profile::path(&path);
    devlink_broker::private_file::write(&profile, &pending, 16384).unwrap();
    let bytes = std::fs::read(&profile).unwrap();
    let (status, result) = broker
        .post(
            "/v1/agents/register",
            &agent_request(&session, "register_agent", json!({})),
        )
        .await;
    assert_eq!(status, 503, "{result}");
    assert_eq!(result["error"]["code"], "BROKER_CONTEXT_UNAVAILABLE");
    assert_eq!(std::fs::read(profile).unwrap(), bytes);
    assert_eq!(remote.state.agent_calls.load(Ordering::SeqCst), 0);
    broker.stop().await;
}

#[tokio::test]
async fn unbound_registration_prioritizes_ready_identity_over_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", false).await;
    remote.state.agent_mode.store(9, Ordering::SeqCst);
    let (status, result) = broker
        .post(
            "/v1/agents/register",
            &agent_request(
                &session,
                "register_agent",
                json!({"name":"Existing Agent","toolType":"codex"}),
            ),
        )
        .await;
    assert_eq!(status, 403, "{result}");
    assert_eq!(result["error"]["code"], "FORBIDDEN");
    assert_eq!(
        broker.central.presence().await,
        Some(devlink_broker::protocol::PresenceStatus::Offline)
    );
    broker.stop().await;
}

#[tokio::test]
async fn concurrent_insert_conflict_is_unknown_and_never_requeried_or_rekeyed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_millis(100)).await;
    let session = broker.claim("host", true).await;
    for (mode, code) in [
        (13, "CENTRAL_RESULT_UNKNOWN"),
        (14, "CENTRAL_PROTOCOL_ERROR"),
        (15, "CENTRAL_TIMEOUT"),
        (16, "REQUEST_KEY_CONFLICT"),
    ] {
        remote.state.mode.store(mode, Ordering::SeqCst);
        let body = request(&session, &format!("collision-{mode}"));
        let (_, result) = broker.post("/v1/tasks/create", &body).await;
        assert_eq!(result["error"]["code"], code, "{result}");
        assert_eq!(result["error"]["outcome"], "unknown");
        assert!(result["requestKey"].is_string());
        let calls = remote.state.calls.lock().await;
        assert_eq!(calls.len(), usize::from(mode - 12));
        assert_eq!(calls.last().unwrap()["requestKey"], result["requestKey"]);
    }
    // The redirect trap would catch any unsupported recovery fetch.
    assert_eq!(remote.trapped.load(Ordering::SeqCst), 0);
    broker.stop().await;
}

#[tokio::test]
async fn pending_agent_confirmation_cannot_revive_a_released_session() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", false).await;
    remote.state.agent_mode.store(1, Ordering::SeqCst);
    let body = agent_request(
        &session,
        "register_agent",
        json!({"name":"metadata mismatch","toolType":"codex"}),
    );
    let confirm = broker.post("/v1/agents/register", &body);
    let release = async {
        timeout(Duration::from_secs(2), async {
            while remote.state.agent_calls.load(Ordering::SeqCst) == 0 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        broker
            .client
            .delete(format!(
                "{}/v1/sessions/{}",
                broker.url,
                session["sessionId"].as_str().unwrap()
            ))
            .bearer_auth(LOCAL)
            .json(&json!({"requestId":"release","instanceId":"host"}))
            .send()
            .await
            .unwrap();
    };
    let ((status, result), _) = tokio::join!(confirm, release);
    assert_eq!(status, 409);
    assert_eq!(result["error"]["code"], "SESSION_EXPIRED");
    assert_eq!(
        broker.central.presence().await,
        Some(devlink_broker::protocol::PresenceStatus::Offline)
    );
    broker.stop().await;
}

#[tokio::test]
async fn pending_registration_prioritizes_replacement_over_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    remote.state.agent_mode.store(1, Ordering::SeqCst);
    let body = agent_request(
        &session,
        "register_agent",
        json!({"name":"metadata mismatch","toolType":"codex"}),
    );
    let confirm = broker.post("/v1/agents/register", &body);
    let replace = async {
        timeout(Duration::from_secs(2), async {
            while remote.state.agent_calls.load(Ordering::SeqCst) == 0 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        broker.claim("replacement", true).await;
    };
    let ((status, result), _) = tokio::join!(confirm, replace);
    assert_eq!(status, 409, "{result}");
    assert_eq!(result["error"]["code"], "SESSION_REPLACED");
    assert_eq!(
        broker.central.presence().await,
        Some(devlink_broker::protocol::PresenceStatus::Online)
    );
    broker.stop().await;
}

#[tokio::test]
async fn known_task_identity_survives_response_validation_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    for mode in [10, 11, 12] {
        remote.state.mode.store(mode, Ordering::SeqCst);
        let (status, result) = broker
            .post(
                "/v1/tasks/create",
                &request(&session, &format!("invalid-response-{mode}")),
            )
            .await;
        assert_eq!(status, 502);
        assert_eq!(result["error"]["code"], "CENTRAL_PROTOCOL_ERROR");
        assert_eq!(result["error"]["outcome"], "unknown");
        assert!(
            result["error"]["taskId"]
                .as_str()
                .is_some_and(|id| id.parse::<u64>().is_ok()),
            "known task ID was lost"
        );
        assert!(result["requestKey"].is_string());
    }
    broker.stop().await;
}

#[tokio::test]
async fn oversized_prompt_is_rejected_before_persistence_or_central_creation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    for (index, description, context) in [
        (1, "한".repeat(16_000), "글".repeat(16_000)),
        (2, "\u{0001}".repeat(16_000), "context".into()),
    ] {
        let mut body = request(&session, &format!("oversized-{index}"));
        body["arguments"]["description"] = json!(description);
        body["arguments"]["taskContext"] = json!(context);
        assert!(serde_json::to_vec(&body).unwrap().len() < 128 * 1024);
        let prompt = json!({"title":body["arguments"]["title"],"description":body["arguments"]["description"],
            "taskContext":body["arguments"]["taskContext"]}).to_string();
        let wire = json!({"type":"TASK_CREATED","event_id":"synthetic-event","payload":{
            "task_id":101,"team_id":1,"requester_agent_id":12,"receiver_agent_id":13,
            "prompt":prompt,"created_at":"2026-10-29T09:00:00"}});
        assert!(
            serde_json::to_vec(&wire).unwrap().len()
                > devlink_broker::protocol::MAX_CENTRAL_MESSAGE_BYTES
        );
        let (status, result) = broker.post("/v1/tasks/create", &body).await;
        assert_eq!(status, 400);
        assert_eq!(result["error"]["code"], "VALIDATION_FAILED");
    }
    assert!(remote.state.calls.lock().await.is_empty());
    assert!(!TaskRequests::path(&path).exists());
    let mut valid = request(&session, "escaped-but-deliverable");
    valid["arguments"]["description"] = json!("\"".repeat(16_000));
    valid["arguments"]["taskContext"] = json!("context");
    assert_eq!(broker.post("/v1/tasks/create", &valid).await.0, 200);
    broker.stop().await;
}

#[tokio::test]
async fn authenticated_creation_maps_actor_payload_ids_and_delivery_without_waiting_for_ack() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    let (status, result) = broker
        .post("/v1/tasks/create", &request(&session, "create-1"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(result["data"]["taskId"], "101");
    assert_eq!(result["data"]["receiverAgentId"], "13");
    assert_eq!(result["data"]["delivery"]["status"], "ACK_PENDING");
    assert_eq!(result["requestKey"], result["data"]["requestKey"]);
    let sent = remote.state.calls.lock().await[0].clone();
    assert!(sent.get("requesterAgentId").is_none());
    assert_eq!(sent["receiverAgentId"], 13);
    assert_ne!(sent["requestKey"], "create-1");
    let prompt: Value = serde_json::from_str(sent["prompt"].as_str().unwrap()).unwrap();
    assert_eq!(
        prompt,
        json!({"title":"로그인 수정","description":"오류 메시지 수정","taskContext":"작업 맥락"})
    );
    remote.state.mode.store(8, Ordering::SeqCst);
    let (status, unknown) = broker
        .post("/v1/tasks/create", &request(&session, "create-2"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(unknown["data"]["status"], "PENDING");
    assert_eq!(unknown["data"]["delivery"]["status"], "DELIVERY_UNKNOWN");
    remote.state.mode.store(17, Ordering::SeqCst);
    let (status, rejected) = broker
        .post("/v1/tasks/create", &request(&session, "create-rejected"))
        .await;
    assert_eq!(status, 200);
    assert_eq!(rejected["data"]["status"], "REJECTED");
    broker.stop().await;
}

#[tokio::test]
async fn repeated_request_survives_broker_restart_and_changed_payload_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    let body = request(&session, "stable-request");
    let (_, first) = broker.post("/v1/tasks/create", &body).await;
    let (_, second) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(first["requestKey"], second["requestKey"]);
    assert_eq!(second["data"]["replayed"], true);
    let mut conflict = body;
    conflict["arguments"]["description"] = json!("different");
    let (status, error) = broker.post("/v1/tasks/create", &conflict).await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "REQUEST_KEY_CONFLICT");
    assert_eq!(remote.state.calls.lock().await.len(), 2);
    broker.stop().await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("restarted-host", true).await;
    let (_, third) = broker
        .post("/v1/tasks/create", &request(&session, "stable-request"))
        .await;
    assert_eq!(first["requestKey"], third["requestKey"]);
    assert_eq!(third["data"]["taskId"], "101");
    assert_eq!(remote.state.saved.lock().await.len(), 1);
    broker.stop().await;
}

#[tokio::test]
async fn spoofed_unbound_and_replaced_sessions_never_reach_central() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let unbound = broker.claim("unbound", false).await;
    let (status, error) = broker
        .post("/v1/tasks/create", &request(&unbound, "unbound"))
        .await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "AGENT_UNREGISTERED");
    let session = broker.claim("host", true).await;
    for field in ["userId", "teamId", "agentId"] {
        let mut body = request(&session, field);
        body["context"][field] = json!("999");
        assert_eq!(broker.post("/v1/tasks/create", &body).await.0, 403);
    }
    let replacement = broker.claim("replacement", true).await;
    let (_, error) = broker
        .post("/v1/tasks/create", &request(&session, "old-session"))
        .await;
    assert_eq!(error["error"]["code"], "SESSION_REPLACED");
    let mut invalid = request(&replacement, "wrong-operation");
    invalid["operation"] = json!("complete_task");
    assert_eq!(broker.post("/v1/tasks/create", &invalid).await.0, 400);
    assert!(remote.state.calls.lock().await.is_empty());
    broker.stop().await;
}

#[tokio::test]
async fn timeout_is_unknown_without_retry_and_explicit_replay_recovers_saved_task() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    remote.state.mode.store(1, Ordering::SeqCst);
    let broker = Broker::start(&path, &remote, Duration::from_millis(100)).await;
    let session = broker.claim("host", true).await;
    let body = request(&session, "timed-out");
    let (status, error) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(status, 502);
    assert_eq!(error["error"]["code"], "CENTRAL_TIMEOUT");
    assert_eq!(error["error"]["outcome"], "unknown");
    assert!(error["requestKey"].is_string());
    sleep(Duration::from_millis(150)).await;
    assert_eq!(
        remote.state.calls.lock().await.len(),
        1,
        "transport retried a mutation"
    );
    remote.state.mode.store(0, Ordering::SeqCst);
    let (_, recovered) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(recovered["requestKey"], error["requestKey"]);
    assert_eq!(recovered["data"]["replayed"], true);
    assert_eq!(broker.central.status().await, ConnectionStatus::Connected);
    broker.stop().await;
}

#[tokio::test]
async fn central_denials_protocol_errors_and_redirects_keep_local_session_alive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    for (mode, expected) in [
        (2, "CENTRAL_UNAUTHENTICATED"),
        (3, "FORBIDDEN"),
        (4, "CENTRAL_UNAVAILABLE"),
        (5, "CENTRAL_PROTOCOL_ERROR"),
        (6, "CENTRAL_PROTOCOL_ERROR"),
        (7, "CENTRAL_PROTOCOL_ERROR"),
        (9, "CENTRAL_RESULT_UNKNOWN"),
    ] {
        remote.state.mode.store(mode, Ordering::SeqCst);
        let (_, error) = broker
            .post(
                "/v1/tasks/create",
                &request(&session, &format!("error-{mode}")),
            )
            .await;
        assert_eq!(error["error"]["code"], expected);
        if mode == 9 {
            assert!(error["error"]["taskId"].is_string());
        }
        assert_ne!(error["error"]["scope"], "local_auth");
        let (status, _) = broker
            .post(
                &format!(
                    "/v1/sessions/{}/heartbeat",
                    session["sessionId"].as_str().unwrap()
                ),
                &json!({"requestId":"renew","instanceId":"host"}),
            )
            .await;
        assert_eq!(status, 200);
    }
    assert_eq!(
        remote.trapped.load(Ordering::SeqCst),
        0,
        "redirect leaked a credential"
    );
    broker.stop().await;
}

#[tokio::test]
async fn released_session_waiting_for_duplicate_gate_is_not_sent_late() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    remote.state.mode.store(1, Ordering::SeqCst);
    let broker = Broker::start(&path, &remote, Duration::from_secs(2)).await;
    let session = broker.claim("host", true).await;
    let body = request(&session, "concurrent");
    let url = format!("{}/v1/tasks/create", broker.url);
    let first = broker
        .client
        .post(&url)
        .bearer_auth(LOCAL)
        .json(&body)
        .send();
    let second = broker
        .client
        .post(&url)
        .bearer_auth(LOCAL)
        .json(&body)
        .send();
    let release = async {
        remote.wait_calls(1).await;
        broker
            .client
            .delete(format!(
                "{}/v1/sessions/{}",
                broker.url,
                session["sessionId"].as_str().unwrap()
            ))
            .bearer_auth(LOCAL)
            .json(&json!({"requestId":"release","instanceId":"host"}))
            .send()
            .await
            .unwrap();
    };
    let (a, b, _) = tokio::join!(first, second, release);
    let statuses = [a.unwrap().status().as_u16(), b.unwrap().status().as_u16()];
    assert!(statuses.contains(&200) && statuses.contains(&409));
    assert_eq!(remote.state.calls.lock().await.len(), 1);
    assert_eq!(remote.state.max_active.load(Ordering::SeqCst), 1);
    broker.stop().await;
}

#[tokio::test]
async fn bad_local_credentials_and_storage_failure_never_call_central() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    let body = request(&session, "never-send");
    let response = broker
        .client
        .post(format!("{}/v1/tasks/create", broker.url))
        .bearer_auth(AGENT)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    let response = broker
        .client
        .post(format!("{}/v1/tasks/create", broker.url))
        .bearer_auth(LOCAL)
        .header("Origin", "https://example.test")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    std::fs::create_dir(TaskRequests::path(&path)).unwrap();
    let (status, error) = broker.post("/v1/tasks/create", &body).await;
    assert_eq!(status, 503);
    assert_eq!(error["error"]["code"], "BROKER_STORAGE_FAILED");
    assert!(remote.state.calls.lock().await.is_empty());
    broker.stop().await;
}

// Opt-in: compile packages/mcp-server into a directory under rust/target and set
// DEVLINK_MCP_BUILD to that path. The harness uses its existing node_modules.
// Reuse the HTTP fixtures above; READY identity is prepared, not received over WS.
#[tokio::test]
#[ignore = "requires Node >=24, DEVLINK_MCP_BUILD and a real 90-second lease wait"]
async fn actual_mcp_session_ownership_expiry_and_identity_rejection() {
    let build = std::env::var_os("DEVLINK_MCP_BUILD").expect("set DEVLINK_MCP_BUILD");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir_in(workspace.join("target")).unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(2)).await;
    let mut command = tokio::process::Command::new("node");
    command
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/mcp_sessions.mjs"
        ))
        .arg(&build)
        .arg(&broker.url)
        .env("DEVLINK_TEST_LOCAL_TOKEN", LOCAL)
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = timeout(Duration::from_secs(120), command.output())
        .await
        .expect("MCP session harness timed out")
        .expect("cannot start Node");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["passed"], true);
    // Only the three explicit successful calls reach the fixture. Rejected
    // identity, replaced owner and expired session never create central tasks.
    assert_eq!(remote.state.calls.lock().await.len(), 3);
    assert_eq!(remote.state.saved.lock().await.len(), 3);
    assert_eq!(
        broker.central.presence().await,
        Some(devlink_broker::protocol::PresenceStatus::Offline)
    );
    broker.stop().await;
}

#[tokio::test]
#[ignore = "requires Node >=24 and current MCP build at DEVLINK_MCP_BUILD"]
async fn actual_mcp_tools_use_rust_sessions_and_mock_central() {
    let build = std::env::var_os("DEVLINK_MCP_BUILD").expect("set DEVLINK_MCP_BUILD");
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir_in(workspace.join("target")).unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_millis(200)).await;
    for (scenario, mode, expected_calls) in [
        ("success", 0, 2),
        ("delivery-unknown", 8, 1),
        ("central-auth", 2, 1),
        ("timeout", 1, 1),
        ("local-auth", 0, 0),
    ] {
        remote.state.mode.store(mode, Ordering::SeqCst);
        let before = remote.state.calls.lock().await.len();
        let mut command = tokio::process::Command::new("node");
        command
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/support/mcp_task_creation.mjs"
            ))
            .arg(&build)
            .arg(&broker.url)
            .arg(scenario)
            .env("DEVLINK_TEST_LOCAL_TOKEN", LOCAL)
            .env("DEVLINK_TEST_AGENT_TOKEN", AGENT)
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        let output = timeout(Duration::from_secs(20), command.output())
            .await
            .expect("MCP harness timed out")
            .expect("cannot start Node");
        assert!(
            output.status.success(),
            "{scenario}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["scenario"], scenario);
        assert_eq!(report["passed"], true);
        // The real MCP transport must not retry a central mutation implicitly.
        let calls = remote.state.calls.lock().await;
        assert_eq!(calls.len() - before, expected_calls, "{scenario}");
        for sent in &calls[before..] {
            assert!(sent.get("requesterAgentId").is_none());
            assert_eq!(sent["receiverAgentId"], 13);
            let prompt: Value = serde_json::from_str(sent["prompt"].as_str().unwrap()).unwrap();
            assert_eq!(prompt["title"], "MCP integration");
            assert_eq!(prompt["description"], "실제 MCP 입력");
            assert_eq!(prompt["taskContext"], "mock only");
        }
        if scenario == "success" {
            assert_ne!(calls[before]["requestKey"], calls[before + 1]["requestKey"]);
            assert_eq!(remote.state.saved.lock().await.len(), 2);
        }
        println!("actual MCP: {scenario} passed ({expected_calls} central calls)");
    }
    broker.stop().await;
}

fn keyed_request(session: &Value, id: &str, key: Value) -> Value {
    let mut body = request(session, id);
    body["arguments"]["requestKey"] = key;
    body
}

#[tokio::test]
async fn caller_request_key_replays_across_calls_and_restarts_and_rejects_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("host", true).await;
    let key = json!("agent-chosen-key");
    let (status, first) = broker
        .post(
            "/v1/tasks/create",
            &keyed_request(&session, "call-1", key.clone()),
        )
        .await;
    assert_eq!(status, 200, "{first}");
    assert_eq!(first["requestKey"], key);
    assert_eq!(remote.state.calls.lock().await[0]["requestKey"], key);
    // A separate tool call has a new requestId; the key still names the same request.
    let (status, second) = broker
        .post(
            "/v1/tasks/create",
            &keyed_request(&session, "call-2", key.clone()),
        )
        .await;
    assert_eq!(status, 200, "{second}");
    assert_eq!(second["data"]["taskId"], first["data"]["taskId"]);
    assert_eq!(second["data"]["replayed"], true);
    assert_eq!(second["requestKey"], key);
    let mut changed = keyed_request(&session, "call-3", key.clone());
    changed["arguments"]["description"] = json!("different");
    let (status, conflict) = broker.post("/v1/tasks/create", &changed).await;
    assert_eq!(status, 409, "{conflict}");
    assert_eq!(conflict["error"]["code"], "REQUEST_KEY_CONFLICT");
    assert_eq!(conflict["error"]["outcome"], "rejected");
    assert_eq!(conflict["requestKey"], key);
    assert_eq!(
        remote.state.calls.lock().await.len(),
        2,
        "conflict reached central"
    );
    // A broker-issued key can be passed back to retry the same request.
    let (_, plain) = broker
        .post("/v1/tasks/create", &request(&session, "plain"))
        .await;
    let (status, reused) = broker
        .post(
            "/v1/tasks/create",
            &keyed_request(&session, "plain-retry", plain["requestKey"].clone()),
        )
        .await;
    assert_eq!(status, 200, "{reused}");
    assert_eq!(reused["data"]["taskId"], plain["data"]["taskId"]);
    assert_eq!(reused["data"]["replayed"], true);
    let long = json!("k".repeat(65));
    for invalid in [
        json!(""),
        json!(" key"),
        json!("a b"),
        long,
        json!(null),
        json!(7),
    ] {
        let body = keyed_request(&session, "invalid", invalid.clone());
        let (status, error) = broker.post("/v1/tasks/create", &body).await;
        assert_eq!(status, 400, "{invalid}: {error}");
    }
    let mut canonical = keyed_request(&session, "canonical", json!("canonical-key"));
    canonical["arguments"] =
        json!({"receiverAgentId":"13","prompt":"opaque","requestKey":"canonical-key"});
    let (status, created) = broker.post("/v1/tasks/create", &canonical).await;
    assert_eq!(status, 200, "{created}");
    assert_eq!(created["requestKey"], "canonical-key");
    assert_eq!(remote.state.calls.lock().await.len(), 5);
    broker.stop().await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let session = broker.claim("restarted-host", true).await;
    let (status, third) = broker
        .post(
            "/v1/tasks/create",
            &keyed_request(&session, "call-4", key.clone()),
        )
        .await;
    assert_eq!(status, 200, "{third}");
    assert_eq!(third["data"]["taskId"], first["data"]["taskId"]);
    assert_eq!(remote.state.saved.lock().await.len(), 3);
    broker.stop().await;
}

#[tokio::test]
async fn unknown_outcome_with_caller_key_recovers_through_a_new_tool_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    remote.state.mode.store(1, Ordering::SeqCst);
    let broker = Broker::start(&path, &remote, Duration::from_millis(100)).await;
    let session = broker.claim("host", true).await;
    let (status, error) = broker
        .post("/v1/tasks/create", &request(&session, "lost-response"))
        .await;
    assert_eq!(status, 502);
    assert_eq!(error["error"]["outcome"], "unknown");
    let known = error["requestKey"].clone();
    assert!(known.is_string());
    sleep(Duration::from_millis(500)).await;
    remote.state.mode.store(0, Ordering::SeqCst);
    // The Agent retries with a new tool call (new requestId) and the known key.
    let (status, recovered) = broker
        .post(
            "/v1/tasks/create",
            &keyed_request(&session, "retry-call", known.clone()),
        )
        .await;
    assert_eq!(status, 200, "{recovered}");
    assert_eq!(recovered["requestKey"], known);
    assert_eq!(recovered["data"]["replayed"], true);
    assert_eq!(remote.state.saved.lock().await.len(), 1);
    broker.stop().await;
}

#[tokio::test]
async fn team_context_relays_parts_to_the_verified_team_and_rejects_bad_input_locally() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    let remote = Central::start(&path).await;
    let broker = Broker::start(&path, &remote, Duration::from_secs(1)).await;
    let unbound = broker.claim("other", false).await;
    let (_, result) = broker
        .post(
            "/v1/teams/context",
            &agent_request(&unbound, "get_team_context", json!({"part":"BACKEND"})),
        )
        .await;
    assert_eq!(result["error"]["code"], "AGENT_UNREGISTERED");
    let session = broker.claim("host", true).await;
    let (status, result) = broker
        .post(
            "/v1/teams/context",
            &agent_request(
                &session,
                "get_team_context",
                json!({"part":"BACKEND","counterpart":"FRONTEND"}),
            ),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["data"]["teamId"], 1);
    assert_eq!(result["data"]["query"]["part"], "BACKEND");
    assert_eq!(result["data"]["query"]["counterpart"], "FRONTEND");
    assert_eq!(result["data"]["version"], 3);
    assert_eq!(result["data"]["issues"][0]["evidence"][0], "102:5");
    let (status, result) = broker
        .post(
            "/v1/teams/context",
            &agent_request(&session, "get_team_context", json!({"part":"INFRA"})),
        )
        .await;
    assert_eq!(status, 200, "{result}");
    assert!(result["data"]["query"].get("counterpart").is_none());
    assert_eq!(remote.state.context_calls.load(Ordering::SeqCst), 2);
    for args in [
        json!({}),
        json!({"part":"DB"}),
        json!({"part":"BACKEND","counterpart":"db"}),
        json!({"part":"BACKEND","teamId":"2"}),
        json!({"part":"BACKEND","counterpart":""}),
    ] {
        let (status, result) = broker
            .post(
                "/v1/teams/context",
                &agent_request(&session, "get_team_context", args),
            )
            .await;
        assert_eq!(status, 400, "{result}");
    }
    let (status, result) = broker
        .post(
            "/v1/teams/context",
            &agent_request(&session, "list_agents", json!({"part":"BACKEND"})),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(result["error"]["code"], "VALIDATION_FAILED");
    assert_eq!(remote.state.context_calls.load(Ordering::SeqCst), 2);
    for (mode, code) in [
        (1, "CENTRAL_UNAUTHENTICATED"),
        (2, "CENTRAL_PROTOCOL_ERROR"),
    ] {
        remote.state.context_mode.store(mode, Ordering::SeqCst);
        let (status, result) = broker
            .post(
                "/v1/teams/context",
                &agent_request(&session, "get_team_context", json!({"part":"OTHER"})),
            )
            .await;
        assert_ne!(status, 401);
        assert_eq!(result["error"]["code"], code, "{mode}: {result}");
    }
    assert_eq!(remote.trapped.load(Ordering::SeqCst), 0);
    broker.stop().await;
}
