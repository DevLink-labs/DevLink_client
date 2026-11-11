//! Final status recovery for sent tasks.
//!
//! Central delivers TASK_STATUS_CHANGED and TASK_COMPLETED to the requester only over the
//! socket and gives up after a few attempts, so a status that changed while this broker
//! was off or disconnected never reached the local stores. For each task in the sent task
//! ledger that has no retained final status, the broker reads the Agent task detail
//! (`GET /api/v1/tasks/{taskId}`) on every central READY and right before the overdue
//! check decides the task. A COMPLETED, FAILED or REJECTED answer is stored as the same
//! notification the socket would have carried (type, taskId, taskVersion, status), with an
//! eventId derived from the task, so the update feed, `get_updates` and the Host hook show
//! it once and a late socket event for the task is dropped as a duplicate.
//!
//! Lookups only read central. A failed, unauthorized or missing (404) lookup changes
//! nothing and waits for the next READY or check. Each pass makes at most one call per
//! unfinished task and at most `MAX_PER_PASS` calls, one at a time. Tasks past the cap
//! are not decided overdue until they are looked up, and the round a READY starts goes on
//! in later passes until every task was asked once.
use crate::{
    central::{CentralState, ConnectionStatus, endpoint_url},
    notifications::{
        CompletedPayload, Notifications, StatusPayload, TaskNotification, recovered_event_id,
    },
    sent_tasks,
    task_client::TaskClient,
    task_operations::{TaskStatus, Version},
};
use anyhow::Result;
use serde::Deserialize;
use std::{collections::HashMap, path::Path, sync::Arc, time::Instant};
use tokio::sync::{Mutex, Notify};

/// Calls in one pass. Tasks not reached go first in the next pass.
pub const MAX_PER_PASS: usize = 32;

/// What one lookup decided about one task.
#[derive(Debug, PartialEq, Eq)]
enum Lookup {
    /// Final in central: the notification to store.
    Final(Box<TaskNotification>),
    /// Still PENDING or IN_PROGRESS.
    Open,
    /// This task only (deleted, malformed or foreign answer): try it again later.
    Skip,
    /// Central unavailable or the credential refused: end the pass.
    Stop,
}

#[derive(Clone)]
pub struct Recovery {
    client: TaskClient,
    central: CentralState,
    store: Notifications,
    origin: String,
    wake: Arc<Notify>,
    /// Last lookup per task in this process, so a capped pass reaches every task in turn.
    attempted: Arc<Mutex<HashMap<u64, Instant>>>,
}

impl Recovery {
    /// `wake` is the READY trigger given to the central connection
    /// (`CentralState::with_sent_task_wake`).
    pub fn new(
        client: TaskClient,
        central: CentralState,
        store: Notifications,
        server_url: &str,
        wake: Arc<Notify>,
    ) -> Result<Self> {
        Ok(Self {
            client,
            central,
            store,
            origin: endpoint_url(server_url)?.to_string(),
            wake,
            attempted: Arc::default(),
        })
    }

    pub(crate) fn wake(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// The Agent of the current READY, with its team when central reported one.
    async fn connected(&self) -> Option<(u64, Option<u64>)> {
        self.central
            .with_ready_context(|status, identity, _| {
                (status == ConnectionStatus::Connected)
                    .then(|| identity.map(|id| (id.agent_id, id.team_id)))
                    .flatten()
            })
            .await
    }

    /// Look up unfinished tasks of the connected Agent in `scope` and store each final
    /// status.
    pub(crate) async fn pass(&self, config_path: &Path, scope: Scope) -> Pass {
        let mut outcome = Pass::default();
        // Held for the whole pass, so passes never overlap.
        let mut attempted = self.attempted.lock().await;
        let Some((agent, team)) = self.connected().await else {
            return outcome;
        };
        let config = config_path.to_path_buf();
        let origin = self.origin.clone();
        let due = match scope {
            Scope::Due { minutes, now_ms } => Some((minutes, now_ms)),
            Scope::Round { .. } => None,
        };
        let Ok(Ok(tasks)) = tokio::task::spawn_blocking(move || {
            sent_tasks::unfinished(&config, &origin, agent, due)
        })
        .await
        else {
            return outcome;
        };
        let mut tasks: Vec<_> = tasks
            .into_iter()
            .filter(|task| team.is_none_or(|team| team == task.team_id))
            .collect();
        if let Scope::Round { since } = scope {
            let open: std::collections::HashSet<u64> = tasks.iter().map(|t| t.task_id).collect();
            attempted.retain(|task, _| open.contains(task));
            // A round asks each task once, however many passes that takes.
            tasks.retain(|task| attempted.get(&task.task_id).is_none_or(|at| *at < since));
        }
        // Never looked up first, then the longest ago; newer tasks first among equals.
        tasks.sort_by_key(|task| {
            (
                attempted.get(&task.task_id).copied(),
                std::cmp::Reverse(task.created_at_ms),
            )
        });
        let rest = tasks.split_off(tasks.len().min(MAX_PER_PASS));
        let mut stopped = false;
        for task in tasks {
            attempted.insert(task.task_id, Instant::now());
            let lookup = decode(
                self.client.task_detail::<Detail>(task.task_id).await,
                &self.origin,
                agent,
                task,
            );
            match lookup {
                Lookup::Final(event) => {
                    // The answer was read for this Agent; another Agent connected since must
                    // not receive it. A plain disconnect keeps the verified answer.
                    if self.connected().await.is_some_and(|(now, _)| now != agent) {
                        stopped = true;
                        break;
                    }
                    if self
                        .store
                        .record(self.origin.clone(), agent, *event)
                        .await
                        .is_ok()
                    {
                        outcome.stored += 1;
                    }
                    let (config, origin) = (config_path.to_path_buf(), self.origin.clone());
                    let _ = tokio::task::spawn_blocking(move || {
                        sent_tasks::settle(&config, &origin, agent, task.task_id)
                    })
                    .await;
                }
                Lookup::Open | Lookup::Skip => {}
                Lookup::Stop => {
                    stopped = true;
                    break;
                }
            }
        }
        // Tasks past the cap were never asked: the overdue scan waits for their lookup, and
        // a round continues with them. Tasks a failed central left unasked are decided as
        // before and wait for the next READY.
        outcome.more = !stopped && !rest.is_empty();
        outcome.held = rest
            .into_iter()
            .map(|task| (self.origin.clone(), agent, task.task_id))
            .collect();
        outcome
    }
}

/// Which unfinished tasks a pass looks up.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Scope {
    /// Those the overdue scan at `now_ms` decides, right before it does.
    Due { minutes: u64, now_ms: u64 },
    /// Every task not looked up since `since` (the READY that started the round).
    Round { since: Instant },
}

/// What one pass did.
#[derive(Debug, Default)]
pub(crate) struct Pass {
    /// Final statuses stored.
    pub stored: usize,
    /// Tasks in scope left unasked by the per-pass cap, as (origin, agent, taskId): the
    /// overdue scan must not decide them before they are looked up.
    pub held: std::collections::HashSet<(String, u64, u64)>,
    /// A round has tasks left for another pass.
    pub more: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentRef {
    agent_id: u64,
}

/// The fields of central `TaskDetailResponse` a final notification needs. The rest
/// (prompt, file diffs, event history) is skipped while the answer streams in.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Detail {
    task_id: u64,
    team_id: u64,
    requester_agent: AgentRef,
    status: TaskStatus,
    version: u64,
    result_text: Option<String>,
    #[serde(default)]
    partially_modified: bool,
    started_at: Option<String>,
    completed_at: Option<String>,
}

fn decode(
    answer: Result<Detail, crate::task_client::TaskError>,
    origin: &str,
    agent: u64,
    task: sent_tasks::Unfinished,
) -> Lookup {
    let detail = match answer {
        Ok(detail) => detail,
        // Central said this task is gone or the request was not valid for it.
        Err(error) if matches!(error.code, "RESOURCE_NOT_FOUND" | "VALIDATION_FAILED") => {
            return Lookup::Skip;
        }
        // A malformed answer (or one without the fields of `Detail`) is about this reply only.
        Err(error) if error.code == "CENTRAL_PROTOCOL_ERROR" => return Lookup::Skip,
        // Unavailable, timed out, 401 or 403: the next call would fail the same way.
        Err(_) => return Lookup::Stop,
    };
    if detail.task_id != task.task_id
        || detail.team_id != task.team_id
        || detail.requester_agent.agent_id != agent
        || detail.version > i64::MAX as u64
    {
        return Lookup::Skip;
    }
    let completed = detail.status == TaskStatus::Completed;
    let event_id = recovered_event_id(origin, agent, detail.task_id, detail.version, completed);
    let (task_id, version) = (detail.task_id, Version(detail.version));
    let status = |previous_status| TaskNotification::TaskStatusChanged {
        event_id: event_id.clone(),
        task_id,
        version,
        payload: StatusPayload {
            previous_status,
            status: detail.status,
        },
    };
    let event = match detail.status {
        TaskStatus::Pending | TaskStatus::InProgress => return Lookup::Open,
        TaskStatus::Failed => status(TaskStatus::InProgress),
        TaskStatus::Rejected => status(TaskStatus::Pending),
        TaskStatus::Completed => TaskNotification::TaskCompleted {
            event_id: event_id.clone(),
            task_id,
            version,
            payload: CompletedPayload {
                status: TaskStatus::Completed,
                result_text: detail.result_text.unwrap_or_default(),
                partially_modified: detail.partially_modified,
                started_at: detail.started_at.unwrap_or_default(),
                completed_at: detail.completed_at.unwrap_or_default(),
            },
        },
    };
    // Same rules as a socket event; an incomplete detail waits for a later lookup.
    if event.validate().is_err() {
        return Lookup::Skip;
    }
    Lookup::Final(Box::new(event))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        central::CentralIdentity,
        config,
        sessions::{Identifier, Principal},
        updates::Feed,
    };
    use axum::{
        Json, Router,
        extract::{Path as UrlPath, State},
        http::StatusCode,
        routing::get,
    };
    use serde_json::{Value, json};
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::RwLock;
    use tokio_util::sync::CancellationToken;

    const AGENT: u64 = 12;
    const TEAM: u64 = 7;
    const MINUTE: u64 = 60_000;

    #[derive(Clone, Default)]
    struct Central {
        answers: Arc<std::sync::Mutex<HashMap<u64, (StatusCode, Value)>>>,
        calls: Arc<AtomicUsize>,
    }
    impl Central {
        fn answer(&self, task: u64, code: StatusCode, body: Value) {
            self.answers.lock().unwrap().insert(task, (code, body));
        }
        fn task(&self, task: u64, status: &str, version: u64) {
            self.answer(
                task,
                StatusCode::OK,
                json!({"success":true,"data":detail(task, status, version)}),
            );
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    fn detail(task: u64, status: &str, version: u64) -> Value {
        let done = status == "COMPLETED";
        json!({
            "taskId": task, "teamId": TEAM, "teamName": "team", "taskSummary": "summary",
            "requesterAgent": {"agentId": AGENT, "name": "requester"},
            "receiverAgent": {"agentId": 99, "name": "receiver"},
            "prompt": "prompt", "status": status, "version": version,
            "resultText": if done { json!("완료했습니다") } else { Value::Null },
            "failureReason": if status == "FAILED" { json!("실패") } else { Value::Null },
            "rejectionReason": if status == "REJECTED" { json!("거절") } else { Value::Null },
            "partiallyModified": false, "taskFiles": [], "events": [],
            "createdAt": "2026-11-10T09:00:00",
            "startedAt": if status == "PENDING" || status == "REJECTED" { Value::Null } else { json!("2026-11-10T09:01:00.123456") },
            "completedAt": if done { json!("2026-11-10T09:10:00.654321") } else { Value::Null },
            "lastActivityAt": "2026-11-10T09:10:00"
        })
    }

    async fn task_route(
        State(central): State<Central>,
        UrlPath(task): UrlPath<u64>,
    ) -> (StatusCode, Json<Value>) {
        central.calls.fetch_add(1, Ordering::SeqCst);
        let answer = central.answers.lock().unwrap().get(&task).cloned();
        let (code, body) = answer.unwrap_or((
            StatusCode::NOT_FOUND,
            json!({"success":false,"code":"RESOURCE_NOT_FOUND"}),
        ));
        (code, Json(body))
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        config: PathBuf,
        origin: String,
        central: Central,
        state: CentralState,
        store: Notifications,
        recovery: Recovery,
    }

    async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        config::init(&config).unwrap();
        let central = Central::default();
        let app = Router::new()
            .route("/api/v1/tasks/{task}", get(task_route))
            .with_state(central.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        connect(&state, AGENT).await;
        let store = Notifications::open(&config).unwrap();
        let recovery = Recovery::new(
            TaskClient::new(&origin, "synthetic-token").unwrap(),
            state.clone(),
            store.clone(),
            &origin,
            Arc::new(Notify::new()),
        )
        .unwrap();
        Fixture {
            _dir: dir,
            origin: endpoint_url(&origin).unwrap().to_string(),
            config,
            central,
            state,
            store,
            recovery,
        }
    }

    async fn connect(state: &CentralState, agent: u64) {
        let session = state.begin_session().await;
        assert!(session.set_status(ConnectionStatus::Connected).await);
        assert!(
            session
                .set_identity(CentralIdentity {
                    connection_id: format!("c{agent}"),
                    agent_id: agent,
                    user_id: Some(3),
                    team_id: Some(TEAM),
                })
                .await
        );
    }

    impl Fixture {
        /// A READY round's pass.
        async fn round(&self) -> usize {
            let since = Instant::now();
            self.recovery
                .pass(&self.config, Scope::Round { since })
                .await
                .stored
        }
        /// The pass right before the overdue scan at `now_ms`.
        async fn due(&self, minutes: u64, now_ms: u64) -> Pass {
            self.recovery
                .pass(&self.config, Scope::Due { minutes, now_ms })
                .await
        }
        fn overdue(&self) -> Vec<u64> {
            sent_tasks::events_config(&self.config, &self.origin, AGENT, TEAM)
                .unwrap()
                .into_iter()
                .map(|e| e.task_id)
                .collect()
        }
        fn record(&self, task: u64, created_at_ms: u64) {
            sent_tasks::record(&self.config, &self.origin, AGENT, TEAM, task, created_at_ms)
                .unwrap();
        }
        fn events(&self) -> Vec<TaskNotification> {
            Notifications::events_config(&self.config, &self.origin, AGENT).unwrap()
        }
        /// The local update feed as `get_updates` and `devlink updates` read it.
        fn feed(&self) -> Vec<Value> {
            let actor = Principal {
                user_id: Identifier::parse("3").unwrap(),
                team_id: Identifier::parse(TEAM.to_string()).unwrap(),
                agent_id: Some(Identifier::parse(AGENT.to_string()).unwrap()),
                tool_type: None,
            };
            let page = Feed::load(&self.config, &self.origin, &actor)
                .unwrap()
                .page(None, 50);
            page["updates"].as_array().unwrap().clone()
        }
        fn notifications_file(&self) -> Option<Vec<u8>> {
            std::fs::read(Notifications::path(&self.config)).ok()
        }
    }

    fn socket_completed(task: u64, version: u64) -> TaskNotification {
        // Central's own event: another eventId and full timestamp precision.
        serde_json::from_value(json!({"type":"TASK_COMPLETED","event_id":uuid::Uuid::new_v4().to_string(),
            "task_id":task,"version":version,"payload":{"status":"COMPLETED","result_text":"완료했습니다",
            "partially_modified":false,"started_at":"2026-11-10T09:01:00.123456789","completed_at":"2026-11-10T09:10:00.654321987"}}))
        .unwrap()
    }

    #[tokio::test]
    async fn completion_missed_offline_is_stored_once_and_a_late_socket_event_is_ignored() {
        let f = fixture().await;
        f.record(10, 0);
        f.central.task(10, "COMPLETED", 3);

        assert_eq!(f.round().await, 1);
        let feed = f.feed();
        assert_eq!(feed.len(), 1);
        assert_eq!(feed[0]["kind"], "TASK_COMPLETED");
        assert_eq!(feed[0]["direction"], "sent");
        assert_eq!(feed[0]["taskId"], "10");
        assert_eq!(feed[0]["taskVersion"], "3");
        assert_eq!(feed[0]["status"], "COMPLETED");
        let list = Notifications::list_config(&f.config, &f.origin, AGENT, 20).unwrap();
        assert_eq!(list[0].summary.as_deref(), Some("완료했습니다"));

        // The next READY has nothing left to look up.
        assert_eq!(f.round().await, 0);
        assert_eq!(f.central.calls(), 1);

        // Central's retry reaches the reconnected socket late: a duplicate, not a rejection.
        let before = f.notifications_file();
        f.store
            .record(f.origin.clone(), AGENT, socket_completed(10, 3))
            .await
            .unwrap();
        // Even with a higher version, a second final status of the task is the same event.
        f.store
            .record(f.origin.clone(), AGENT, socket_completed(10, 4))
            .await
            .unwrap();
        assert_eq!(f.notifications_file(), before);
        assert_eq!(f.store.rejected(), 0);
        assert_eq!(f.feed().len(), 1);
    }

    #[tokio::test]
    async fn a_socket_event_stored_first_is_never_looked_up_or_duplicated() {
        let f = fixture().await;
        f.record(10, 0);
        f.central.task(10, "COMPLETED", 3);
        f.store
            .record(f.origin.clone(), AGENT, socket_completed(10, 3))
            .await
            .unwrap();
        assert_eq!(f.round().await, 0);
        assert_eq!(f.central.calls(), 0);
        // A lookup that raced the socket event is dropped by the store.
        let Lookup::Final(event) = decode(
            Ok(serde_json::from_value(detail(10, "COMPLETED", 3)).unwrap()),
            &f.origin,
            AGENT,
            sent_tasks::Unfinished {
                task_id: 10,
                team_id: TEAM,
                created_at_ms: 0,
            },
        ) else {
            panic!("final lookup");
        };
        f.store
            .record(f.origin.clone(), AGENT, *event)
            .await
            .unwrap();
        assert_eq!(f.events().len(), 1);
        assert_eq!(f.store.rejected(), 0);
    }

    #[tokio::test]
    async fn failed_and_rejected_are_stored_as_status_changes_and_open_tasks_wait() {
        let f = fixture().await;
        for task in [11, 12, 13, 14] {
            f.record(task, 0);
        }
        f.central.task(11, "FAILED", 2);
        f.central.task(12, "REJECTED", 1);
        f.central.task(13, "IN_PROGRESS", 1);
        f.central.task(14, "PENDING", 0);

        assert_eq!(f.round().await, 2);
        let mut events = f.events();
        events.sort_by_key(TaskNotification::task_id);
        let [failed, rejected] = &events[..] else {
            panic!("two final statuses: {events:?}");
        };
        let TaskNotification::TaskStatusChanged {
            task_id: 11,
            version: Version(2),
            payload,
            ..
        } = failed
        else {
            panic!("failed status change: {failed:?}");
        };
        assert_eq!(
            (payload.previous_status, payload.status),
            (TaskStatus::InProgress, TaskStatus::Failed)
        );
        let TaskNotification::TaskStatusChanged {
            task_id: 12,
            version: Version(1),
            payload,
            ..
        } = rejected
        else {
            panic!("rejected status change: {rejected:?}");
        };
        assert_eq!(
            (payload.previous_status, payload.status),
            (TaskStatus::Pending, TaskStatus::Rejected)
        );
        let kinds: Vec<_> = f.feed().iter().map(|u| u["kind"].clone()).collect();
        assert_eq!(
            kinds,
            [json!("TASK_STATUS_CHANGED"), json!("TASK_STATUS_CHANGED")]
        );

        // Open tasks are asked again on the next READY; finished ones are not.
        f.central.task(13, "COMPLETED", 2);
        assert_eq!(f.round().await, 1);
        assert_eq!(f.central.calls(), 4 + 2);
    }

    #[tokio::test]
    async fn failed_unauthorized_or_deleted_lookups_change_nothing() {
        let f = fixture().await;
        for task in 20..=26 {
            f.record(task, 0);
        }
        // 20 is deleted (default 404). Every other answer is wrong in its own way.
        f.central.answer(
            21,
            StatusCode::OK,
            json!({"success":true,"data":{"taskId":21}}),
        );
        let mut foreign = detail(22, "COMPLETED", 3);
        foreign["requesterAgent"]["agentId"] = json!(AGENT + 1);
        f.central
            .answer(22, StatusCode::OK, json!({"success":true,"data":foreign}));
        let mut other_team = detail(23, "COMPLETED", 3);
        other_team["teamId"] = json!(TEAM + 1);
        f.central.answer(
            23,
            StatusCode::OK,
            json!({"success":true,"data":other_team}),
        );
        let mut blank = detail(24, "COMPLETED", 3);
        blank["resultText"] = json!("  ");
        f.central
            .answer(24, StatusCode::OK, json!({"success":true,"data":blank}));
        f.central.answer(
            25,
            StatusCode::OK,
            json!({"success":true,"data":detail(99, "FAILED", 2)}),
        );
        f.central.answer(
            26,
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"success":false,"code":"INTERNAL_SERVER_ERROR"}),
        );
        assert_eq!(f.round().await, 0);
        assert_eq!(f.notifications_file(), None);
        assert!(f.feed().is_empty());

        // 401 and 403 end the pass after one call: the credential would be refused again.
        for code in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            for task in 20..=26 {
                f.central
                    .answer(task, code, json!({"success":false,"code":"UNAUTHORIZED"}));
            }
            let before = f.central.calls();
            assert_eq!(f.round().await, 0);
            assert_eq!(f.central.calls(), before + 1);
        }
        assert_eq!(f.notifications_file(), None);

        // Disconnected: nothing is asked at all. The next READY tries again.
        f.state.begin_session().await;
        for task in 20..=26 {
            f.central.task(task, "COMPLETED", 3);
        }
        let before = f.central.calls();
        assert_eq!(f.round().await, 0);
        assert_eq!(f.central.calls(), before);
        connect(&f.state, AGENT).await;
        assert_eq!(f.round().await, 7);
    }

    #[tokio::test]
    async fn an_unreachable_central_ends_the_pass_without_changes() {
        let f = fixture().await;
        f.record(30, 0);
        f.record(31, 0);
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unreachable = format!("ws://{}", closed.local_addr().unwrap());
        drop(closed);
        let recovery = Recovery {
            client: TaskClient::new(&unreachable, "synthetic-token").unwrap(),
            ..f.recovery.clone()
        };
        assert_eq!(
            recovery
                .pass(
                    &f.config,
                    Scope::Round {
                        since: Instant::now()
                    }
                )
                .await
                .stored,
            0
        );
        assert_eq!(recovery.attempted.lock().await.len(), 1);
        assert_eq!(f.notifications_file(), None);
    }

    #[tokio::test]
    async fn an_unavailable_central_keeps_the_advisory_for_tasks_it_could_not_confirm() {
        let f = fixture().await;
        for task in [40, 41, 42, 43] {
            f.record(task, 0);
        }
        f.central.task(40, "COMPLETED", 3);
        f.central.task(41, "IN_PROGRESS", 1);
        f.central.answer(
            42,
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"success":false,"code":"UNAVAILABLE"}),
        );
        f.central.task(43, "REJECTED", 1);
        // Before the threshold nothing is due, so nothing is asked.
        assert_eq!(f.due(30, 29 * MINUTE).await.stored, 0);
        assert_eq!(f.central.calls(), 0);
        // 40 is confirmed. The 503 for 42 ends the pass, so 43 is not asked this time.
        let now = 30 * MINUTE;
        assert_eq!(f.due(30, now).await.stored, 1);
        assert_eq!(f.central.calls(), 3);
        assert_eq!(sent_tasks::scan(&f.config, 30, now).unwrap(), 3);
        let overdue: Vec<_> = sent_tasks::events_config(&f.config, &f.origin, AGENT, TEAM)
            .unwrap()
            .into_iter()
            .map(|e| e.task_id)
            .collect();
        assert_eq!(overdue, [41, 42, 43]);
        // The next READY still finds the final status of 43; the advisory stays as it was.
        assert_eq!(f.round().await, 1);
        let kinds: Vec<_> = f.feed().iter().map(|u| u["kind"].clone()).collect();
        assert_eq!(
            kinds,
            [
                json!("TASK_COMPLETED"),
                json!("TASK_STATUS_CHANGED"),
                json!("TASK_OVERDUE"),
                json!("TASK_OVERDUE"),
                json!("TASK_OVERDUE")
            ]
        );
    }

    #[tokio::test]
    async fn the_overdue_check_asks_first_and_skips_confirmed_tasks() {
        let f = fixture().await;
        for task in [40, 41] {
            f.record(task, 0);
        }
        f.central.task(40, "COMPLETED", 3);
        f.central.task(41, "IN_PROGRESS", 1);
        let now = 30 * MINUTE;
        assert_eq!(f.due(30, now).await.stored, 1);
        assert_eq!(f.central.calls(), 2);
        assert_eq!(sent_tasks::scan(&f.config, 30, now).unwrap(), 1);
        let overdue: Vec<_> = sent_tasks::events_config(&f.config, &f.origin, AGENT, TEAM)
            .unwrap()
            .into_iter()
            .map(|e| e.task_id)
            .collect();
        assert_eq!(overdue, [41]);
        let kinds: Vec<_> = f.feed().iter().map(|u| u["kind"].clone()).collect();
        assert_eq!(kinds, [json!("TASK_COMPLETED"), json!("TASK_OVERDUE")]);
        // Decided tasks are not due again.
        assert_eq!(f.due(30, 90 * MINUTE).await.stored, 0);
        assert_eq!(f.central.calls(), 2);
    }

    #[tokio::test]
    async fn a_confirmed_final_status_settles_even_when_it_cannot_be_stored() {
        let f = fixture().await;
        f.record(50, 0);
        f.central.task(50, "COMPLETED", 3);
        f.store.stop_accepting();
        let now = 30 * MINUTE;
        assert_eq!(f.due(30, now).await.stored, 0);
        assert_eq!(sent_tasks::scan(&f.config, 30, now).unwrap(), 0);
        assert!(f.events().is_empty());
    }

    #[tokio::test]
    async fn passes_are_capped_and_reach_every_task_in_turn() {
        let f = fixture().await;
        let total = MAX_PER_PASS as u64 + 8;
        for task in 1..=total {
            f.record(task, task);
            f.central.task(task, "IN_PROGRESS", 1);
        }
        f.round().await;
        assert_eq!(f.central.calls(), MAX_PER_PASS);
        // The oldest eight were not reached; they go first now.
        for task in 1..=8 {
            f.central.task(task, "COMPLETED", 2);
        }
        assert_eq!(f.round().await, 8);
    }

    #[tokio::test]
    async fn tasks_past_the_cap_are_not_decided_overdue_before_their_lookup() {
        let f = fixture().await;
        let total = MAX_PER_PASS as u64 + 1;
        for task in 1..=total {
            f.record(task, task);
            f.central.task(task, "COMPLETED", 3);
        }
        let now = 31 * MINUTE;
        let pass = f.due(30, now).await;
        assert_eq!(pass.stored, MAX_PER_PASS);
        // The oldest task was not asked: it stays undecided, not overdue.
        assert_eq!(
            pass.held,
            [(f.origin.clone(), AGENT, 1)].into_iter().collect()
        );
        assert_eq!(
            sent_tasks::scan_holding(&f.config, 30, now, &pass.held).unwrap(),
            0
        );
        // The next minute asks it first and finds it completed.
        let pass = f.due(30, now + MINUTE).await;
        assert_eq!((pass.stored, pass.held.len()), (1, 0));
        assert_eq!(
            sent_tasks::scan_holding(&f.config, 30, now + MINUTE, &pass.held).unwrap(),
            0
        );
        assert!(f.overdue().is_empty());
        assert_eq!(f.events().len(), total as usize);
        assert_eq!(f.central.calls(), total as usize);
    }

    #[tokio::test]
    async fn a_detail_larger_than_the_response_limit_still_gives_its_final_status() {
        let f = fixture().await;
        f.record(80, 0);
        // File diffs and event history the recovery does not need, well past 256 KiB.
        let mut large = detail(80, "COMPLETED", 3);
        large["taskFiles"] = json!([{"relativePath": "a.txt", "changeType": "MODIFIED",
            "diffText": "+\"줄\"\n".repeat(64 * 1024)}]);
        large["events"] = json!(
            (0..2000)
                .map(|id| json!({"eventId": id, "eventType": "STATUS_CHANGED",
                    "payload": {"note": "x".repeat(64)}}))
                .collect::<Vec<_>>()
        );
        let body = json!({"success": true, "data": large});
        assert!(serde_json::to_vec(&body).unwrap().len() > 512 * 1024);
        f.central.answer(80, StatusCode::OK, body);
        let now = 30 * MINUTE;
        assert_eq!(f.due(30, now).await.stored, 1);
        assert_eq!(sent_tasks::scan(&f.config, 30, now).unwrap(), 0);
        assert!(f.overdue().is_empty());
        let list = Notifications::list_config(&f.config, &f.origin, AGENT, 20).unwrap();
        assert_eq!(list[0].summary.as_deref(), Some("완료했습니다"));
    }

    #[tokio::test]
    async fn a_ready_round_continues_past_the_cap_with_the_overdue_check_off() {
        let f = fixture().await;
        let ledger = sent_tasks::Ledger::new(&f.config, &f.origin)
            .unwrap()
            .without_overdue();
        let total = MAX_PER_PASS as u64 + 8;
        for task in 1..=total {
            ledger.record_now(AGENT, TEAM, task, task).unwrap();
            f.central.task(task, "COMPLETED", 3);
        }
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(sent_tasks::run_every(
            f.config.clone(),
            0,
            Some(f.recovery.clone()),
            cancel.clone(),
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_millis(50),
        ));
        // One READY only: the round goes on without a reconnect.
        f.recovery.wake().notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while f.events().len() < total as usize {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // The round ends once every task was asked.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
        worker.await.unwrap();
        assert_eq!(f.central.calls(), total as usize);
    }

    #[tokio::test]
    async fn a_round_asks_each_task_once_and_then_ends() {
        let f = fixture().await;
        let total = MAX_PER_PASS as u64 + 8;
        for task in 1..=total {
            f.record(task, task);
        }
        // Every lookup is a 404: nothing settles, but the round still ends.
        let since = Instant::now();
        let first = f.recovery.pass(&f.config, Scope::Round { since }).await;
        assert!(first.more);
        let second = f.recovery.pass(&f.config, Scope::Round { since }).await;
        assert!(!second.more);
        let third = f.recovery.pass(&f.config, Scope::Round { since }).await;
        assert!(!third.more);
        assert_eq!(f.central.calls(), total as usize);
    }

    #[tokio::test]
    async fn frequent_ready_signals_never_postpone_the_overdue_check() {
        let f = fixture().await;
        f.record(90, 0);
        f.central.task(90, "IN_PROGRESS", 1);
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(sent_tasks::run_every(
            f.config.clone(),
            30,
            Some(f.recovery.clone()),
            cancel.clone(),
            std::time::Duration::from_millis(300),
            std::time::Duration::from_millis(50),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while f.overdue().is_empty() {
                // A READY far more often than the check interval.
                f.recovery.wake().notify_one();
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        worker.await.unwrap();
        assert_eq!(f.overdue(), [90]);
    }

    #[tokio::test]
    async fn another_agent_or_team_is_never_looked_up() {
        let f = fixture().await;
        sent_tasks::record(&f.config, &f.origin, AGENT + 1, TEAM, 60, 0).unwrap();
        sent_tasks::record(&f.config, &f.origin, AGENT, TEAM + 1, 61, 0).unwrap();
        sent_tasks::record(&f.config, "ws://127.0.0.1:9", AGENT, TEAM, 62, 0).unwrap();
        assert_eq!(f.round().await, 0);
        assert_eq!(f.central.calls(), 0);
    }

    #[tokio::test]
    async fn ready_wakes_the_worker_even_with_the_overdue_check_off() {
        let f = fixture().await;
        let ledger = sent_tasks::Ledger::new(&f.config, &f.origin)
            .unwrap()
            .without_overdue();
        ledger.record_now(AGENT, TEAM, 70, 0).unwrap();
        f.central.task(70, "COMPLETED", 3);
        // Recorded while the check was off: enabling it later never flags the task.
        assert_eq!(sent_tasks::scan(&f.config, 1, u64::MAX).unwrap(), 0);
        let cancel = CancellationToken::new();
        let worker = tokio::spawn(sent_tasks::run(
            f.config.clone(),
            0,
            Some(f.recovery.clone()),
            cancel.clone(),
        ));
        f.recovery.wake().notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while f.events().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        worker.await.unwrap();
        assert_eq!(f.feed().len(), 1);
    }
}
