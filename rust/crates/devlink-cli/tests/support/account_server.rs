use axum::{
    Json, Router,
    extract::{Path, State, WebSocketUpgrade, ws::Message},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use devlink_broker::config::{self, Config};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
};
use tokio::{net::TcpListener, task::JoinHandle};

pub const PASSWORD: &str = "test-password-do-not-print";
pub const COOKIE: &str = "post-login-cookie-do-not-print";
pub const CSRF: &str = "post-login-csrf-do-not-print";
pub const INVITE: &str = "synthetic-invite-code-read-from-stdin-only";
pub const AGENT_SECRET: &str = "synthetic-agent-token-never-print";

#[derive(Clone, Default)]
pub struct MockState {
    pub expired: Arc<AtomicBool>,
    pub logout_error: Arc<AtomicBool>,
    pub redirect_login: Arc<AtomicBool>,
    pub changed_user: Arc<AtomicBool>,
    pub me_failure: Arc<AtomicBool>,
    pub me_mode: Arc<AtomicU8>,
    pub calls: Arc<Mutex<Vec<String>>>,
    pub joined: Arc<AtomicBool>,
    pub membership_missing: Arc<AtomicBool>,
    pub teams: Arc<Mutex<std::collections::BTreeMap<u64, Value>>>,
    pub user_id: Arc<AtomicU64>,
    pub agents: Arc<Mutex<std::collections::BTreeMap<u64, Value>>>,
    pub tokens: Arc<Mutex<std::collections::BTreeMap<String, u64>>>,
    pub create_calls: Arc<AtomicUsize>,
    pub token_calls: Arc<AtomicUsize>,
    pub create_mode: Arc<AtomicU8>,
    pub token_mode: Arc<AtomicU8>,
    pub wrong_context: Arc<AtomicBool>,
    pub csrf_failure: Arc<AtomicBool>,
    pub csrf_mode: Arc<AtomicU8>,
    pub pause_token: Arc<AtomicBool>,
    pub token_entered: Arc<tokio::sync::Notify>,
    pub token_resume: Arc<tokio::sync::Notify>,
}

pub fn success(value: Value) -> Response {
    Json(json!({"success":true,"data":value,"code":null,"message":null})).into_response()
}
pub fn failure(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"success":false,"data":null,"code":code,"message":format!("{COOKIE} {PASSWORD}")}))).into_response()
}
fn cookie(headers: &HeaderMap, value: &str) -> bool {
    headers
        .get("cookie")
        .is_some_and(|v| v == &format!("JSESSIONID={value}"))
}
fn user(state: &MockState) -> Value {
    let id = state.user_id.load(Ordering::SeqCst);
    json!({"userId":if id==0{51}else{id},"email":"verified@example.test","name":"Verified\u{1b}[31m\u{202e}","nickname":null,"createdAt":"2026-10-29T00:00:00"})
}

async fn csrf(State(state): State<MockState>, headers: HeaderMap) -> Response {
    assert!(!headers.contains_key("authorization"));
    state.calls.lock().unwrap().push("csrf".into());
    if state.csrf_failure.load(Ordering::SeqCst) {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "UNTRUSTED");
    }
    let authenticated = cookie(&headers, COOKIE);
    match state.csrf_mode.load(Ordering::SeqCst) {
        1 => return success(json!({"headerName":"untrusted-header","token":"untrusted-csrf"})),
        2 => return success(json!({"headerName":"X-CSRF-TOKEN","token":"untrusted\ncsrf"})),
        _ => {}
    }
    let token = if authenticated {
        CSRF
    } else {
        "pre-login-csrf"
    };
    let mut response =
        success(json!({"headerName":"X-CSRF-TOKEN","parameterName":"_csrf","token":token}));
    if !authenticated {
        response.headers_mut().insert(
            "set-cookie",
            "JSESSIONID=pre-login-cookie; Path=/; HttpOnly"
                .parse()
                .unwrap(),
        );
    }
    response
}

async fn login(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.calls.lock().unwrap().push("login".into());
    assert!(cookie(&headers, "pre-login-cookie"));
    assert_eq!(headers.get("x-csrf-token").unwrap(), "pre-login-csrf");
    assert!(!headers.contains_key("authorization"));
    if state.redirect_login.load(Ordering::SeqCst) {
        return (StatusCode::FOUND, [("location", "/sink")]).into_response();
    }
    if body["password"] != PASSWORD {
        return failure(StatusCode::UNAUTHORIZED, "INVALID_CREDENTIALS");
    }
    state.expired.store(false, Ordering::SeqCst);
    let mut response = success(user(&state));
    response.headers_mut().insert(
        "set-cookie",
        format!("JSESSIONID={COOKIE}; Path=/; HttpOnly")
            .parse()
            .unwrap(),
    );
    response
}

async fn me(State(state): State<MockState>, headers: HeaderMap) -> Response {
    state.calls.lock().unwrap().push("me".into());
    assert!(!headers.contains_key("authorization"));
    if !cookie(&headers, COOKIE) || state.expired.load(Ordering::SeqCst) {
        return failure(StatusCode::UNAUTHORIZED, "AUTHENTICATION_REQUIRED");
    }
    if state.me_failure.load(Ordering::SeqCst) {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "UNTRUSTED_SECRET_RESPONSE");
    }
    match state.me_mode.load(Ordering::SeqCst) {
        1 => return failure(StatusCode::FORBIDDEN, "ACCESS_DENIED"),
        2 => return (StatusCode::OK, "{invalid-response").into_response(),
        _ => {}
    }
    let mut value = user(&state);
    if state.changed_user.load(Ordering::SeqCst) {
        value["userId"] = json!(52);
    }
    let mut response = success(value);
    if state.me_mode.load(Ordering::SeqCst) == 3 {
        response.headers_mut().insert(
            "set-cookie",
            "JSESSIONID=; Max-Age=0; Path=/; HttpOnly".parse().unwrap(),
        );
    }
    response
}

fn agent_value(id: u64, user: u64, team: u64, name: &str, tool: Value, project: Value) -> Value {
    json!({"agentId":id,"teamId":team,"ownerUserId":user,"name":name,"roleDescription":null,"toolType":tool,"projectName":project,"status":"OFFLINE","lastSeenAt":null,"createdAt":"2026-10-29T00:00:00"})
}
fn agent_for(state: &MockState, id: u64) -> Option<Value> {
    match id {
        301 => Some(agent_value(
            301,
            51,
            71,
            "Existing",
            json!("CODEX"),
            json!("Existing project"),
        )),
        302 => Some(agent_value(
            302,
            88,
            71,
            "Other owner",
            json!("CODEX"),
            json!("Other project"),
        )),
        303 => Some(agent_value(
            303,
            51,
            99,
            "Other team",
            json!("CODEX"),
            json!("Other project"),
        )),
        _ => state.agents.lock().unwrap().get(&id).cloned(),
    }
}
async fn agent_create(
    Path(team): Path<u64>,
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert!(authorized(&state, &headers, true));
    assert_eq!(team, 71);
    state.create_calls.fetch_add(1, Ordering::SeqCst);
    if state.create_mode.load(Ordering::SeqCst) == 2 {
        return failure(StatusCode::BAD_REQUEST, "VALIDATION_FAILED");
    }
    if state.create_mode.load(Ordering::SeqCst) == 3 {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "UNTRUSTED");
    }
    assert!(body.get("ownerUserId").is_none() && body.get("agentId").is_none());
    let count = state.agents.lock().unwrap().len();
    let id = 201 + count as u64;
    let user = user(&state)["userId"].as_u64().unwrap();
    let mut agent = agent_value(
        id,
        user,
        team,
        body["name"].as_str().unwrap(),
        body["toolType"].clone(),
        body["projectName"].clone(),
    );
    agent["roleDescription"] = body["roleDescription"].clone();
    state.agents.lock().unwrap().insert(id, agent.clone());
    if state.create_mode.load(Ordering::SeqCst) == 1 {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "UNTRUSTED");
    }
    success(agent)
}
async fn agent_list(
    Path(team): Path<u64>,
    State(state): State<MockState>,
    headers: HeaderMap,
) -> Response {
    assert!(authorized(&state, &headers, false));
    assert_eq!(team, 71);
    let mut agents = state
        .agents
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    agents.push(agent_for(&state, 301).unwrap());
    agents.push(agent_for(&state, 302).unwrap());
    success(json!(agents))
}
async fn agent_get(
    Path(id): Path<u64>,
    State(state): State<MockState>,
    headers: HeaderMap,
) -> Response {
    assert!(authorized(&state, &headers, false));
    if id == 303 {
        return failure(StatusCode::FORBIDDEN, "ACCESS_DENIED");
    }
    agent_for(&state, id)
        .map(success)
        .unwrap_or_else(|| failure(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND"))
}
async fn agent_token(
    Path(id): Path<u64>,
    State(state): State<MockState>,
    headers: HeaderMap,
) -> Response {
    assert!(authorized(&state, &headers, true));
    if state.pause_token.load(Ordering::SeqCst) {
        state.token_entered.notify_one();
        state.token_resume.notified().await;
    }
    let Some(agent) = agent_for(&state, id) else {
        return failure(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND");
    };
    if agent["ownerUserId"] != user(&state)["userId"] {
        return failure(StatusCode::FORBIDDEN, "ACCESS_DENIED");
    }
    let count = state.token_calls.fetch_add(1, Ordering::SeqCst) + 1;
    let token = format!("{AGENT_SECRET}-{id}-{count}");
    let mut tokens = state.tokens.lock().unwrap();
    tokens.retain(|_, owner| *owner != id);
    tokens.insert(token.clone(), id);
    drop(tokens);
    if state.token_mode.load(Ordering::SeqCst) == 1 {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "UNTRUSTED");
    }
    success(json!({"agentId":id,"token":token}))
}
fn token_agent(state: &MockState, headers: &HeaderMap) -> Option<u64> {
    let bearer = headers
        .get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")?;
    state.tokens.lock().unwrap().get(bearer).copied()
}
async fn agent_context(State(state): State<MockState>, headers: HeaderMap) -> Response {
    assert!(!headers.contains_key("cookie"));
    let Some(mut id) = token_agent(&state, &headers) else {
        return failure(StatusCode::UNAUTHORIZED, "AUTHENTICATION_REQUIRED");
    };
    if state.wrong_context.load(Ordering::SeqCst) {
        id = 301;
    }
    agent_for(&state, id)
        .map(success)
        .unwrap_or_else(|| failure(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND"))
}
async fn websocket(
    State(state): State<MockState>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    use futures_util::StreamExt;
    let Some(id) = token_agent(&state, &headers) else {
        return failure(StatusCode::UNAUTHORIZED, "AUTHENTICATION_REQUIRED");
    };
    upgrade.on_upgrade(move |mut socket|async move{
        state.calls.lock().unwrap().push(format!("ws:{id}"));
        let ready=json!({"type":"BROKER_READY","payload":{"connection_id":format!("profile-{id}"),"agent_id":id}});
        if socket.send(Message::Text(ready.to_string().into())).await.is_err(){return;}
        while let Some(Ok(message))=socket.next().await{
            if let Message::Text(text)=message
                && let Ok(value)=serde_json::from_str::<Value>(&text)
                    && value["type"]=="HEARTBEAT_PING"{
                        let pong=json!({"type":"HEARTBEAT_PONG","payload":{"ping_id":value["payload"]["ping_id"]}});
                        if socket.send(Message::Text(pong.to_string().into())).await.is_err(){break;}
            }
        }
    })
}

async fn logout(State(state): State<MockState>, headers: HeaderMap) -> Response {
    state.calls.lock().unwrap().push("logout".into());
    assert!(cookie(&headers, COOKIE));
    assert_eq!(headers.get("x-csrf-token").unwrap(), CSRF);
    if state.logout_error.load(Ordering::SeqCst) {
        return failure(StatusCode::SERVICE_UNAVAILABLE, "UNTRUSTED_SECRET_RESPONSE");
    }
    state.expired.store(true, Ordering::SeqCst);
    success(Value::Null)
}

fn team_value(id: u64, name: &str, role: &str) -> Value {
    json!({"teamId":id,"name":name,"myRole":role,"createdAt":"2026-10-29T00:00:00"})
}
fn team_for(state: &MockState, id: u64) -> Option<Value> {
    match id {
        71 => Some(team_value(71, "Owned team", "OWNER")),
        72 if state.joined.load(Ordering::SeqCst) => Some(team_value(72, "Joined team", "MEMBER")),
        _ => state.teams.lock().unwrap().get(&id).cloned(),
    }
}
fn authorized(state: &MockState, headers: &HeaderMap, mutation: bool) -> bool {
    assert!(!headers.contains_key("authorization"));
    cookie(headers, COOKIE)
        && !state.expired.load(Ordering::SeqCst)
        && (!mutation || headers.get("x-csrf-token").is_some_and(|v| v == CSRF))
}
async fn team_list(State(state): State<MockState>, headers: HeaderMap) -> Response {
    state.calls.lock().unwrap().push("team_list".into());
    if !authorized(&state, &headers, false) {
        return failure(StatusCode::UNAUTHORIZED, "AUTHENTICATION_REQUIRED");
    }
    let mut teams = vec![team_for(&state, 71).unwrap()];
    if let Some(team) = team_for(&state, 72) {
        teams.push(team);
    }
    teams.extend(state.teams.lock().unwrap().values().cloned());
    success(json!(teams))
}
async fn team_create(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.calls.lock().unwrap().push("team_create".into());
    assert!(authorized(&state, &headers, true));
    assert_eq!(
        body.as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["name"]
    );
    let mut teams = state.teams.lock().unwrap();
    let id = 101 + teams.len() as u64;
    let team = team_value(id, body["name"].as_str().unwrap(), "OWNER");
    teams.insert(id, team.clone());
    let mut created = team;
    created["inviteCode"] = json!(INVITE);
    success(created)
}
async fn team_detail(
    Path(id): Path<u64>,
    State(state): State<MockState>,
    headers: HeaderMap,
) -> Response {
    state.calls.lock().unwrap().push("team_detail".into());
    if !authorized(&state, &headers, false) {
        return failure(StatusCode::UNAUTHORIZED, "AUTHENTICATION_REQUIRED");
    }
    if id == 99 {
        return failure(StatusCode::FORBIDDEN, "ACCESS_DENIED");
    }
    team_for(&state, id)
        .map(success)
        .unwrap_or_else(|| failure(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND"))
}
async fn team_members(
    Path(id): Path<u64>,
    State(state): State<MockState>,
    headers: HeaderMap,
) -> Response {
    state.calls.lock().unwrap().push("team_members".into());
    if !authorized(&state, &headers, false) {
        return failure(StatusCode::UNAUTHORIZED, "AUTHENTICATION_REQUIRED");
    }
    if id == 99 {
        return failure(StatusCode::FORBIDDEN, "ACCESS_DENIED");
    }
    let Some(team) = team_for(&state, id) else {
        return failure(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND");
    };
    let user = if state.membership_missing.load(Ordering::SeqCst) {
        777
    } else {
        user(&state)["userId"].as_u64().unwrap()
    };
    success(
        json!([{"userId":user,"name":"Verified","nickname":null,"role":team["myRole"],"joinedAt":"2026-10-29T00:00:00"}]),
    )
}
async fn team_join(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.calls.lock().unwrap().push("team_join".into());
    assert!(authorized(&state, &headers, true));
    if body["inviteCode"] != INVITE {
        return failure(StatusCode::NOT_FOUND, "RESOURCE_NOT_FOUND");
    }
    let replayed = state.joined.swap(true, Ordering::SeqCst);
    success(
        json!({"teamId":72,"name":"Joined team","myRole":"MEMBER","joinedAt":"2026-10-29T00:00:00","replayed":replayed}),
    )
}

pub struct Fixture {
    pub _root: tempfile::TempDir,
    pub path: PathBuf,
    pub state: MockState,
    pub server: JoinHandle<()>,
}

impl Fixture {
    pub async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let state = MockState::default();
        let app = Router::new()
            .route(
                "/api/v1/agents/teams/{id}",
                get(agent_list).post(agent_create),
            )
            .route("/api/v1/agents/{id}", get(agent_get))
            .route("/api/v1/agents/{id}/token", post(agent_token))
            .route("/api/v1/agents/me", get(agent_context))
            .route("/ws/broker", get(websocket))
            .route("/api/v1/teams", get(team_list).post(team_create))
            .route("/api/v1/teams/join", post(team_join))
            .route("/api/v1/teams/{id}", get(team_detail))
            .route("/api/v1/teams/{id}/members", get(team_members))
            .route("/api/v1/auth/csrf", get(csrf))
            .route("/api/v1/auth/login", post(login))
            .route("/api/v1/auth/users/me", get(me))
            .route("/api/v1/auth/logout", post(logout))
            .route("/sink", get(|| async { StatusCode::IM_A_TEAPOT }))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        config::atomic_json(
            &path,
            &Config {
                server_url: format!("ws://{address}"),
                local_port: 0,
                ..Config::default()
            },
        )
        .unwrap();
        Self {
            _root: root,
            path,
            state,
            server,
        }
    }

    pub fn run(&self, args: &[&str], input: Option<&str>) -> Output {
        run_at(&self.path, args, input)
    }

    pub fn login(&self) -> Output {
        login_at(&self.path)
    }
}

pub fn run_at(path: &std::path::Path, args: &[&str], input: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devlink"));
    command
        .arg("--config")
        .arg(path)
        .args(args)
        .env_remove("DEVLINK_TOKEN")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(input) = input {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

pub fn login_at(path: &std::path::Path) -> Output {
    run_at(
        path,
        &[
            "auth",
            "login",
            "--email",
            "input@example.test",
            "--password-stdin",
        ],
        Some(&format!("{PASSWORD}\n")),
    )
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

pub fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
pub fn assert_no_secrets(output: &Output) {
    let value = text(output);
    for secret in [
        PASSWORD,
        COOKIE,
        CSRF,
        AGENT_SECRET,
        "pre-login-cookie",
        "pre-login-csrf",
    ] {
        assert!(!value.contains(secret));
    }
}
