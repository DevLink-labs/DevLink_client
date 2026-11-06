//! Requester-side "no result yet" advisory (MVP RCV-005).
//!
//! The broker records each task this Agent created. When a recorded task has had no
//! COMPLETED, FAILED or REJECTED notification for `taskOverdueMinutes`, one
//! `TASK_OVERDUE` event is appended for the local update feed. Central task state is
//! never changed, nothing is cancelled or retried, and a missed central notification can
//! make a finished task look overdue: `get_task_result` stays authoritative.
use crate::{
    central::endpoint_url, config, notifications::Notifications, private_file,
    task_operations::TaskStatus,
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio_util::sync::CancellationToken;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_TASKS: usize = 1024;
const MAX_EVENTS: usize = 1024;
/// How often the broker checks recorded tasks against the threshold.
const SCAN_INTERVAL: Duration = Duration::from_secs(60);
/// The file lock fails fast, so in-process writers queue here first (create handlers and
/// the scan run concurrently), and a busy file lock is retried briefly.
static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
const LOCK_ATTEMPTS: usize = 40;
const LOCK_RETRY: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SentTask {
    origin: String,
    requester_agent_id: u64,
    team_id: u64,
    task_id: u64,
    /// Local time the broker saw the successful create (milliseconds since the epoch).
    created_at_ms: u64,
    /// Overdue was decided (reported, or the task already had a final status).
    settled: bool,
}

/// One local advisory per task; `event_id` is `overdue:<taskId>`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OverdueEvent {
    pub event_id: String,
    pub origin: String,
    pub requester_agent_id: u64,
    pub team_id: u64,
    pub task_id: u64,
    pub minutes: u64,
    pub observed_at_ms: u64,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    tasks: Vec<SentTask>,
    events: Vec<OverdueEvent>,
}

fn path(config_path: &Path) -> PathBuf {
    config::state_directory(config_path).join("sent-tasks.json")
}
fn valid_id(id: u64) -> bool {
    id > 0 && id <= i64::MAX as u64
}
fn read(path: &Path) -> Result<Snapshot> {
    let value: Snapshot = private_file::read(path, MAX_BYTES)?.unwrap_or(Snapshot {
        schema_version: 1,
        ..Snapshot::default()
    });
    ensure!(
        value.schema_version == 1
            && value.tasks.len() <= MAX_TASKS
            && value.events.len() <= MAX_EVENTS,
        "Invalid sent task snapshot"
    );
    let mut tasks = std::collections::HashSet::new();
    for task in &value.tasks {
        ensure!(
            valid_id(task.requester_agent_id)
                && valid_id(task.team_id)
                && valid_id(task.task_id)
                && endpoint_url(&task.origin)?.as_str() == task.origin
                && tasks.insert((&task.origin, task.requester_agent_id, task.task_id)),
            "Invalid sent task record"
        );
    }
    let mut events = std::collections::HashSet::new();
    for event in &value.events {
        ensure!(
            event.event_id == format!("overdue:{}", event.task_id)
                && valid_id(event.task_id)
                && (1..=10_080).contains(&event.minutes)
                && valid_id(event.requester_agent_id)
                && valid_id(event.team_id)
                && endpoint_url(&event.origin)?.as_str() == event.origin
                && events.insert((&event.origin, event.requester_agent_id, &event.event_id)),
            "Invalid overdue event"
        );
    }
    Ok(value)
}
/// Serialize writers in this process, then take the cross-process file lock.
fn locked(path: &Path) -> Result<(std::sync::MutexGuard<'static, ()>, std::fs::File)> {
    let guard = WRITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let lock = path.with_extension("lock");
    for _ in 1..LOCK_ATTEMPTS {
        if let Some(file) = config::try_lock(&lock)? {
            return Ok((guard, file));
        }
        std::thread::sleep(LOCK_RETRY);
    }
    Ok((guard, config::lock(&lock)?))
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Where the create path records successful creates for one broker profile.
#[derive(Clone)]
pub struct Ledger {
    config_path: Arc<PathBuf>,
    origin: Arc<String>,
}
impl Ledger {
    pub fn new(config_path: &Path, server_url: &str) -> Result<Self> {
        Ok(Self {
            config_path: Arc::new(config_path.to_path_buf()),
            origin: Arc::new(endpoint_url(server_url)?.to_string()),
        })
    }
    /// Best effort and detached: the create response never waits for this local write,
    /// and a failed write never changes the create result.
    pub fn created(&self, requester_agent_id: u64, team_id: u64, task_id: u64) {
        let ledger = self.clone();
        drop(tokio::task::spawn_blocking(move || {
            record(
                &ledger.config_path,
                &ledger.origin,
                requester_agent_id,
                team_id,
                task_id,
                now_ms(),
            )
        }));
    }
}

/// Record a created task once; a replayed create of the same task is a no-op.
pub fn record(
    config_path: &Path,
    origin: &str,
    requester_agent_id: u64,
    team_id: u64,
    task_id: u64,
    created_at_ms: u64,
) -> Result<()> {
    ensure!(
        valid_id(requester_agent_id) && valid_id(team_id) && valid_id(task_id),
        "Invalid sent task"
    );
    let origin = endpoint_url(origin)?.to_string();
    let path = path(config_path);
    let _guard = locked(&path)?;
    let mut snapshot = read(&path)?;
    if snapshot.tasks.iter().any(|t| {
        t.origin == origin && t.requester_agent_id == requester_agent_id && t.task_id == task_id
    }) {
        return Ok(());
    }
    // A replay after the record was pruned must not lead to a second advisory.
    let reported = snapshot.events.iter().any(|e| {
        e.origin == origin && e.requester_agent_id == requester_agent_id && e.task_id == task_id
    });
    if snapshot.tasks.len() >= MAX_TASKS {
        // Prefer forgetting tasks already decided; otherwise the oldest record.
        let index = snapshot.tasks.iter().position(|t| t.settled).unwrap_or(0);
        snapshot.tasks.remove(index);
    }
    snapshot.tasks.push(SentTask {
        origin,
        requester_agent_id,
        team_id,
        task_id,
        created_at_ms,
        settled: reported,
    });
    private_file::write(&path, &snapshot, MAX_BYTES)
}

fn finished(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Rejected
    )
}

/// Decide every recorded task past `minutes`: append one overdue event if no final
/// status notification was retained, otherwise only mark it settled. Returns the
/// number of new events. `minutes == 0` disables the check.
pub fn scan(config_path: &Path, minutes: u64, now_ms: u64) -> Result<usize> {
    if minutes == 0 {
        return Ok(0);
    }
    let threshold = minutes.saturating_mul(60_000);
    let path = path(config_path);
    let _guard = locked(&path)?;
    let mut snapshot = read(&path)?;
    let mut finished_by_scope = HashMap::<(String, u64), std::collections::HashSet<u64>>::new();
    let (mut changed, mut added) = (false, 0);
    for index in 0..snapshot.tasks.len() {
        let task = snapshot.tasks[index].clone();
        if task.settled || now_ms < task.created_at_ms.saturating_add(threshold) {
            continue;
        }
        let key = (task.origin.clone(), task.requester_agent_id);
        if !finished_by_scope.contains_key(&key) {
            let done = Notifications::events_config(config_path, &key.0, key.1)?
                .into_iter()
                .filter(|event| finished(event.status()))
                .map(|event| event.task_id())
                .collect();
            finished_by_scope.insert(key.clone(), done);
        }
        let event_id = format!("overdue:{}", task.task_id);
        let duplicate = snapshot.events.iter().any(|e| {
            e.origin == task.origin
                && e.requester_agent_id == task.requester_agent_id
                && e.event_id == event_id
        });
        if !duplicate && !finished_by_scope[&key].contains(&task.task_id) {
            if snapshot.events.len() >= MAX_EVENTS {
                // Retention is full: leave the task undecided rather than drop history.
                continue;
            }
            snapshot.events.push(OverdueEvent {
                event_id,
                origin: task.origin.clone(),
                requester_agent_id: task.requester_agent_id,
                team_id: task.team_id,
                task_id: task.task_id,
                minutes,
                observed_at_ms: now_ms,
            });
            added += 1;
        }
        snapshot.tasks[index].settled = true;
        changed = true;
    }
    if changed {
        private_file::write(&path, &snapshot, MAX_BYTES)?;
    }
    Ok(added)
}

/// Update-feed source in persisted append order. Read-only.
pub(crate) fn events_config(
    config_path: &Path,
    origin: &str,
    agent_id: u64,
    team_id: u64,
) -> Result<Vec<OverdueEvent>> {
    let origin = endpoint_url(origin)?.to_string();
    Ok(read(&path(config_path))?
        .events
        .into_iter()
        .filter(|e| e.origin == origin && e.requester_agent_id == agent_id && e.team_id == team_id)
        .collect())
}

/// Caller holds stopped-broker/control locks; resets local advisory retention only.
pub fn clear_config(config_path: &Path) -> Result<()> {
    let path = path(config_path);
    let _guard = locked(&path)?;
    private_file::write(
        &path,
        &Snapshot {
            schema_version: 1,
            ..Snapshot::default()
        },
        MAX_BYTES,
    )
}

/// Background check every minute while the broker runs. Failures are retried next round.
pub async fn run(config_path: PathBuf, minutes: u64, cancel: CancellationToken) {
    if minutes == 0 {
        return;
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(SCAN_INTERVAL) => {}
        }
        let path = config_path.clone();
        let _ = tokio::task::spawn_blocking(move || scan(&path, minutes, now_ms())).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        notifications::{CompletedPayload, StatusPayload, TaskNotification},
        task_operations::Version,
    };

    const ORIGIN: &str = "ws://127.0.0.1:9";
    const MINUTE: u64 = 60_000;

    fn profile() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        config::init(&config).unwrap();
        (dir, config)
    }
    fn events(config: &Path, agent: u64, team: u64) -> Vec<u64> {
        events_config(config, ORIGIN, agent, team)
            .unwrap()
            .into_iter()
            .map(|e| e.task_id)
            .collect()
    }
    async fn finish(config: &Path, agent: u64, task: u64, status: TaskStatus) {
        let event_id = uuid::Uuid::new_v4().to_string();
        let event = match status {
            TaskStatus::Completed => TaskNotification::TaskCompleted {
                event_id,
                task_id: task,
                version: Version(3),
                payload: CompletedPayload {
                    status,
                    result_text: "done".into(),
                    partially_modified: false,
                    started_at: "2026-11-06T00:00:00Z".into(),
                    completed_at: "2026-11-06T00:10:00Z".into(),
                },
            },
            _ => TaskNotification::TaskStatusChanged {
                event_id,
                task_id: task,
                version: Version(2),
                payload: StatusPayload {
                    previous_status: if status == TaskStatus::Rejected {
                        TaskStatus::Pending
                    } else {
                        TaskStatus::InProgress
                    },
                    status,
                },
            },
        };
        Notifications::open(config)
            .unwrap()
            .record(ORIGIN.into(), agent, event)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn overdue_is_reported_once_after_the_threshold_unless_finished() {
        let (_dir, config) = profile();
        for task in [10, 11, 12] {
            record(&config, ORIGIN, 7, 3, task, 0).unwrap();
        }
        record(&config, ORIGIN, 7, 3, 10, 5 * MINUTE).unwrap(); // replay keeps the first time
        finish(&config, 7, 11, TaskStatus::Failed).await;

        assert_eq!(scan(&config, 30, 29 * MINUTE).unwrap(), 0);
        assert!(events(&config, 7, 3).is_empty());
        assert_eq!(scan(&config, 30, 30 * MINUTE).unwrap(), 2);
        assert_eq!(events(&config, 7, 3), [10, 12]);
        // Decided tasks are never reported again, even after a later final status.
        finish(&config, 7, 10, TaskStatus::Completed).await;
        assert_eq!(scan(&config, 30, 90 * MINUTE).unwrap(), 0);
        assert_eq!(events(&config, 7, 3), [10, 12]);
        let event = &events_config(&config, ORIGIN, 7, 3).unwrap()[0];
        assert_eq!(event.event_id, "overdue:10");
        assert_eq!(event.minutes, 30);
    }

    #[tokio::test]
    async fn scope_threshold_zero_and_clear_are_respected() {
        let (_dir, config) = profile();
        record(&config, ORIGIN, 7, 3, 20, 0).unwrap();
        record(&config, ORIGIN, 8, 3, 21, 0).unwrap();
        record(&config, "ws://127.0.0.1:10", 7, 3, 22, 0).unwrap();
        assert_eq!(scan(&config, 0, u64::MAX).unwrap(), 0);
        assert_eq!(scan(&config, 1, MINUTE).unwrap(), 3);
        assert_eq!(events(&config, 7, 3), [20]);
        assert_eq!(events(&config, 8, 3), [21]);
        assert!(events(&config, 7, 4).is_empty());
        clear_config(&config).unwrap();
        assert!(events(&config, 7, 3).is_empty());
        assert!(record(&config, ORIGIN, 0, 3, 1, 0).is_err());
    }

    #[test]
    fn a_replay_after_pruning_does_not_report_twice() {
        let (_dir, config) = profile();
        record(&config, ORIGIN, 7, 3, 30, 0).unwrap();
        assert_eq!(scan(&config, 1, MINUTE).unwrap(), 1);
        // Simulate the record being pruned, then the same task recorded again.
        let mut snapshot = read(&path(&config)).unwrap();
        snapshot.tasks.clear();
        private_file::write(&path(&config), &snapshot, MAX_BYTES).unwrap();
        record(&config, ORIGIN, 7, 3, 30, 5 * MINUTE).unwrap();
        assert_eq!(scan(&config, 1, 60 * MINUTE).unwrap(), 0);
        assert_eq!(events(&config, 7, 3), [30]);
    }

    #[test]
    fn concurrent_records_and_scans_are_all_kept() {
        let (_dir, config) = profile();
        std::thread::scope(|scope| {
            for task in 1..=8u64 {
                let config = &config;
                scope.spawn(move || {
                    record(config, ORIGIN, 7, 3, task, 0).unwrap();
                    scan(config, 30, 0).unwrap();
                });
            }
        });
        assert_eq!(read(&path(&config)).unwrap().tasks.len(), 8);
    }

    #[test]
    fn retention_forgets_settled_tasks_first() {
        let (_dir, config) = profile();
        // Seed a full ledger in one write; recording 1024 times would be slow on Windows ACLs.
        let tasks = (1..=MAX_TASKS as u64)
            .map(|task| SentTask {
                origin: endpoint_url(ORIGIN).unwrap().to_string(),
                requester_agent_id: 7,
                team_id: 3,
                task_id: task,
                created_at_ms: task * MINUTE,
                settled: false,
            })
            .collect();
        let full = Snapshot {
            schema_version: 1,
            tasks,
            events: Vec::new(),
        };
        private_file::write(&path(&config), &full, MAX_BYTES).unwrap();
        // Only task 1 is old enough to be decided.
        assert_eq!(scan(&config, 1, 2 * MINUTE).unwrap(), 1);
        record(&config, ORIGIN, 7, 3, 5000, 0).unwrap();
        let snapshot = read(&path(&config)).unwrap();
        assert_eq!(snapshot.tasks.len(), MAX_TASKS);
        assert!(!snapshot.tasks.iter().any(|t| t.task_id == 1));
        assert!(snapshot.tasks.iter().any(|t| t.task_id == 2));
    }
}
