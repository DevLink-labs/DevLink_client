//! Question/answer arrival sync against mock central WebSocket and HTTP endpoints.
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use devlink_broker::{
    central::{self, CentralState, ConnectionStatus},
    config::Config,
    task_client::TaskClient,
    task_messages::{self, TaskMessage, TaskMessages},
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::RwLock,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "synthetic-agent-token";
const BODY: &str = "never-store-question-body";

type Pages = Arc<Mutex<HashMap<String, (u16, Value)>>>;
#[derive(Clone, Default)]
struct Central {
    /// Cursor of each pull (`None` when omitted), in arrival order.
    log: Arc<Mutex<Vec<Option<String>>>>,
    pages: Pages,
}
impl Central {
    fn page(&self, cursor: &str, status: u16, body: Value) {
        self.pages
            .lock()
            .unwrap()
            .insert(cursor.to_owned(), (status, body));
    }
    fn log(&self) -> Vec<Option<String>> {
        self.log.lock().unwrap().clone()
    }
}
async fn task_messages_api(
    State(central): State<Central>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> (StatusCode, Json<Value>) {
    assert_eq!(
        headers.get("authorization").unwrap(),
        &format!("Bearer {TOKEN}")
    );
    assert_eq!(query.get("limit").map(String::as_str), Some("100"));
    let cursor = query.get("cursor").cloned();
    central.log.lock().unwrap().push(cursor.clone());
    let key = cursor.unwrap_or_else(|| "-".into());
    let (status, body) = central.pages.lock().unwrap().get(&key).cloned().unwrap_or((
        500,
        json!({"success":false,"data":null,"code":"INTERNAL_SERVER_ERROR"}),
    ));
    (StatusCode::from_u16(status).unwrap(), Json(body))
}

fn item(event_id: u64, sequence: u64, kind: &str) -> Value {
    // Central never sends a body here; an extra field must not leak into storage either.
    json!({"eventId":event_id.to_string(),"taskId":100,"sequence":sequence.to_string(),
        "eventType":kind,"createdAt":"2026-11-03T05:30:00Z","message":BODY})
}
fn ok(items: Vec<Value>, next: &str, has_next: bool) -> Value {
    json!({"success":true,"data":{"items":items,"nextCursor":next,"hasNext":has_next},"code":null,"message":null})
}
fn notice(kind: &str, receiver: u64, event_id: u64) -> String {
    let event_type = if kind == "TASK_QUESTION_CREATED" {
        "QUESTION"
    } else {
        "ANSWER"
    };
    json!({"event_id":uuid::Uuid::new_v4().to_string(),"type":kind,"task_id":100,"receiver_agent_id":receiver,
        "payload":{"task_event_id":event_id.to_string(),"sequence":event_id.to_string(),"event_type":event_type}})
    .to_string()
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    origin: String,
    central: Central,
    store: TaskMessages,
    listener: Option<TcpListener>,
    cancel: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("ws://{}", listener.local_addr().unwrap());
        let store = TaskMessages::open(&path).unwrap();
        Self {
            _dir: dir,
            path,
            origin,
            central: Central::default(),
            store,
            listener: Some(listener),
            cancel: CancellationToken::new(),
            tasks: Vec::new(),
        }
    }
    /// Start the broker's central loop and sync worker against the mock endpoints.
    async fn start(&mut self, interval: Duration) {
        let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let rest = format!("ws://{}", http.local_addr().unwrap());
        let app = Router::new()
            .route("/api/v1/agents/me/task-messages", get(task_messages_api))
            .with_state(self.central.clone());
        self.tasks.push(tokio::spawn(async move {
            axum::serve(http, app).await.unwrap();
        }));
        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
            .with_task_messages(self.store.clone());
        let config = Config {
            server_url: self.origin.clone(),
            auto_reconnect: true,
            reconnect_initial_ms: 50,
            reconnect_max_ms: 100,
            ..Config::default()
        };
        self.tasks.push(tokio::spawn(task_messages::run(
            self.store.clone(),
            state.clone(),
            TaskClient::new(&rest, TOKEN).unwrap(),
            self.origin.clone(),
            interval,
            self.cancel.clone(),
        )));
        self.tasks.push(tokio::spawn(central::run_with_state(
            config,
            TOKEN.into(),
            state,
            self.cancel.clone(),
        )));
    }
    async fn accept(&self) -> WebSocketStream<tokio::net::TcpStream> {
        let (stream, _) = self.listener.as_ref().unwrap().accept().await.unwrap();
        let mut socket = accept_async(stream).await.unwrap();
        socket
            .send(Message::Text(
                json!({"type":"BROKER_READY","payload":{"connection_id":uuid::Uuid::new_v4().to_string(),
                    "agent_id":12,"user_id":3,"team_id":1}})
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
        socket
    }
    fn events(&self) -> Vec<TaskMessage> {
        TaskMessages::events_config(&self.path, &self.origin, 12).unwrap()
    }
    async fn wait(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        if timeout(Duration::from_secs(10), async {
            while !done(self) {
                sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .is_err()
        {
            panic!(
                "timed out waiting for {what}; pulls={:?} diagnostics={:?}",
                self.central.log(),
                self.store.diagnostics()
            );
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// No broker frame may follow a notice: no ACK, no other reply.
async fn assert_silent(socket: &mut WebSocketStream<tokio::net::TcpStream>) {
    if let Ok(Some(Ok(Message::Text(text)))) =
        timeout(Duration::from_millis(300), socket.next()).await
    {
        panic!("Unexpected broker frame after notice: {text}");
    }
}

fn stored_without_body(path: &Path) {
    let raw = std::fs::read_to_string(TaskMessages::path(path)).unwrap();
    assert!(!raw.contains(BODY) && !raw.contains(TOKEN));
    devlink_broker::credentials::verify_private_permissions(&TaskMessages::path(path)).unwrap();
}

#[tokio::test]
async fn ready_and_notices_pull_by_cursor_dedupe_and_reconnect_catches_up() {
    let mut f = Fixture::new().await;
    f.central
        .page("-", 200, ok(vec![item(501, 1, "QUESTION")], "c1", true));
    f.central
        .page("c1", 200, ok(vec![item(502, 2, "ANSWER")], "c2", false));
    f.central.page("c2", 200, ok(vec![], "c2", false));
    f.start(Duration::from_secs(3600)).await;

    // READY alone triggers a pull, which follows hasNext through the stored cursor.
    let mut socket = f.accept().await;
    f.wait("initial pull", |f| f.events().len() == 2).await;
    assert_eq!(f.central.log(), [None, Some("c1".into())]);
    assert_eq!(
        f.store.cursor(&f.origin, 12).await.unwrap().as_deref(),
        Some("c2")
    );
    stored_without_body(&f.path);

    // A misrouted notice is counted and never pulls or replies.
    socket
        .send(Message::Text(notice("TASK_ANSWER_CREATED", 99, 503).into()))
        .await
        .unwrap();
    assert_silent(&mut socket).await;
    assert_eq!(f.store.diagnostics().rejected_events, 1);
    assert_eq!(f.central.log().len(), 2);

    // Notices are signals only: the pulled page, including a replayed eventId, is the record.
    f.central.page(
        "c2",
        200,
        ok(
            vec![item(502, 2, "ANSWER"), item(503, 3, "QUESTION")],
            "c3",
            false,
        ),
    );
    f.central.page("c3", 200, ok(vec![], "c3", false));
    for _ in 0..3 {
        socket
            .send(Message::Text(
                notice("TASK_QUESTION_CREATED", 12, 503).into(),
            ))
            .await
            .unwrap();
    }
    assert_silent(&mut socket).await;
    f.wait("notice pull", |f| f.events().len() == 3).await;
    let ids: Vec<_> = f.events().into_iter().map(|e| e.event_id).collect();
    assert_eq!(ids, ["501", "502", "503"]);
    assert_eq!(f.store.diagnostics().rejected_events, 1);
    // Three notices coalesce into at most two rounds, each from the saved cursor.
    sleep(Duration::from_millis(200)).await;
    let log = f.central.log();
    assert!(
        log[2..] == [Some("c2".into())] || log[2..] == [Some("c2".into()), Some("c3".into())],
        "{log:?}"
    );

    // A missed notice is recovered on reconnect: the new READY pulls from the stored cursor.
    f.central
        .page("c3", 200, ok(vec![item(504, 4, "ANSWER")], "c4", false));
    socket.close(None).await.unwrap();
    drop(socket);
    let _socket = f.accept().await;
    f.wait("reconnect pull", |f| f.events().len() == 4).await;
    assert_eq!(f.central.log().last(), Some(&Some("c3".into())));
    assert_eq!(f.store.diagnostics().failed_pulls, 0);
}

#[tokio::test]
async fn rejected_cursor_resyncs_and_safety_net_retries_after_failure() {
    let mut f = Fixture::new().await;
    // A previous run left a record and a cursor the server no longer accepts.
    f.store
        .apply(
            &f.origin,
            12,
            vec![TaskMessage {
                event_id: "501".into(),
                task_id: 100,
                sequence: "1".into(),
                event_type: devlink_broker::protocol::TaskMessageType::Question,
                created_at: "2026-11-03T05:30:00Z".into(),
            }],
            "stale".into(),
        )
        .await
        .unwrap();
    f.central.page(
        "stale",
        400,
        json!({"success":false,"data":null,"code":"VALIDATION_FAILED","message":"invalid"}),
    );
    f.central.page(
        "-",
        200,
        ok(
            vec![item(501, 1, "QUESTION"), item(502, 2, "ANSWER")],
            "c1",
            false,
        ),
    );
    // "c1" is not scripted yet: the safety-net pull fails with 500 and keeps the cursor.
    f.start(Duration::from_millis(1000)).await;
    let _socket = f.accept().await;
    f.wait("resync", |f| f.events().len() == 2).await;
    assert_eq!(f.central.log()[..2], [Some("stale".into()), None]);

    f.wait("failed safety-net pull", |f| {
        f.store.diagnostics().failed_pulls >= 1
    })
    .await;
    assert_eq!(
        f.store.diagnostics().last_error.as_deref(),
        Some("CENTRAL_UNAVAILABLE")
    );
    assert_eq!(
        f.store.cursor(&f.origin, 12).await.unwrap().as_deref(),
        Some("c1")
    );
    f.central
        .page("c1", 200, ok(vec![item(503, 3, "QUESTION")], "c2", false));
    f.central.page("c2", 200, ok(vec![], "c2", false));
    f.wait("retry on next safety-net tick", |f| f.events().len() == 3)
        .await;
    stored_without_body(&f.path);
}

#[tokio::test]
async fn malformed_pages_never_advance_the_cursor() {
    let mut f = Fixture::new().await;
    // hasNext without items would spin forever.
    f.central.page("-", 200, ok(vec![], "c1", true));
    f.start(Duration::from_secs(3600)).await;
    let mut socket = f.accept().await;
    f.wait("protocol failure", |f| {
        f.store.diagnostics().failed_pulls == 1
    })
    .await;
    assert_eq!(f.store.cursor(&f.origin, 12).await.unwrap(), None);

    for bad in [
        json!({"eventId":"0","taskId":100,"sequence":"1","eventType":"QUESTION","createdAt":"t"}),
        json!({"eventId":"5","taskId":100,"sequence":"x","eventType":"QUESTION","createdAt":"t"}),
        json!({"eventId":"5","taskId":100,"sequence":"1","eventType":"STATUS","createdAt":"t"}),
    ] {
        let before = f.store.diagnostics().failed_pulls;
        f.central.page("-", 200, ok(vec![bad], "c1", false));
        socket
            .send(Message::Text(notice("TASK_QUESTION_CREATED", 12, 5).into()))
            .await
            .unwrap();
        f.wait("rejected page", |f| {
            f.store.diagnostics().failed_pulls == before + 1
        })
        .await;
        assert!(f.events().is_empty());
        assert_eq!(f.store.cursor(&f.origin, 12).await.unwrap(), None);
    }
}

#[tokio::test]
async fn foreign_cursor_403_resyncs_once_and_a_repeated_cursor_stops_the_round() {
    let mut f = Fixture::new().await;
    f.store
        .apply(&f.origin, 12, vec![], "foreign".into())
        .await
        .unwrap();
    // Another Agent's or team's cursor is a plain 403, even with a malformed body.
    f.central.page("foreign", 403, json!({"unexpected":true}));
    f.central
        .page("-", 200, ok(vec![item(501, 1, "QUESTION")], "c1", true));
    // A server repeating the cursor with hasNext must not spin through the page budget.
    f.central
        .page("c1", 200, ok(vec![item(502, 2, "ANSWER")], "c1", true));
    f.start(Duration::from_secs(3600)).await;
    let _socket = f.accept().await;
    f.wait("repeated cursor failure", |f| {
        f.store.diagnostics().failed_pulls == 1
    })
    .await;
    assert_eq!(
        f.central.log(),
        [Some("foreign".into()), None, Some("c1".into())]
    );
    assert_eq!(
        f.store.diagnostics().last_error.as_deref(),
        Some("CENTRAL_PROTOCOL_ERROR")
    );
    let ids: Vec<_> = f.events().into_iter().map(|e| e.event_id).collect();
    assert_eq!(ids, ["501"]);
    assert_eq!(
        f.store.cursor(&f.origin, 12).await.unwrap().as_deref(),
        Some("c1")
    );
}

#[tokio::test]
async fn an_agent_after_sixteen_empty_scopes_still_stores_its_pulls() {
    let mut f = Fixture::new().await;
    // Sixteen earlier Agents on this server pulled nothing but left their cursors.
    for agent in 1001..=1016 {
        f.store
            .apply(&f.origin, agent, vec![], format!("old-{agent}"))
            .await
            .unwrap();
    }
    f.central.page(
        "-",
        200,
        ok(
            vec![item(501, 1, "QUESTION"), item(502, 2, "ANSWER")],
            "c1",
            false,
        ),
    );
    f.start(Duration::from_secs(3600)).await;
    let _socket = f.accept().await;
    f.wait("seventeenth agent pull", |f| f.events().len() == 2)
        .await;
    assert_eq!(f.central.log(), [None]);
    assert_eq!(
        f.store.cursor(&f.origin, 12).await.unwrap().as_deref(),
        Some("c1")
    );
    for agent in 1001..=1016 {
        assert_eq!(
            f.store.cursor(&f.origin, agent).await.unwrap(),
            Some(format!("old-{agent}"))
        );
    }
    assert_eq!(f.store.diagnostics().failed_pulls, 0);
    stored_without_body(&f.path);
}
