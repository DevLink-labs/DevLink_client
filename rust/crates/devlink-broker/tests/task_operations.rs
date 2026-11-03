use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use devlink_broker::{
    central::{CentralIdentity, CentralState, ConnectionStatus},
    session_api,
    sessions::{ReadyIdentityProvider, SessionManager},
    task_client::TaskClient,
    task_routes,
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

const LOCAL: &str = "synthetic-local-token-94";
const AGENT: &str = "synthetic-agent-token-94";
const SECRET: &str = "never-copy-central-error-body";
#[derive(Clone)]
struct Remote {
    mode: Arc<AtomicU8>,
    calls: Arc<Mutex<Vec<Value>>>,
    contexts: Arc<AtomicUsize>,
}
fn check(headers: &HeaderMap) {
    assert_eq!(headers["authorization"], format!("Bearer {AGENT}"));
    assert!(!headers.contains_key("cookie"));
}
async fn context(State(s): State<Remote>, headers: HeaderMap) -> Json<Value> {
    check(&headers);
    s.contexts.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"success":true,"data":{"agentId":12,"teamId":1,"ownerUserId":3,"name":"Agent","toolType":"CODEX","projectName":"test","status":"ONLINE","createdAt":"2026-10-30T01:00:00"}}),
    )
}
fn summary(id: u64, status: &str, direction: &str) -> Value {
    json!({"taskId":id,"teamId":1,"requesterAgent":{"agentId":if direction=="sent"{12}else{13},"name":"Requester"},"receiverAgent":{"agentId":if direction=="sent"{13}else{12},"name":"Receiver"},"status":status,"createdAt":"2026-10-30T01:00:00","startedAt":null,"completedAt":null})
}
fn detail(id: u64) -> Value {
    let mut data = summary(id, "COMPLETED", "received");
    data["prompt"] = json!("original prompt");
    data["version"] = json!(2);
    data["resultText"] = json!(" summary\n검증 결과 그대로 ");
    data["failureReason"] = Value::Null;
    data["rejectionReason"] = Value::Null;
    data["partiallyModified"] = json!(false);
    data["taskFiles"] = json!([{"fileId":9223372036854775807_u64,"relativePath":"src/없는 파일.rs","changeType":"RENAMED","diffText":"- old\n+ new"}]);
    data["events"] = json!([{"eventType":"STATUS_CHANGED","message":"원문 이력","createdAt":"2026-10-30T01:01:00"}]);
    data["unexpectedToken"] = json!(SECRET);
    data
}
async fn reply(s: &Remote, mut data: Value) -> Response {
    let mode = s.mode.load(Ordering::SeqCst);
    if mode == 11 {
        sleep(Duration::from_millis(350)).await;
    }
    match mode {
        1 => (StatusCode::UNAUTHORIZED, SECRET).into_response(),
        2 => (StatusCode::FORBIDDEN, SECRET).into_response(),
        3 => (
            StatusCode::CONFLICT,
            Json(json!({"success":false,"code":"TASK_VERSION_CONFLICT","message":SECRET})),
        )
            .into_response(),
        4 => (
            StatusCode::CONFLICT,
            Json(json!({"success":false,"code":"TASK_STATE_CONFLICT","message":SECRET})),
        )
            .into_response(),
        5 => (
            StatusCode::NOT_FOUND,
            Json(json!({"success":false,"code":"RESOURCE_NOT_FOUND","message":SECRET})),
        )
            .into_response(),
        6 => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"success":false,"code":"INTERNAL_SERVER_ERROR","message":SECRET})),
        )
            .into_response(),
        7 => (
            StatusCode::FOUND,
            [("location", "http://127.0.0.1:1/do-not-follow")],
            SECRET,
        )
            .into_response(),
        8 => (StatusCode::OK, "x".repeat(256 * 1024 + 1)).into_response(),
        9 => Json(json!({"success":true,"data":{"taskId":101}})).into_response(),
        12 => {
            let stream = futures_util::stream::unfold(0, |i| async move {
                match i {
                    0 => Some((Ok::<_, std::io::Error>("{\"success\":"), 1)),
                    1 => {
                        sleep(Duration::from_millis(350)).await;
                        Some((Ok("true,\"data\":null}"), 2))
                    }
                    _ => None,
                }
            });
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        _ => {
            if mode == 10 {
                data["taskId"] = json!(999);
            }
            if mode == 13 {
                data["teamId"] = json!(999);
            }
            if mode == 16 {
                data["status"] = json!("FUTURE_STATUS");
            }
            if mode == 17 {
                data["version"] = json!("2");
            }
            Json(json!({"success":true,"data":data,"message":SECRET})).into_response()
        }
    }
}
async fn list(
    State(s): State<Remote>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    check(&headers);
    assert_eq!(q["scope"], "mine");
    assert!(!q.contains_key("agentId"));
    s.calls
        .lock()
        .await
        .push(json!({"operation":"list","query":q}));
    let page = q["page"].parse::<u32>().unwrap();
    let size = q["size"].parse::<u32>().unwrap();
    let mut item = summary(
        101 + u64::from(page),
        q.get("status").map(String::as_str).unwrap_or("PENDING"),
        &q["direction"],
    );
    if s.mode.load(Ordering::SeqCst) == 14 {
        item["receiverAgent"]["agentId"] = json!(999);
    }
    reply(&s,json!({"items":[item],"page":page,"size":size,"totalElements":3,"totalPages":3_u32.div_ceil(size),"hasNext":page+1<3_u32.div_ceil(size)})).await
}
async fn result(Path(id): Path<u64>, State(s): State<Remote>, headers: HeaderMap) -> Response {
    check(&headers);
    s.calls
        .lock()
        .await
        .push(json!({"operation":"result","taskId":id}));
    reply(&s, detail(id)).await
}
async fn status(
    Path(id): Path<u64>,
    State(s): State<Remote>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    check(&headers);
    assert!(body.get("agentId").is_none());
    assert!(body.get("requestKey").is_none());
    s.calls
        .lock()
        .await
        .push(json!({"operation":"status","taskId":id,"body":body}));
    reply(&s,json!({"taskId":id,"status":body["status"],"version":body["version"].as_u64().unwrap()+1,
        "failureReason":body.get("failureReason"),"rejectionReason":null,"partiallyModified":body.get("partiallyModified").unwrap_or(&json!(false)),"startedAt":"2026-10-30T01:00:00","completedAt":null})).await
}
async fn complete(
    Path(id): Path<u64>,
    State(s): State<Remote>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    check(&headers);
    assert!(body.get("agentId").is_none());
    assert!(body.get("requestKey").is_none());
    s.calls
        .lock()
        .await
        .push(json!({"operation":"complete","taskId":id,"body":body}));
    reply(&s,json!({"taskId":id,"status":"COMPLETED","version":body["version"].as_u64().unwrap()+1,"completedAt":"2026-10-30T01:02:00"})).await
}
struct Fixture {
    remote: Remote,
    url: String,
    client: reqwest::Client,
    cancel: CancellationToken,
    local: JoinHandle<()>,
    server: JoinHandle<()>,
}
impl Fixture {
    async fn new(budget: Duration) -> Self {
        let remote = Remote {
            mode: Arc::default(),
            calls: Arc::default(),
            contexts: Arc::default(),
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .route("/api/v1/agents/me", get(context))
            .route("/api/v1/teams/1/tasks", get(list))
            .route("/api/v1/tasks/{id}", get(result))
            .route("/api/v1/tasks/{id}/status", patch(status))
            .route("/api/v1/tasks/{id}/complete", post(complete))
            .with_state(remote.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let central = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        let current = central.begin_session().await;
        current
            .set_identity(CentralIdentity {
                connection_id: "fixture".into(),
                agent_id: 12,
                user_id: Some(3),
                team_id: Some(1),
            })
            .await;
        current.set_status(ConnectionStatus::Connected).await;
        let sessions = SessionManager::new(
            Arc::new(ReadyIdentityProvider::new(central.clone())),
            central,
        );
        let cancel = CancellationToken::new();
        let router = session_api::router(Arc::new(LOCAL.into()), sessions.clone()).merge(
            task_routes::router(
                Arc::new(LOCAL.into()),
                sessions,
                TaskClient::with_timeout(&format!("ws://{address}"), AGENT, budget).unwrap(),
                cancel.clone(),
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let local = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            remote,
            url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            cancel,
            local,
            server,
        }
    }
    async fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        let r = self
            .client
            .post(format!("{}{path}", self.url))
            .bearer_auth(LOCAL)
            .json(body)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        let data: Value = r.json().await.unwrap();
        assert!(!data.to_string().contains(SECRET));
        assert!(!data.to_string().contains(AGENT));
        (status, data)
    }
    async fn claim(&self, bound: bool) -> Value {
        let mut body = json!({"requestId":"claim","instanceId":"owner","mode":"fresh"});
        if bound {
            body["agentId"] = json!("12");
        }
        let (status, data) = self.post("/v1/sessions", &body).await;
        assert_eq!(status, 200);
        data["data"].clone()
    }
    async fn wait_call(&self) {
        timeout(Duration::from_secs(3), async {
            while self.remote.calls.lock().await.is_empty() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn release(&self, session: &Value) {
        self.client
            .delete(format!(
                "{}/v1/sessions/{}",
                self.url,
                session["sessionId"].as_str().unwrap()
            ))
            .bearer_auth(LOCAL)
            .json(&json!({"requestId":"release","instanceId":"owner"}))
            .send()
            .await
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.local.abort();
        self.server.abort();
    }
}
fn request(session: &Value, operation: &str, args: Value) -> Value {
    let mut context = session["principal"].clone();
    context["sessionId"] = session["sessionId"].clone();
    json!({"requestId":uuid::Uuid::new_v4().to_string(),"operation":operation,"arguments":args,"context":context})
}
fn status_args() -> Value {
    json!({"taskId":"101","version":"0","status":"IN_PROGRESS","requestKey":"known-key"})
}

#[tokio::test]
async fn status_and_complete_map_exact_provider_fields_and_long_precision() {
    let f = Fixture::new(Duration::from_secs(1)).await;
    let session = f.claim(true).await;
    let mut args = status_args();
    args["taskId"] = json!("9223372036854775807");
    args["version"] = json!("9223372036854775806");
    let (status, response) = f
        .post(
            "/v1/tasks/status",
            &request(&session, "update_task_status", args),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["taskId"], "9223372036854775807");
    assert_eq!(response["data"]["version"], "9223372036854775807");
    assert_eq!(response["requestKey"], "known-key");
    let args = json!({"taskId":"101","version":1,"status":"FAILED","reason":"  exact failure\n사유  ","partiallyModified":false});
    let (status, response) = f
        .post(
            "/v1/tasks/status",
            &request(&session, "update_task_status", args),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["failureReason"], "  exact failure\n사유  ");
    assert_eq!(response["data"]["partiallyModified"], false);
    let files = json!([{"relativePath":"src/nonexistent.rs","changeType":"RENAMED","diffText":"- old\n+ new"},{"relativePath":"gone.rs","changeType":"DELETED","diffText":null}]);
    let args = json!({"taskId":"101","version":"1","resultText":"  result\n검증 그대로 ","taskFiles":files});
    let (status, response) = f
        .post(
            "/v1/tasks/complete",
            &request(&session, "complete_task", args),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["version"], "2");
    let calls = f.remote.calls.lock().await;
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[2]["body"]["taskFiles"], files);
    assert_eq!(calls[2]["body"]["resultText"], "  result\n검증 그대로 ");
}
#[tokio::test]
async fn list_cursors_bind_actor_filters_and_size_and_result_preserves_files() {
    let f = Fixture::new(Duration::from_secs(1)).await;
    let session = f.claim(true).await;
    let (status, response) = f
        .post(
            "/v1/tasks/list",
            &request(&session, "get_tasks", json!({"limit":1})),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    let cursor = response["data"]["nextCursor"].as_str().unwrap();
    let (status, response) = f
        .post(
            "/v1/tasks/list",
            &request(&session, "get_tasks", json!({"limit":1,"cursor":cursor})),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["page"], 1);
    for args in [
        json!({"limit":2,"cursor":cursor}),
        json!({"direction":"sent","limit":1,"cursor":cursor}),
        json!({"cursor":"garbage"}),
        json!({"scope":"team"}),
        json!({"size":1,"limit":1}),
        json!({"page":2147483648_u64}),
    ] {
        assert_eq!(
            f.post("/v1/tasks/list", &request(&session, "get_tasks", args))
                .await
                .0,
            400
        );
    }
    let (status, response) = f
        .post(
            "/v1/tasks/list",
            &request(
                &session,
                "get_tasks",
                json!({"direction":"sent","status":"REJECTED"}),
            ),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["items"][0]["status"], "REJECTED");
    let (status, response) = f
        .post(
            "/v1/tasks/result",
            &request(&session, "get_task_result", json!({"taskId":"101"})),
        )
        .await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(
        response["data"]["taskFiles"][0]["fileId"],
        "9223372036854775807"
    );
    assert_eq!(response["data"]["taskFiles"][0]["changeType"], "RENAMED");
    assert!(response["data"].get("unexpectedToken").is_none());
    assert_eq!(f.remote.calls.lock().await.len(), 4);
}
#[tokio::test]
async fn invalid_legacy_dtos_and_actor_inputs_never_reach_central() {
    let f = Fixture::new(Duration::from_secs(1)).await;
    let unbound = f.claim(false).await;
    assert_eq!(
        f.post(
            "/v1/tasks/status",
            &request(&unbound, "update_task_status", status_args())
        )
        .await
        .1["error"]["code"],
        "AGENT_UNREGISTERED"
    );
    f.release(&unbound).await;
    let session = f.claim(true).await;
    for field in ["userId", "teamId", "agentId"] {
        let mut body = request(&session, "update_task_status", status_args());
        body["context"][field] = json!("999");
        assert_eq!(f.post("/v1/tasks/status", &body).await.0, 403);
    }
    for args in [
        json!({"taskId":"101","status":"IN_PROGRESS"}),
        json!({"taskId":"101","version":0,"status":"FAILED","reason":"reason"}),
        json!({"taskId":"101","version":0,"status":"REJECTED"}),
        json!({"taskId":"01","version":0,"status":"IN_PROGRESS"}),
        json!({"taskId":"101","version":"01","status":"IN_PROGRESS"}),
        json!({"taskId":"101","version":1.5,"status":"IN_PROGRESS"}),
        json!({"taskId":"101","version":"9223372036854775808","status":"IN_PROGRESS"}),
        json!({"taskId":"101","version":0,"status":"IN_PROGRESS","agentId":"999"}),
    ] {
        assert_eq!(
            f.post(
                "/v1/tasks/status",
                &request(&session, "update_task_status", args)
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        f.post(
            "/v1/tasks/complete",
            &request(
                &session,
                "complete_task",
                json!({"taskId":"101","result":{"summary":"legacy","verification":"must not drop"}})
            )
        )
        .await
        .0,
        400
    );
    for args in [
        json!({"taskId":"101","version":0,"resultText":"  "}),
        json!({"taskId":"101","version":0,"resultText":"result","verification":"must not drop"}),
        json!({"taskId":"101","version":0,"resultText":"result","taskFiles":[{"relativePath":" ","changeType":"ADDED"}]}),
        json!({"taskId":"101","version":0,"resultText":"result","taskFiles":[{"relativePath":"😀".repeat(251),"changeType":"ADDED"}]}),
        json!({"taskId":"101","version":0,"resultText":"result","taskFiles":[{"relativePath":"x","changeType":"UNKNOWN"}]}),
    ] {
        assert_eq!(
            f.post(
                "/v1/tasks/complete",
                &request(&session, "complete_task", args)
            )
            .await
            .0,
            400
        );
    }
    let body = request(&session, "get_task_result", json!({"taskId":"101"}));
    assert_eq!(
        f.client
            .post(format!("{}/v1/tasks/result", f.url))
            .bearer_auth(AGENT)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        f.client
            .post(format!("{}/v1/tasks/result", f.url))
            .bearer_auth(LOCAL)
            .header("Origin", "https://example.test")
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert!(f.remote.calls.lock().await.is_empty());
    assert_eq!(f.remote.contexts.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn central_errors_and_body_timeouts_preserve_known_ids_and_keys_without_retry() {
    let f = Fixture::new(Duration::from_millis(100)).await;
    let session = f.claim(true).await;
    for (index, mode, code, outcome) in [
        (0, 1, "CENTRAL_UNAUTHENTICATED", "rejected"),
        (1, 2, "FORBIDDEN", "rejected"),
        (2, 3, "TASK_VERSION_CONFLICT", "rejected"),
        (3, 4, "TASK_STATE_CONFLICT", "rejected"),
        (4, 5, "RESOURCE_NOT_FOUND", "rejected"),
        (5, 6, "CENTRAL_UNAVAILABLE", "unknown"),
        (6, 7, "CENTRAL_PROTOCOL_ERROR", "unknown"),
        (7, 8, "CENTRAL_PROTOCOL_ERROR", "unknown"),
        (8, 9, "CENTRAL_PROTOCOL_ERROR", "unknown"),
        (9, 10, "CENTRAL_PROTOCOL_ERROR", "unknown"),
        (10, 11, "CENTRAL_TIMEOUT", "unknown"),
        (11, 12, "CENTRAL_TIMEOUT", "unknown"),
        (12, 16, "CENTRAL_PROTOCOL_ERROR", "unknown"),
        (13, 17, "CENTRAL_PROTOCOL_ERROR", "unknown"),
    ] {
        f.remote.mode.store(mode, Ordering::SeqCst);
        let (status, response) = f
            .post(
                "/v1/tasks/status",
                &request(&session, "update_task_status", status_args()),
            )
            .await;
        assert_ne!(status, 401);
        assert_eq!(response["error"]["code"], code, "mode {mode}: {response}");
        assert_eq!(response["error"]["outcome"], outcome);
        assert_eq!(response["error"]["taskId"], "101");
        assert_eq!(response["requestKey"], "known-key");
        assert_eq!(f.remote.calls.lock().await.len(), index + 1);
    }
}
#[tokio::test]
async fn reads_revalidate_owner_and_dispatched_mutations_keep_their_known_result() {
    for mutation in [false, true] {
        let f = Fixture::new(Duration::from_secs(1)).await;
        let session = f.claim(true).await;
        f.remote.mode.store(11, Ordering::SeqCst);
        let body = if mutation {
            request(&session, "update_task_status", status_args())
        } else {
            request(&session, "get_task_result", json!({"taskId":"101"}))
        };
        let path = if mutation {
            "/v1/tasks/status"
        } else {
            "/v1/tasks/result"
        };
        let work = f.post(path, &body);
        let release = async {
            f.wait_call().await;
            f.release(&session).await;
        };
        let ((status, response), _) = tokio::join!(work, release);
        if mutation {
            assert_eq!(status, 200, "{response}");
            assert_eq!(response["data"]["status"], "IN_PROGRESS");
        } else {
            assert_eq!(response["error"]["code"], "SESSION_EXPIRED");
            assert_eq!(response["error"]["taskId"], "101");
        }
    }
}
#[tokio::test]
async fn cancellation_and_cross_scope_responses_are_not_success() {
    let f = Fixture::new(Duration::from_secs(1)).await;
    let session = f.claim(true).await;
    for mode in [13, 14] {
        f.remote.mode.store(mode, Ordering::SeqCst);
        let path = if mode == 13 {
            "/v1/tasks/result"
        } else {
            "/v1/tasks/list"
        };
        let (op, args) = if mode == 13 {
            ("get_task_result", json!({"taskId":"101"}))
        } else {
            ("get_tasks", json!({}))
        };
        let (_, response) = f.post(path, &request(&session, op, args)).await;
        assert_eq!(response["error"]["code"], "CENTRAL_PROTOCOL_ERROR");
    }
    f.remote.mode.store(11, Ordering::SeqCst);
    let body = request(
        &session,
        "complete_task",
        json!({"taskId":"101","version":0,"resultText":"result"}),
    );
    let work = f.post("/v1/tasks/complete", &body);
    let cancel = async {
        timeout(Duration::from_secs(3), async {
            while f.remote.calls.lock().await.len() < 3 {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        f.cancel.cancel();
    };
    let ((_, response), _) = tokio::join!(work, cancel);
    assert_eq!(response["error"]["outcome"], "unknown");
    assert_eq!(response["error"]["taskId"], "101");
    assert_eq!(f.remote.calls.lock().await.len(), 3);
}

#[tokio::test]
async fn completion_defaults_empty_files_but_never_silently_discards_extra_fields() {
    let f = Fixture::new(Duration::from_secs(1)).await;
    let session = f.claim(true).await;
    for files in [None, Some(Value::Null), Some(json!([]))] {
        let mut args = json!({"taskId":"101","version":0,"resultText":" exact result "});
        if let Some(files) = files {
            args["taskFiles"] = files;
        }
        let (status, response) = f
            .post(
                "/v1/tasks/complete",
                &request(&session, "complete_task", args),
            )
            .await;
        assert_eq!(status, 200, "{response}");
    }
    for call in f.remote.calls.lock().await.iter() {
        assert_eq!(call["body"]["taskFiles"], json!([]));
    }
    let (status, response) = f
        .post(
            "/v1/tasks/complete",
            &request(
                &session,
                "complete_task",
                json!({
                    "taskId":"101", "version":0, "resultText":"x".repeat(128 * 1024)
                }),
            ),
        )
        .await;
    assert_eq!(status, 413, "{response}");
    assert_eq!(f.remote.calls.lock().await.len(), 3);
}
