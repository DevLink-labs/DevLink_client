use devlink_broker::{
    agent_profile,
    config::{self, Config},
    inbox::Inbox,
    local_updates::{Start, UpdatesError, page},
    notifications::{Notifications, TaskNotification},
    private_file,
    protocol::{TaskCreatedMessage, TaskCreatedPayload, TaskMessageType},
    task_messages::{TaskMessage, TaskMessages},
    teams, update_hook, user_auth,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf};

const ORIGIN: &str = "ws://127.0.0.1:9";
const PROMPT: &str = "never-copy-prompt-into-updates";
const RESULT: &str = "never-copy-result-into-updates";

struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    project: PathBuf,
}
impl Fixture {
    /// Bound Agent 12 of user 3 in team 1.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        config::init(&path).unwrap();
        config::set(&path, "serverUrl", ORIGIN).unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = project.canonicalize().unwrap();
        let fixture = Self { dir, path, project };
        fixture.select_team(1);
        fixture.bind();
        fixture
    }
    fn origin(&self) -> String {
        user_auth::origin(&Config::load(&self.path).unwrap())
            .unwrap()
            .to_string()
    }
    fn select_team(&self, team: u64) {
        private_file::write(
            &teams::selection_path(&self.path),
            &json!({"schemaVersion":1,"origin":self.origin(),"userId":3,"teamId":team}),
            16384,
        )
        .unwrap();
    }
    fn bind(&self) {
        let profile = json!({"schemaVersion":1,"origin":self.origin(),"userId":3,"teamId":1,
            "projectRoot":self.project.to_str().unwrap(),"state":{"kind":"ready","agent":{"agentId":12,"teamId":1,
                "ownerUserId":3,"name":"fixture","roleDescription":null,"toolType":"CLAUDE_CODE",
                "projectName":"project","status":"OFFLINE","lastSeenAt":null,"createdAt":"synthetic"},
                "token":"synthetic-token"}});
        private_file::write(&agent_profile::path(&self.path), &profile, 16384).unwrap();
    }
    fn page(&self, start: Start<'_>, limit: usize) -> Value {
        let value = page(&self.path, start, limit).unwrap();
        let text = value.to_string();
        assert!(!text.contains(PROMPT) && !text.contains(RESULT), "{text}");
        value
    }
    fn after(&self, cursor: Option<&str>) -> Value {
        self.page(Start::After(cursor), 50)
    }
    async fn receive(&self, task_id: u64, team_id: u64, receiver: u64) {
        let message = TaskCreatedMessage {
            event_id: format!("created-{task_id}-{receiver}"),
            payload: TaskCreatedPayload {
                task_id,
                team_id,
                requester_agent_id: 11,
                receiver_agent_id: receiver,
                prompt: PROMPT.into(),
                created_at: "2026-11-09T10:00:00".into(),
            },
        };
        Inbox::open(&self.path)
            .unwrap()
            .record(ORIGIN.into(), receiver, message)
            .await
            .unwrap();
    }
    async fn notify(&self, requester: u64, task_id: u64, version: u64, status: &str) {
        let event_id = uuid::Uuid::new_v4().to_string();
        let value = if status == "COMPLETED" {
            json!({"type":"TASK_COMPLETED","event_id":event_id,"task_id":task_id,"version":version,
                "payload":{"status":status,"result_text":RESULT,"partially_modified":false,
                    "started_at":"2026-11-09T10:00:00","completed_at":"2026-11-09T10:01:00"}})
        } else {
            json!({"type":"TASK_STATUS_CHANGED","event_id":event_id,"task_id":task_id,"version":version,
                "payload":{"previous_status":"PENDING","status":status}})
        };
        let event: TaskNotification = serde_json::from_value(value).unwrap();
        Notifications::open(&self.path)
            .unwrap()
            .record(ORIGIN.into(), requester, event)
            .await
            .unwrap();
    }
    async fn messages(&self, agent: u64, items: &[(u64, u64, TaskMessageType)], cursor: &str) {
        let items = items
            .iter()
            .map(|(id, task, kind)| TaskMessage {
                event_id: id.to_string(),
                task_id: *task,
                sequence: id.to_string(),
                event_type: *kind,
                created_at: "2026-11-09T10:00:00Z".into(),
            })
            .collect();
        TaskMessages::open(&self.path)
            .unwrap()
            .apply(ORIGIN, agent, items, cursor.into())
            .await
            .unwrap();
    }
    /// Every file under the fixture with its bytes, to prove reads write nothing.
    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &std::path::Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(self.dir.path(), &mut out);
        out
    }
}

fn kinds(page: &Value) -> Vec<(String, String)> {
    page["updates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["kind"].as_str().unwrap().to_owned(),
                u["taskId"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[tokio::test]
async fn page_lists_only_the_bound_agent_with_scope_and_resumes_from_cursor() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;
    f.receive(102, 2, 12).await; // another team
    f.receive(103, 1, 13).await; // another Agent
    f.notify(12, 201, 1, "IN_PROGRESS").await;
    f.notify(12, 201, 2, "COMPLETED").await;
    f.notify(13, 202, 1, "IN_PROGRESS").await; // another requester
    f.messages(
        12,
        &[
            (1, 300, TaskMessageType::Question),
            (2, 301, TaskMessageType::Answer),
        ],
        "c1",
    )
    .await;
    f.messages(13, &[(9, 302, TaskMessageType::Question)], "c1")
        .await;

    let first = f.after(None);
    assert_eq!(first["schemaVersion"], 1);
    assert_eq!(
        first["scope"],
        json!({"userId":"3","teamId":"1","agentId":"12","toolType":"claude_code",
            "projectPath":f.project.to_str().unwrap()})
    );
    assert_eq!(
        kinds(&first),
        [
            ("TASK_CREATED", "101"),
            ("TASK_STATUS_CHANGED", "201"),
            ("TASK_COMPLETED", "201"),
            ("TASK_QUESTION_CREATED", "300"),
            ("TASK_ANSWER_CREATED", "301"),
        ]
        .map(|(k, t)| (k.to_owned(), t.to_owned()))
    );
    let created = &first["updates"][0];
    assert_eq!(created["eventId"], "created-101-12");
    assert_eq!(created["requesterAgentId"], "11");
    assert_eq!(first["updates"][2]["taskVersion"], "2");
    assert_eq!(first["hasMore"], false);
    assert_eq!(first["cursorReset"], false);
    assert_eq!(first["coverage"], "local");

    let cursor = first["nextCursor"].as_str().unwrap().to_owned();
    let empty = f.after(Some(&cursor));
    assert!(kinds(&empty).is_empty());
    assert_eq!(empty["nextCursor"], cursor.as_str());

    f.receive(104, 1, 12).await;
    let next = f.after(Some(&cursor));
    assert_eq!(kinds(&next), [("TASK_CREATED".into(), "104".into())]);
    assert_eq!(next["cursorReset"], false);
}

#[tokio::test]
async fn limit_pages_with_has_more() {
    let f = Fixture::new();
    for task in 1..=3 {
        f.receive(task, 1, 12).await;
    }
    let first = f.page(Start::After(None), 2);
    assert_eq!(first["updates"].as_array().unwrap().len(), 2);
    assert_eq!(first["hasMore"], true);
    let rest = f.page(Start::After(first["nextCursor"].as_str()), 2);
    assert_eq!(kinds(&rest), [("TASK_CREATED".into(), "3".into())]);
    assert_eq!(rest["hasMore"], false);
}

#[tokio::test]
async fn latest_skips_retained_history() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;
    f.notify(12, 201, 1, "IN_PROGRESS").await;
    let latest = f.page(Start::Latest, 1);
    assert!(kinds(&latest).is_empty());
    assert_eq!(latest["hasMore"], false);
    assert_eq!(latest["scope"]["agentId"], "12");

    f.notify(12, 201, 2, "COMPLETED").await;
    let next = f.after(latest["nextCursor"].as_str());
    assert_eq!(kinds(&next), [("TASK_COMPLETED".into(), "201".into())]);
}

#[tokio::test]
async fn reads_write_nothing_and_leave_the_hook_cursor_untouched() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;
    f.messages(12, &[(1, 300, TaskMessageType::Question)], "c1")
        .await;
    let before = f.snapshot();
    let first = f.after(None);
    f.after(first["nextCursor"].as_str());
    f.page(Start::Latest, 1);
    assert_eq!(f.snapshot(), before);
    // Desktop polling did not consume the Host hook's view.
    let digest = update_hook::digest(&f.path, "codex", &f.project)
        .unwrap()
        .unwrap();
    assert!(
        digest.contains("#101") && digest.contains("#300"),
        "{digest}"
    );
    // Nor does the hook consume the desktop's.
    assert_eq!(kinds(&f.after(None)).len(), 2);
}

#[tokio::test]
async fn cleared_store_restarts_with_cursor_reset() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;
    f.notify(12, 201, 1, "IN_PROGRESS").await;
    let cursor = f.after(None)["nextCursor"].as_str().unwrap().to_owned();

    Inbox::clear_config(&f.path).unwrap();
    f.receive(102, 1, 12).await;
    let page = f.after(Some(&cursor));
    assert_eq!(page["cursorReset"], true);
    assert_eq!(
        kinds(&page),
        [
            ("TASK_CREATED".into(), "102".into()),
            ("TASK_STATUS_CHANGED".into(), "201".into()),
        ]
    );
}

#[tokio::test]
async fn invalid_input_unbound_or_mismatched_profile_is_a_coded_error() {
    let f = Fixture::new();
    for cursor in ["", "v9.0.0", "v3.0.0.0.0.zz", "not-a-cursor"] {
        assert_eq!(
            page(&f.path, Start::After(Some(cursor)), 20),
            Err(UpdatesError::INPUT_INVALID),
            "{cursor}"
        );
    }
    for limit in [0, 51] {
        assert_eq!(
            page(&f.path, Start::After(None), limit),
            Err(UpdatesError::INPUT_INVALID)
        );
    }

    f.select_team(2);
    assert_eq!(
        page(&f.path, Start::After(None), 20),
        Err(UpdatesError::BINDING_MISMATCH)
    );

    let unbound = tempfile::tempdir().unwrap();
    let path = unbound.path().join("config.json");
    config::init(&path).unwrap();
    assert_eq!(
        page(&path, Start::Latest, 20),
        Err(UpdatesError::AGENT_UNBOUND)
    );
    // A missing profile is unbound as well, and reading it creates nothing.
    let missing = unbound.path().join("missing").join("config.json");
    assert_eq!(
        page(&missing, Start::After(None), 20),
        Err(UpdatesError::AGENT_UNBOUND)
    );
    assert!(!missing.parent().unwrap().exists());
}
