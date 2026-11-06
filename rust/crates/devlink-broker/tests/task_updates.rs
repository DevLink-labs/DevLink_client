use axum::{Json, Router, extract::State, routing::get};
use devlink_broker::{
    agent_context::ContextClient,
    central::{CentralIdentity, CentralState, ConnectionStatus, SessionHandle},
    inbox::Inbox,
    notifications::{Notifications, TaskNotification},
    private_file,
    protocol::{TaskCreatedMessage, TaskCreatedPayload, TaskMessageType},
    session_api,
    sessions::{ReadyIdentityProvider, SessionManager},
    task_messages::{TaskMessage, TaskMessages},
    updates,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{net::TcpListener, sync::RwLock, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const LOCAL: &str = "synthetic-local-token-updates";
const ORIGIN: &str = "ws://127.0.0.1:9";
const PROMPT: &str = "never-copy-prompt-into-updates";
const RESULT: &str = "never-copy-result-into-updates";

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    url: String,
    client: reqwest::Client,
    cancel: CancellationToken,
    local: JoinHandle<()>,
    central: CentralState,
    current: SessionHandle,
    remote: Option<(Arc<AtomicUsize>, JoinHandle<()>)>,
}
fn identity(connection: &str, legacy: bool) -> CentralIdentity {
    CentralIdentity {
        connection_id: connection.into(),
        agent_id: 12,
        user_id: (!legacy).then_some(3),
        team_id: (!legacy).then_some(1),
    }
}
async fn agent_context(State(calls): State<Arc<AtomicUsize>>) -> Json<Value> {
    calls.fetch_add(1, Ordering::SeqCst);
    Json(
        json!({"success":true,"data":{"agentId":12,"ownerUserId":3,"teamId":1,"toolType":"CODEX"}}),
    )
}
impl Fixture {
    async fn new() -> Self {
        Self::build(false).await
    }
    /// Legacy READY without user/team, resolved through a counted central context API.
    async fn legacy() -> Self {
        Self::build(true).await
    }
    async fn build(legacy: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let mut central = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        let mut remote = None;
        if legacy {
            let calls = Arc::new(AtomicUsize::new(0));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let context_url = format!("ws://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route("/api/v1/agents/me", get(agent_context))
                .with_state(calls.clone());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            central = central.with_context_client(
                ContextClient::new(&context_url, "synthetic-agent-token").unwrap(),
            );
            remote = Some((calls, server));
        }
        let current = central.begin_session().await;
        current.set_identity(identity("fixture", legacy)).await;
        current.set_status(ConnectionStatus::Connected).await;
        let sessions = SessionManager::new(
            Arc::new(ReadyIdentityProvider::new(central.clone())),
            central.clone(),
        );
        let cancel = CancellationToken::new();
        let router = session_api::router(Arc::new(LOCAL.into()), sessions.clone()).merge(
            updates::router(
                Arc::new(LOCAL.into()),
                sessions,
                path.clone(),
                ORIGIN,
                cancel.clone(),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let local = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            _dir: dir,
            path,
            url,
            client: reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            cancel,
            local,
            central,
            current,
            remote,
        }
    }
    fn central_calls(&self) -> usize {
        self.remote
            .as_ref()
            .map_or(0, |(calls, _)| calls.load(Ordering::SeqCst))
    }
    fn reset_central_calls(&self) {
        if let Some((calls, _)) = &self.remote {
            calls.store(0, Ordering::SeqCst);
        }
    }
    /// Start a new central connection generation with the given READY identity.
    async fn reconnect(&mut self, identity: CentralIdentity) {
        self.current = self.central.begin_session().await;
        self.current.set_identity(identity).await;
        self.current.set_status(ConnectionStatus::Connected).await;
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
        let text = data.to_string();
        assert!(!text.contains(PROMPT) && !text.contains(RESULT) && !text.contains(LOCAL));
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
    async fn updates(&self, session: &Value, args: Value) -> (u16, Value) {
        let mut context = session["principal"].clone();
        context["sessionId"] = session["sessionId"].clone();
        self.post(
            "/v1/updates",
            &json!({"requestId":uuid::Uuid::new_v4().to_string(),"operation":"get_updates","arguments":args,"context":context}),
        )
        .await
    }
    async fn page(&self, session: &Value, args: Value) -> Value {
        let (status, data) = self.updates(session, args).await;
        assert_eq!(status, 200, "{data}");
        data["data"].clone()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.local.abort();
        if let Some((_, server)) = &self.remote {
            server.abort();
        }
    }
}

fn created(task_id: u64, team_id: u64, receiver: u64) -> TaskCreatedMessage {
    TaskCreatedMessage {
        event_id: uuid::Uuid::new_v4().to_string(),
        payload: TaskCreatedPayload {
            task_id,
            team_id,
            requester_agent_id: 11,
            receiver_agent_id: receiver,
            prompt: PROMPT.into(),
            created_at: "2026-11-03T10:00:00".into(),
        },
    }
}
fn notice(task_id: u64, version: u64, status: &str) -> TaskNotification {
    let value = if status == "COMPLETED" {
        json!({"type":"TASK_COMPLETED","event_id":uuid::Uuid::new_v4().to_string(),"task_id":task_id,"version":version,
            "payload":{"status":status,"result_text":RESULT,"partially_modified":false,"started_at":"2026-11-03T10:00:00","completed_at":"2026-11-03T10:01:00"}})
    } else {
        json!({"type":"TASK_STATUS_CHANGED","event_id":uuid::Uuid::new_v4().to_string(),"task_id":task_id,"version":version,
            "payload":{"previous_status":if status=="FAILED" {"IN_PROGRESS"} else {"PENDING"},"status":status}})
    };
    serde_json::from_value(value).unwrap()
}
async fn receive(path: &Path, origin: &str, receiver: u64, message: TaskCreatedMessage) {
    Inbox::open(path)
        .unwrap()
        .record(origin.into(), receiver, message)
        .await
        .unwrap();
}
async fn notify(path: &Path, origin: &str, requester: u64, event: TaskNotification) {
    Notifications::open(path)
        .unwrap()
        .record(origin.into(), requester, event)
        .await
        .unwrap();
}

#[tokio::test]
async fn feed_is_scoped_ordered_compact_and_read_only() {
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    let empty = f.page(&session, json!({})).await;
    assert_eq!(empty["updates"], json!([]));
    assert_eq!(empty["hasMore"], false);
    assert_eq!(empty["cursorReset"], false);
    assert_eq!(empty["coverage"], "local");

    let first = created(101, 1, 12);
    receive(&f.path, ORIGIN, 12, first.clone()).await;
    // Other receivers, teams and origins never widen this actor's view.
    receive(&f.path, ORIGIN, 13, created(102, 1, 13)).await;
    receive(&f.path, ORIGIN, 12, created(103, 2, 12)).await;
    receive(&f.path, "ws://127.0.0.1:10", 12, created(104, 1, 12)).await;
    let status = notice(201, 1, "IN_PROGRESS");
    notify(&f.path, ORIGIN, 12, status.clone()).await;
    notify(&f.path, ORIGIN, 12, notice(201, 2, "COMPLETED")).await;
    notify(&f.path, ORIGIN, 13, notice(202, 1, "REJECTED")).await;

    let page = f
        .page(&session, json!({"cursor": empty["nextCursor"]}))
        .await;
    assert_eq!(page["cursorReset"], false);
    assert_eq!(page["hasMore"], false);
    assert_eq!(
        page["updates"],
        json!([
            {"eventId":first.event_id,"kind":"TASK_CREATED","direction":"received","taskId":"101","requesterAgentId":"11"},
            {"eventId":status.event_id(),"kind":"TASK_STATUS_CHANGED","direction":"sent","taskId":"201","taskVersion":"1","status":"IN_PROGRESS"},
            {"eventId":page["updates"][2]["eventId"],"kind":"TASK_COMPLETED","direction":"sent","taskId":"201","taskVersion":"2","status":"COMPLETED"},
        ])
    );
    // Reading never consumes: the same cursor returns the same page.
    assert_eq!(
        f.page(&session, json!({"cursor": empty["nextCursor"]}))
            .await,
        page
    );
    let caught_up = f
        .page(&session, json!({"cursor": page["nextCursor"]}))
        .await;
    assert_eq!(caught_up["updates"], json!([]));
    assert_eq!(caught_up["nextCursor"], page["nextCursor"]);

    let later = created(105, 1, 12);
    receive(&f.path, ORIGIN, 12, later.clone()).await;
    let next = f
        .page(&session, json!({"cursor": page["nextCursor"]}))
        .await;
    assert_eq!(next["updates"].as_array().unwrap().len(), 1);
    assert_eq!(next["updates"][0]["eventId"], later.event_id);
}

#[tokio::test]
async fn pages_follow_cursors_without_gaps_or_duplicates() {
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    for task in 1..=3 {
        receive(&f.path, ORIGIN, 12, created(task, 1, 12)).await;
    }
    for version in 1..=2 {
        notify(
            &f.path,
            ORIGIN,
            12,
            notice(50 + version, version, "IN_PROGRESS"),
        )
        .await;
    }
    let mut cursor = Value::Null;
    let mut seen = Vec::new();
    loop {
        let mut args = json!({"limit":2});
        if !cursor.is_null() {
            args["cursor"] = cursor.clone();
        }
        let page = f.page(&session, args).await;
        let items = page["updates"].as_array().unwrap();
        assert!(items.len() <= 2);
        seen.extend(
            items
                .iter()
                .map(|u| u["taskId"].as_str().unwrap().to_owned()),
        );
        cursor = page["nextCursor"].clone();
        if page["hasMore"] == false {
            break;
        }
    }
    assert_eq!(seen, ["1", "2", "3", "51", "52"]);
}

#[tokio::test]
async fn stale_or_foreign_cursors_restart_instead_of_skipping() {
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    receive(&f.path, ORIGIN, 12, created(1, 1, 12)).await;
    let page = f.page(&session, json!({})).await;
    let cursor = page["nextCursor"].as_str().unwrap().to_owned();

    // A tag that does not match this actor and history is restarted, not trusted.
    let mut forged = cursor.clone();
    let last = forged.pop().unwrap();
    forged.push(if last == '0' { '1' } else { '0' });
    let reset = f.page(&session, json!({"cursor": forged})).await;
    assert_eq!(reset["cursorReset"], true);
    assert_eq!(reset["updates"][0]["taskId"], "1");

    // A position beyond the retained history is restarted as well.
    let tag = cursor.rsplit('.').next().unwrap();
    let beyond = f
        .page(&session, json!({"cursor": format!("v1.9.0.{tag}")}))
        .await;
    assert_eq!(beyond["cursorReset"], true);

    // Clearing local retention invalidates old cursors even when new events arrive.
    Inbox::clear_config(&f.path).unwrap();
    receive(&f.path, ORIGIN, 12, created(2, 1, 12)).await;
    let cleared = f.page(&session, json!({"cursor": cursor})).await;
    assert_eq!(cleared["cursorReset"], true);
    assert_eq!(cleared["updates"].as_array().unwrap().len(), 1);
    assert_eq!(cleared["updates"][0]["taskId"], "2");
}

#[tokio::test]
async fn invalid_input_unbound_sessions_and_broken_storage_fail_closed() {
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    for args in [
        json!({"limit":0}),
        json!({"limit":51}),
        json!({"limit":"5"}),
        json!({"cursor":"not-a-cursor"}),
        json!({"cursor":""}),
        json!({"unknown":true}),
    ] {
        let (status, data) = f.updates(&session, args.clone()).await;
        assert_eq!(status, 400, "{args}: {data}");
    }
    let (status, data) = f.updates(&session, json!({})).await;
    assert_eq!(status, 200, "{data}");

    // A forged actor context is never trusted to widen the scope.
    let mut forged = session.clone();
    forged["principal"]["agentId"] = json!("13");
    let (status, _) = f.updates(&forged, json!({})).await;
    assert_ne!(status, 200);

    // Broken local storage is reported without leaking its contents.
    private_file::write(
        &Notifications::path(&f.path),
        &json!({"schema_version":1,"events":"secret-internal"}),
        1024,
    )
    .unwrap();
    let (status, data) = f.updates(&session, json!({})).await;
    assert_eq!(status, 503);
    assert_eq!(data["error"]["code"], "LOCAL_UPDATES_UNAVAILABLE");
    assert!(!data.to_string().contains("secret-internal"));
}

#[tokio::test]
async fn unbound_sessions_cannot_read_updates() {
    let f = Fixture::new().await;
    let session = f.claim(false).await;
    let (status, data) = f.updates(&session, json!({})).await;
    assert_eq!(status, 409, "{data}");
    assert_eq!(data["error"]["code"], "AGENT_UNREGISTERED");
}

#[tokio::test]
async fn cleared_store_redelivering_the_last_event_still_resets_the_cursor() {
    // Review regression: [A, B] consumed, store cleared, then [C, B] stored. The old
    // tag matched position 2 and last B, so C was silently skipped.
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    let (a, b) = (notice(701, 1, "IN_PROGRESS"), notice(702, 1, "IN_PROGRESS"));
    notify(&f.path, ORIGIN, 12, a).await;
    notify(&f.path, ORIGIN, 12, b.clone()).await;
    let consumed = f.page(&session, json!({})).await;
    assert_eq!(consumed["updates"].as_array().unwrap().len(), 2);

    Notifications::clear_config(&f.path).unwrap();
    let c = notice(703, 1, "IN_PROGRESS");
    notify(&f.path, ORIGIN, 12, c.clone()).await;
    notify(&f.path, ORIGIN, 12, b.clone()).await;
    let page = f
        .page(&session, json!({"cursor": consumed["nextCursor"]}))
        .await;
    assert_eq!(page["cursorReset"], true);
    let ids: Vec<_> = page["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["eventId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, [c.event_id().to_owned(), b.event_id().to_owned()]);

    // The same holds for the received side.
    let first = created(801, 1, 12);
    receive(&f.path, ORIGIN, 12, first.clone()).await;
    let caught_up = f
        .page(&session, json!({"cursor": page["nextCursor"]}))
        .await;
    assert_eq!(caught_up["updates"].as_array().unwrap().len(), 1);
    Inbox::clear_config(&f.path).unwrap();
    let replacement = created(802, 1, 12);
    receive(&f.path, ORIGIN, 12, replacement.clone()).await;
    let reset = f
        .page(&session, json!({"cursor": caught_up["nextCursor"]}))
        .await;
    assert_eq!(reset["cursorReset"], true);
    assert_eq!(reset["updates"][0]["eventId"], replacement.event_id);
}

#[tokio::test]
async fn local_reads_survive_a_central_disconnect_without_central_io() {
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    receive(&f.path, ORIGIN, 12, created(901, 1, 12)).await;
    for status in [
        ConnectionStatus::Reconnecting,
        ConnectionStatus::Connecting,
        ConnectionStatus::Disconnected,
    ] {
        f.current.set_status(status).await;
        let page = f.page(&session, json!({})).await;
        assert_eq!(page["updates"][0]["taskId"], "901", "{status:?}");
    }
    // Authentication failure is not a transient disconnect.
    f.current.set_status(ConnectionStatus::AuthFailed).await;
    let (status, data) = f.updates(&session, json!({})).await;
    assert_eq!(status, 503, "{data}");
    assert_eq!(data["error"]["code"], "CENTRAL_UNAUTHENTICATED");
}

#[tokio::test]
async fn legacy_reconnect_of_the_same_agent_serves_local_reads_without_central_requests() {
    let mut f = Fixture::legacy().await;
    let session = f.claim(true).await;
    assert_eq!(session["principal"]["userId"], "3");
    receive(&f.path, ORIGIN, 12, created(911, 1, 12)).await;
    f.reconnect(identity("legacy-reconnect", true)).await;
    f.reset_central_calls();
    let page = f.page(&session, json!({})).await;
    assert_eq!(page["updates"][0]["taskId"], "911");
    assert_eq!(f.central_calls(), 0);
}

#[tokio::test]
async fn a_different_connected_subject_invalidates_local_reads() {
    let mut f = Fixture::new().await;
    let session = f.claim(true).await;
    receive(&f.path, ORIGIN, 12, created(921, 1, 12)).await;
    f.reconnect(CentralIdentity {
        connection_id: "other-subject".into(),
        agent_id: 13,
        user_id: Some(4),
        team_id: Some(1),
    })
    .await;
    let (status, data) = f.updates(&session, json!({})).await;
    assert_eq!(status, 409, "{data}");
    assert_eq!(data["error"]["code"], "SESSION_EXPIRED");
}

async fn arrive(path: &Path, origin: &str, agent: u64, ids: &[(u64, &str)], cursor: &str) {
    let items = ids
        .iter()
        .map(|(id, kind)| TaskMessage {
            event_id: id.to_string(),
            task_id: 300,
            sequence: id.to_string(),
            event_type: if *kind == "QUESTION" {
                TaskMessageType::Question
            } else {
                TaskMessageType::Answer
            },
            created_at: "2026-11-05T10:00:00Z".into(),
        })
        .collect();
    TaskMessages::open(path)
        .unwrap()
        .apply(origin, agent, items, cursor.into())
        .await
        .unwrap();
}

#[tokio::test]
async fn question_and_answer_arrivals_follow_other_sources_without_bodies() {
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    receive(&f.path, ORIGIN, 12, created(101, 1, 12)).await;
    let before = f.page(&session, json!({})).await;
    assert!(
        before["nextCursor"]
            .as_str()
            .unwrap()
            .starts_with("v3.1.0.0.0.")
    );

    arrive(
        &f.path,
        ORIGIN,
        12,
        &[(501, "QUESTION"), (502, "ANSWER")],
        "c1",
    )
    .await;
    // Another Agent's or origin's arrivals never widen this view.
    arrive(&f.path, ORIGIN, 13, &[(601, "QUESTION")], "c1").await;
    arrive(&f.path, "ws://127.0.0.1:10", 12, &[(701, "QUESTION")], "c1").await;
    let page = f
        .page(&session, json!({"cursor": before["nextCursor"]}))
        .await;
    assert_eq!(page["cursorReset"], false);
    assert_eq!(
        page["updates"],
        json!([
            {"eventId":"501","kind":"TASK_QUESTION_CREATED","direction":"received","taskId":"300","sequence":"501"},
            {"eventId":"502","kind":"TASK_ANSWER_CREATED","direction":"received","taskId":"300","sequence":"502"},
        ])
    );
    assert!(
        page["nextCursor"]
            .as_str()
            .unwrap()
            .starts_with("v3.1.0.2.0.")
    );

    // Paging crosses into the new source without gaps.
    let first = f.page(&session, json!({"limit":2})).await;
    assert_eq!(first["hasMore"], true);
    let rest = f
        .page(&session, json!({"cursor": first["nextCursor"], "limit":2}))
        .await;
    assert_eq!(rest["updates"][0]["eventId"], "502");
    assert_eq!(rest["hasMore"], false);

    // Clearing arrivals resets old cursors instead of skipping a re-stored record.
    TaskMessages::clear_config(&f.path).unwrap();
    arrive(
        &f.path,
        ORIGIN,
        12,
        &[(503, "QUESTION"), (502, "ANSWER")],
        "c2",
    )
    .await;
    let reset = f
        .page(&session, json!({"cursor": page["nextCursor"]}))
        .await;
    assert_eq!(reset["cursorReset"], true);
    let ids: Vec<_> = reset["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["eventId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[1..], ["503", "502"]);
}

#[tokio::test]
async fn arrivals_show_after_sixteen_other_scopes_pulled_empty() {
    let f = Fixture::new().await;
    let session = f.claim(true).await;
    // Re-registered Agents and other test servers each left an empty pull behind.
    for agent in 1001..=1008 {
        arrive(&f.path, ORIGIN, agent, &[], "old").await;
    }
    for port in 11..=18 {
        arrive(&f.path, &format!("ws://127.0.0.1:{port}"), 12, &[], "old").await;
    }
    // The seventeenth scope is this session's Agent.
    arrive(
        &f.path,
        ORIGIN,
        12,
        &[(501, "QUESTION"), (502, "ANSWER")],
        "c1",
    )
    .await;
    let page = f.page(&session, json!({})).await;
    assert_eq!(
        page["updates"],
        json!([
            {"eventId":"501","kind":"TASK_QUESTION_CREATED","direction":"received","taskId":"300","sequence":"501"},
            {"eventId":"502","kind":"TASK_ANSWER_CREATED","direction":"received","taskId":"300","sequence":"502"},
        ])
    );
}
