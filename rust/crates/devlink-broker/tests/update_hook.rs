use devlink_broker::{
    agent_profile,
    config::{self, Config},
    inbox::Inbox,
    notifications::{Notifications, TaskNotification},
    private_file,
    protocol::{TaskCreatedMessage, TaskCreatedPayload, TaskMessageType},
    task_messages::{TaskMessage, TaskMessages},
    teams,
    update_hook::digest,
    user_auth,
};
use serde_json::json;
use std::path::PathBuf;

const ORIGIN: &str = "ws://127.0.0.1:9";
const PROMPT: &str = "never-copy-prompt-into-hook";
const RESULT: &str = "never-copy-result-into-hook";

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    project: PathBuf,
    outside: PathBuf,
}
impl Fixture {
    /// Bound Agent 12 of user 3 in team 1, with `project` as its project root.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        config::init(&path).unwrap();
        config::set(&path, "serverUrl", ORIGIN).unwrap();
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let project = project.canonicalize().unwrap();
        let fixture = Self {
            _dir: dir,
            path,
            project,
            outside,
        };
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
                "ownerUserId":3,"name":"fixture","roleDescription":null,"toolType":"CODEX",
                "projectName":"project","status":"OFFLINE","lastSeenAt":null,"createdAt":"synthetic"},
                "token":"synthetic-token"}});
        private_file::write(&agent_profile::path(&self.path), &profile, 16384).unwrap();
    }
    fn digest(&self, consumer: &str) -> Option<String> {
        let text = digest(&self.path, consumer, &self.project.join("src")).unwrap();
        if let Some(text) = &text {
            assert!(!text.contains(PROMPT) && !text.contains(RESULT), "{text}");
            assert!(text.contains("not user approval"), "{text}");
        }
        text
    }
    async fn receive(&self, task_id: u64, team_id: u64, receiver: u64) {
        let message = TaskCreatedMessage {
            event_id: uuid::Uuid::new_v4().to_string(),
            payload: TaskCreatedPayload {
                task_id,
                team_id,
                requester_agent_id: 11,
                receiver_agent_id: receiver,
                prompt: PROMPT.into(),
                created_at: "2026-11-04T10:00:00".into(),
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
                    "started_at":"2026-11-04T10:00:00","completed_at":"2026-11-04T10:01:00"}})
        } else {
            json!({"type":"TASK_STATUS_CHANGED","event_id":event_id,"task_id":task_id,"version":version,
                "payload":{"previous_status":if status == "FAILED" {"IN_PROGRESS"} else {"PENDING"},"status":status}})
        };
        let event: TaskNotification = serde_json::from_value(value).unwrap();
        Notifications::open(&self.path)
            .unwrap()
            .record(ORIGIN.into(), requester, event)
            .await
            .unwrap();
    }
    fn cursor_file(&self, consumer: &str) -> PathBuf {
        config::state_directory(&self.path).join(format!("updates-{consumer}.json"))
    }
}

#[tokio::test]
async fn digest_reports_own_updates_once_with_latest_sent_status() {
    let f = Fixture::new();
    assert_eq!(f.digest("codex"), None);
    f.receive(101, 1, 12).await;
    f.receive(102, 2, 12).await; // another team
    f.receive(103, 1, 13).await; // another Agent
    f.notify(12, 201, 1, "IN_PROGRESS").await;
    f.notify(12, 201, 2, "COMPLETED").await;
    f.notify(13, 202, 1, "IN_PROGRESS").await; // another requester
    let text = f.digest("codex").unwrap();
    assert!(
        text.contains("Received 1 new task(s): #101 from agent 11."),
        "{text}"
    );
    assert!(text.contains("#201 COMPLETED (v2)."), "{text}");
    for absent in ["#102", "#103", "#202", "IN_PROGRESS", "cleared"] {
        assert!(!text.contains(absent), "{absent}: {text}");
    }
    assert_eq!(f.digest("codex"), None);

    f.notify(12, 203, 1, "IN_PROGRESS").await;
    let text = f.digest("codex").unwrap();
    assert!(text.contains("#203 IN_PROGRESS (v1)"), "{text}");
    assert!(!text.contains("#101") && !text.contains("#201"), "{text}");
}

#[tokio::test]
async fn each_consumer_keeps_its_own_cursor() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;
    assert!(f.digest("codex").unwrap().contains("#101"));
    assert_eq!(f.digest("codex"), None);
    assert!(f.digest("claude-code").unwrap().contains("#101"));
    assert!(f.cursor_file("codex").exists() && f.cursor_file("claude-code").exists());
}

#[tokio::test]
async fn digest_lists_at_most_ten_tasks_per_category() {
    let f = Fixture::new();
    for task in 1..=12 {
        f.receive(task, 1, 12).await;
    }
    let text = f.digest("codex").unwrap();
    assert!(text.contains("Received 12 new task(s)"), "{text}");
    assert!(text.contains("#10 from agent 11 and 2 more."), "{text}");
    assert!(!text.contains("#11 from"), "{text}");
}

#[tokio::test]
async fn silent_without_binding_outside_project_or_while_locked() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;

    assert_eq!(digest(&f.path, "codex", &f.outside).unwrap(), None);
    let lock = config::lock(&config::state_directory(&f.path).join("updates-codex.lock")).unwrap();
    assert_eq!(f.digest("codex"), None);
    drop(lock);
    // Neither skipped call consumed the update.
    assert!(f.digest("codex").unwrap().contains("#101"));

    let unbound = tempfile::tempdir().unwrap();
    let path = unbound.path().join("config.json");
    config::init(&path).unwrap();
    assert_eq!(digest(&path, "codex", unbound.path()).unwrap(), None);
    assert!(
        !config::state_directory(&path)
            .join("updates-codex.json")
            .exists()
    );
}

#[tokio::test]
async fn cleared_store_or_unreadable_cursor_restarts_with_notice() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;
    f.notify(12, 201, 1, "IN_PROGRESS").await;
    assert!(f.digest("codex").is_some());

    Inbox::clear_config(&f.path).unwrap();
    f.receive(102, 1, 12).await;
    let text = f.digest("codex").unwrap();
    assert!(text.contains("cleared or replaced"), "{text}");
    assert!(
        text.contains("#102") && text.contains("#201") && !text.contains("#101"),
        "{text}"
    );
    assert_eq!(f.digest("codex"), None);

    private_file::write(&f.cursor_file("codex"), &json!({"schemaVersion":9}), 4096).unwrap();
    let text = f.digest("codex").unwrap();
    assert!(
        text.contains("cleared or replaced") && text.contains("#102"),
        "{text}"
    );
    assert_eq!(f.digest("codex"), None);
}

#[tokio::test]
async fn mismatched_selection_or_invalid_consumer_is_an_error() {
    let f = Fixture::new();
    f.receive(101, 1, 12).await;
    for consumer in ["", "../codex", "Codex"] {
        assert!(digest(&f.path, consumer, &f.project).is_err(), "{consumer}");
    }
    f.select_team(2);
    let error = digest(&f.path, "codex", &f.project).unwrap_err();
    assert!(error.to_string().contains("does not match"), "{error}");
    assert!(!f.cursor_file("codex").exists());
}

#[tokio::test]
async fn late_older_status_does_not_regress_digest() {
    let f = Fixture::new();
    f.notify(12, 201, 2, "COMPLETED").await;
    assert!(f.digest("codex").unwrap().contains("#201 COMPLETED (v2)"));
    f.notify(12, 201, 1, "IN_PROGRESS").await;
    // The late event is consumed without reporting an older status.
    assert_eq!(f.digest("codex"), None);

    f.notify(12, 202, 3, "FAILED").await;
    f.notify(12, 202, 2, "IN_PROGRESS").await;
    f.receive(101, 1, 12).await;
    f.notify(12, 201, 1, "IN_PROGRESS").await;
    let text = f.digest("codex").unwrap();
    assert!(
        text.contains("#202 FAILED (v3)") && text.contains("#101"),
        "{text}"
    );
    assert!(
        !text.contains("#201") && !text.contains("IN_PROGRESS"),
        "{text}"
    );
    assert_eq!(f.digest("codex"), None);
}

#[tokio::test]
async fn digest_counts_new_questions_and_answers_per_task_without_text() {
    let f = Fixture::new();
    let message = |id: u64, task: u64, kind: TaskMessageType| TaskMessage {
        event_id: id.to_string(),
        task_id: task,
        sequence: id.to_string(),
        event_type: kind,
        created_at: "2026-11-05T10:00:00Z".into(),
    };
    let store = TaskMessages::open(&f.path).unwrap();
    store
        .apply(
            ORIGIN,
            12,
            vec![
                message(1, 300, TaskMessageType::Question),
                message(2, 301, TaskMessageType::Answer),
                message(3, 300, TaskMessageType::Answer),
                message(4, 300, TaskMessageType::Answer),
            ],
            "c1".into(),
        )
        .await
        .unwrap();
    // Another Agent's arrivals stay out of this digest.
    store
        .apply(
            ORIGIN,
            13,
            vec![message(9, 302, TaskMessageType::Question)],
            "c1".into(),
        )
        .await
        .unwrap();
    let text = f.digest("codex").unwrap();
    assert!(
        text.contains(
            "New questions/answers from the other agent on 2 task(s): \
             #300 (1 question, 2 answers), #301 (1 answer)."
        ),
        "{text}"
    );
    assert!(text.contains("get_task_result"), "{text}");
    assert!(
        !text.contains("#302") && !text.contains("Received"),
        "{text}"
    );
    assert_eq!(f.digest("codex"), None);
    store
        .apply(
            ORIGIN,
            12,
            vec![message(5, 301, TaskMessageType::Question)],
            "c2".into(),
        )
        .await
        .unwrap();
    let text = f.digest("codex").unwrap();
    assert!(text.contains("on 1 task(s): #301 (1 question)."), "{text}");
}
