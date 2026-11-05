//! Requester-side advisory notifications. Central task/result APIs remain authoritative.
use crate::{
    central::endpoint_url,
    config, private_file,
    task_operations::{TaskStatus, Version},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_EVENTS: usize = 1024;
pub const MAX_PENDING: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusPayload {
    pub previous_status: TaskStatus,
    pub status: TaskStatus,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedPayload {
    pub status: TaskStatus,
    pub result_text: String,
    pub partially_modified: bool,
    pub started_at: String,
    pub completed_at: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "SCREAMING_SNAKE_CASE", deny_unknown_fields)]
pub enum TaskNotification {
    TaskStatusChanged {
        event_id: String,
        task_id: u64,
        version: Version,
        payload: StatusPayload,
    },
    TaskCompleted {
        event_id: String,
        task_id: u64,
        version: Version,
        payload: CompletedPayload,
    },
}
impl TaskNotification {
    pub fn recognizes(text: &str) -> bool {
        serde_json::from_str::<serde_json::Value>(text).is_ok_and(|v| {
            matches!(
                v.get("type").and_then(serde_json::Value::as_str),
                Some("TASK_STATUS_CHANGED" | "TASK_COMPLETED")
            )
        })
    }
    pub fn event_id(&self) -> &str {
        match self {
            Self::TaskStatusChanged { event_id, .. } | Self::TaskCompleted { event_id, .. } => {
                event_id
            }
        }
    }
    pub fn task_id(&self) -> u64 {
        match self {
            Self::TaskStatusChanged { task_id, .. } | Self::TaskCompleted { task_id, .. } => {
                *task_id
            }
        }
    }
    pub fn version(&self) -> u64 {
        match self {
            Self::TaskStatusChanged { version, .. } | Self::TaskCompleted { version, .. } => {
                version.0
            }
        }
    }
    pub fn status(&self) -> TaskStatus {
        match self {
            Self::TaskStatusChanged { payload, .. } => payload.status,
            Self::TaskCompleted { payload, .. } => payload.status,
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(self.event_id()).is_ok(),
            "Invalid notification event ID"
        );
        ensure!(
            self.task_id() > 0
                && self.task_id() <= i64::MAX as u64
                && self.version() <= i64::MAX as u64,
            "Invalid notification identifier"
        );
        match self {
            Self::TaskStatusChanged { payload, .. } => ensure!(
                matches!(
                    (payload.previous_status, payload.status),
                    (
                        TaskStatus::Pending,
                        TaskStatus::InProgress | TaskStatus::Rejected
                    ) | (TaskStatus::InProgress, TaskStatus::Failed)
                ),
                "Invalid status notification"
            ),
            Self::TaskCompleted { payload, .. } => ensure!(
                payload.status == TaskStatus::Completed
                    && !payload.result_text.trim().is_empty()
                    && !payload.started_at.trim().is_empty()
                    && !payload.completed_at.trim().is_empty(),
                "Invalid completion notification"
            ),
        }
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    origin: String,
    requester_agent_id: u64,
    event: TaskNotification,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    events: Vec<Stored>,
}
fn read(path: &Path) -> Result<Snapshot> {
    let value: Snapshot = private_file::read(path, MAX_BYTES)?.unwrap_or(Snapshot {
        schema_version: 1,
        events: Vec::new(),
    });
    ensure!(
        value.schema_version == 1 && value.events.len() <= MAX_EVENTS,
        "Invalid notification snapshot"
    );
    let mut seen = std::collections::HashSet::new();
    for r in &value.events {
        ensure!(
            r.requester_agent_id > 0
                && r.requester_agent_id <= i64::MAX as u64
                && endpoint_url(&r.origin)?.as_str() == r.origin,
            "Invalid notification scope"
        );
        r.event.validate()?;
        ensure!(
            seen.insert((&r.origin, r.requester_agent_id, r.event.event_id())),
            "Conflicting notification history"
        );
    }
    Ok(value)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotificationView {
    pub task_id: String,
    pub version: String,
    pub event_id: String,
    pub status: TaskStatus,
    pub summary: Option<String>,
    pub detail_required: bool,
    pub next_action: &'static str,
}

/// One background writer for all connection generations. No socket or central mutation on failure.
#[derive(Clone)]
pub struct Notifications {
    path: PathBuf,
    writer: Arc<tokio::sync::Mutex<()>>,
    stopped: Arc<AtomicBool>,
    rejected: Arc<AtomicU64>,
    pending: Arc<tokio::sync::Semaphore>,
}
impl Notifications {
    pub fn path(config_path: &Path) -> PathBuf {
        config::state_directory(config_path).join("notifications.json")
    }
    pub fn open(config_path: &Path) -> Result<Self> {
        let path = Self::path(config_path);
        let invalid = read(&path).is_err();
        Ok(Self {
            path,
            writer: Arc::new(tokio::sync::Mutex::new(())),
            stopped: Arc::new(AtomicBool::new(false)),
            rejected: Arc::new(AtomicU64::new(u64::from(invalid))),
            pending: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING)),
        })
    }
    pub fn stop_accepting(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    pub fn reject(&self) {
        self.rejected.fetch_add(1, Ordering::Relaxed);
    }
    pub fn rejected(&self) -> u64 {
        self.rejected.load(Ordering::Relaxed)
    }
    /// Bound admission across connection generations; admitted work outlives a socket session.
    pub fn enqueue(&self, origin: String, requester_agent_id: u64, event: TaskNotification) {
        if self.stopped.load(Ordering::Acquire) {
            self.reject();
            return;
        }
        let Ok(permit) = self.pending.clone().try_acquire_owned() else {
            self.reject();
            return;
        };
        let store = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = store.record(origin, requester_agent_id, event).await;
        });
    }
    pub async fn record(
        &self,
        origin: String,
        requester_agent_id: u64,
        event: TaskNotification,
    ) -> Result<()> {
        let permit = self.writer.clone().lock_owned().await;
        let store = self.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ensure!(
                !store.stopped.load(Ordering::Acquire),
                "Notification store stopped"
            );
            // A previous process may still be flushing a blocking worker during restart.
            // Serialize read/modify/replace across processes as well as async writers.
            let _disk_guard = config::lock(&store.path.with_extension("lock"))?;
            event.validate()?;
            ensure!(
                requester_agent_id > 0 && requester_agent_id <= i64::MAX as u64,
                "Invalid requester"
            );
            let origin = endpoint_url(&origin)?.to_string();
            let mut snapshot = read(&store.path)?;
            if let Some(existing) = snapshot.events.iter().find(|r| {
                r.origin == origin
                    && r.requester_agent_id == requester_agent_id
                    && r.event.event_id() == event.event_id()
            }) {
                ensure!(existing.event == event, "Conflicting notification replay");
                return Ok(());
            }
            if let Some(existing) = snapshot.events.iter().find(|r| {
                r.origin == origin
                    && r.requester_agent_id == requester_agent_id
                    && r.event.task_id() == event.task_id()
                    && r.event.version() == event.version()
            }) {
                let mut old = serde_json::to_value(&existing.event)?;
                let mut new = serde_json::to_value(&event)?;
                old.as_object_mut().unwrap().remove("event_id");
                new.as_object_mut().unwrap().remove("event_id");
                ensure!(old == new, "Conflicting notification version");
            }
            // Keep retained event IDs for replay detection, while list() selects the highest version.
            ensure!(snapshot.events.len() < MAX_EVENTS, "Notification retention full; verify central details and clear local notifications while stopped");
            snapshot.events.push(Stored {
                origin,
                requester_agent_id,
                event,
            });
            ensure!(
                !store.stopped.load(Ordering::Acquire),
                "Notification store stopped"
            );
            private_file::write(&store.path, &snapshot, MAX_BYTES)
        })
        .await
        .context("Notification worker failed")?;
        if result.is_err() {
            self.reject();
        }
        result
    }
    /// Caller holds stopped-broker/control locks; this only resets local advisory retention.
    pub fn clear_config(config_path: &Path) -> Result<()> {
        let _disk_guard = config::lock(&Self::path(config_path).with_extension("lock"))?;
        private_file::write(
            &Self::path(config_path),
            &Snapshot {
                schema_version: 1,
                events: Vec::new(),
            },
            MAX_BYTES,
        )
    }
    /// Update-feed source in persisted append order, including superseded versions. Read-only.
    pub(crate) fn events_config(
        config_path: &Path,
        origin: &str,
        agent_id: u64,
    ) -> Result<Vec<TaskNotification>> {
        let origin = endpoint_url(origin)?.to_string();
        Ok(read(&Self::path(config_path))?
            .events
            .into_iter()
            .filter(|r| r.origin == origin && r.requester_agent_id == agent_id)
            .map(|r| r.event)
            .collect())
    }
    /// Retained summaries are advisory. Missing/failed notifications require authorized central lookup.
    pub fn list_config(
        config_path: &Path,
        origin: &str,
        agent_id: u64,
        limit: usize,
    ) -> Result<Vec<NotificationView>> {
        ensure!(
            (1..=MAX_EVENTS).contains(&limit),
            "Invalid notification limit"
        );
        let origin = endpoint_url(origin)?.to_string();
        let mut latest = std::collections::BTreeMap::<u64, &Stored>::new();
        let snapshot = read(&Self::path(config_path))?;
        for r in &snapshot.events {
            if r.origin != origin || r.requester_agent_id != agent_id {
                continue;
            }
            if latest
                .get(&r.event.task_id())
                .is_none_or(|old| r.event.version() > old.event.version())
            {
                latest.insert(r.event.task_id(), r);
            }
        }
        Ok(latest.values().rev().take(limit).map(|r| NotificationView {
            task_id: r.event.task_id().to_string(), version: r.event.version().to_string(),
            event_id: r.event.event_id().to_string(), status: r.event.status(),
            summary: match &r.event { TaskNotification::TaskCompleted { payload, .. } => Some(payload.result_text.clone()), _ => None },
            detail_required: true,
            next_action: "Use the bound MCP session's get_task_result(taskId) or get_tasks(direction=sent) to verify central details, failure/rejection reason and changed files; local notifications may be missing or stale.",
        }).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn event(version: u64, status: &str) -> TaskNotification {
        let value = if status == "COMPLETED" {
            serde_json::json!({"type":"TASK_COMPLETED","event_id":uuid::Uuid::new_v4().to_string(),"task_id":7,"version":version,
                "payload":{"status":status,"result_text":"  완료\n검증 내용  ","partially_modified":false,"started_at":"2026-11-02T10:00:00","completed_at":"2026-11-02T10:01:00"}})
        } else {
            serde_json::json!({"type":"TASK_STATUS_CHANGED","event_id":uuid::Uuid::new_v4().to_string(),"task_id":7,"version":version,
                "payload":{"previous_status":if status=="FAILED" {"IN_PROGRESS"} else {"PENDING"},"status":status}})
        };
        serde_json::from_value(value).unwrap()
    }
    #[tokio::test]
    async fn retained_replay_order_scope_and_precision_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = Notifications::open(&path).unwrap();
        let completed = event(i64::MAX as u64, "COMPLETED");
        store
            .record("ws://localhost:8080".into(), 11, completed.clone())
            .await
            .unwrap();
        store
            .record("ws://localhost:8080/ws/broker".into(), 11, completed)
            .await
            .unwrap();
        store
            .record("ws://localhost:8080".into(), 11, event(1, "IN_PROGRESS"))
            .await
            .unwrap();
        store
            .record("ws://localhost:8080".into(), 12, event(2, "FAILED"))
            .await
            .unwrap();
        store
            .record("ws://localhost:8081".into(), 11, event(2, "FAILED"))
            .await
            .unwrap();
        let reopened = Notifications::open(&path).unwrap();
        assert_eq!(reopened.rejected(), 0);
        let list = Notifications::list_config(&path, "ws://localhost:8080", 11, 20).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].version, i64::MAX.to_string());
        assert_eq!(list[0].status, TaskStatus::Completed);
        assert_eq!(list[0].summary.as_deref(), Some("  완료\n검증 내용  "));
        assert!(list[0].detail_required);
        assert_eq!(read(&Notifications::path(&path)).unwrap().events.len(), 4);
        crate::credentials::verify_private_permissions(&Notifications::path(&path)).unwrap();
    }
    #[tokio::test]
    async fn conflicting_replay_and_version_leave_snapshot_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = Notifications::open(&path).unwrap();
        let completed = event(2, "COMPLETED");
        store
            .record("ws://localhost:8080".into(), 11, completed.clone())
            .await
            .unwrap();
        let before = std::fs::read(Notifications::path(&path)).unwrap();
        let mut conflict = event(2, "FAILED");
        assert!(
            store
                .record("ws://localhost:8080".into(), 11, conflict.clone())
                .await
                .is_err()
        );
        if let TaskNotification::TaskStatusChanged { event_id, .. } = &mut conflict {
            *event_id = completed.event_id().into();
        }
        assert!(
            store
                .record("ws://localhost:8080".into(), 11, conflict)
                .await
                .is_err()
        );
        assert_eq!(before, std::fs::read(Notifications::path(&path)).unwrap());
        assert_eq!(store.rejected(), 2);
    }
    #[tokio::test]
    async fn corrupt_snapshot_is_never_overwritten_or_fatal_to_store_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        private_file::write(
            &Notifications::path(&path),
            &serde_json::json!({"secret":"internal"}),
            MAX_BYTES,
        )
        .unwrap();
        let before = std::fs::read(Notifications::path(&path)).unwrap();
        let store = Notifications::open(&path).unwrap();
        assert_eq!(store.rejected(), 1);
        let error = store
            .record("ws://localhost:8080".into(), 11, event(1, "IN_PROGRESS"))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("internal"));
        assert_eq!(before, std::fs::read(Notifications::path(&path)).unwrap());
    }
    #[test]
    fn provider_payloads_are_strict_and_validated() {
        for status in ["IN_PROGRESS", "FAILED", "REJECTED", "COMPLETED"] {
            event(1, status).validate().unwrap();
        }
        let mut value = serde_json::to_value(event(2, "COMPLETED")).unwrap();
        value["payload"]["result_text"] = serde_json::json!(null);
        assert!(serde_json::from_value::<TaskNotification>(value).is_err());
        let mut value = serde_json::to_value(event(2, "FAILED")).unwrap();
        value["version"] = serde_json::json!(u64::MAX);
        assert!(serde_json::from_value::<TaskNotification>(value).is_err());
        let mut invalid = event(2, "FAILED");
        if let TaskNotification::TaskStatusChanged { event_id, .. } = &mut invalid {
            *event_id = " ".into();
        }
        assert!(invalid.validate().is_err());
    }

    #[tokio::test]
    async fn capacity_and_cross_process_lock_never_erase_high_water_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let origin = endpoint_url("ws://localhost:8080").unwrap();
        let snapshot = Snapshot {
            schema_version: 1,
            events: (0..MAX_EVENTS)
                .map(|v| Stored {
                    origin: origin.clone(),
                    requester_agent_id: 11,
                    event: event(v as u64, "IN_PROGRESS"),
                })
                .collect(),
        };
        private_file::write(&Notifications::path(&path), &snapshot, MAX_BYTES).unwrap();
        let store = Notifications::open(&path).unwrap();
        assert!(
            store
                .record(origin.clone(), 11, event(MAX_EVENTS as u64, "COMPLETED"))
                .await
                .is_err()
        );
        let before = std::fs::read(Notifications::path(&path)).unwrap();
        assert_eq!(
            Notifications::list_config(&path, &origin, 11, 20).unwrap()[0].version,
            (MAX_EVENTS - 1).to_string()
        );
        let lock = config::lock(&Notifications::path(&path).with_extension("lock")).unwrap();
        assert!(
            store
                .record(origin.clone(), 11, event(0, "IN_PROGRESS"))
                .await
                .is_err()
        );
        assert!(Notifications::clear_config(&path).is_err());
        assert_eq!(before, std::fs::read(Notifications::path(&path)).unwrap());
        drop(lock);
        Notifications::clear_config(&path).unwrap();
        assert!(
            Notifications::list_config(&path, &origin, 11, 20)
                .unwrap()
                .is_empty()
        );
        store.stop_accepting();
        assert!(
            store
                .record(origin, 11, event(1, "IN_PROGRESS"))
                .await
                .is_err()
        );
    }
}
