use devlink_broker::{
    central::{CentralIdentity, CentralState, ConnectionStatus},
    inbox::Inbox,
    notifications::{Notifications, TaskNotification},
    private_file,
    protocol::{TaskCreatedMessage, TaskCreatedPayload},
    session_api,
    sessions::{ReadyIdentityProvider, SessionManager},
    updates,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
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
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
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
