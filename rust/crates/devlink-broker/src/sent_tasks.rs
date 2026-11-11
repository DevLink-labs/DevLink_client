//! Requester-side "no result yet" advisory (MVP RCV-005).
//!
//! The broker records each task this Agent created. When a recorded task has had no
//! COMPLETED, FAILED or REJECTED notification for `taskOverdueMinutes`, one
//! `TASK_OVERDUE` event is appended for the local update feed. Central task state is
//! never changed and nothing is cancelled or retried. The same records drive the final
//! status recovery in `sent_task_recovery`: right before a task is decided, and on every
//! central READY, a task without a retained final status is looked up in central, so a
//! final status missed while the broker was off is stored and never reported as overdue.
//! `get_task_result` stays authoritative.
use crate::{
    central::endpoint_url,
    config,
    notifications::{Notifications, is_final},
    private_file,
    sent_task_recovery::Recovery,
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
    /// Overdue was decided (reported, the task already had a final status, or it was
    /// recorded while the check was off).
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
    /// Changed by every clear. A broker records only into the generation it started
    /// with, so a create still queued when a clear ran cannot restore the ledger (#80).
    #[serde(default)]
    generation: u64,
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
    /// Ledger generation when the broker started, read under the ledger lock. `None`
    /// (unreadable at start) refuses every record: a clear that later repairs the file
    /// must not let this broker's queued records restore it, and while the broker runs
    /// the file cannot be cleared, so no valid record is lost by refusing.
    generation: Option<u64>,
    /// `taskOverdueMinutes > 0`. Off, tasks are still recorded for final status recovery,
    /// but already settled, so enabling the check later never flags them at once.
    overdue: bool,
}
impl Ledger {
    pub fn new(config_path: &Path, server_url: &str) -> Result<Self> {
        Ok(Self {
            config_path: Arc::new(config_path.to_path_buf()),
            origin: Arc::new(endpoint_url(server_url)?.to_string()),
            generation: {
                let path = path(config_path);
                locked(&path)
                    .ok()
                    .and_then(|_guard| read(&path).ok())
                    .map(|s| s.generation)
            },
            overdue: true,
        })
    }
    /// Record for final status recovery only (`taskOverdueMinutes = 0`).
    pub fn without_overdue(mut self) -> Self {
        self.overdue = false;
        self
    }
    /// The synchronous write behind `created`, exposed for ordering tests.
    pub fn record_now(
        &self,
        requester_agent_id: u64,
        team_id: u64,
        task_id: u64,
        created_at_ms: u64,
    ) -> Result<()> {
        let generation = self.generation.ok_or_else(|| {
            anyhow::anyhow!("Sent task ledger was unreadable when the broker started")
        })?;
        record_in(
            &self.config_path,
            SentTask {
                origin: self.origin.to_string(),
                requester_agent_id,
                team_id,
                task_id,
                created_at_ms,
                settled: !self.overdue,
            },
            Some(generation),
        )
    }
    /// Best effort and detached: the create response never waits for this local write,
    /// and a failed write never changes the create result.
    pub fn created(&self, requester_agent_id: u64, team_id: u64, task_id: u64) {
        let ledger = self.clone();
        drop(tokio::task::spawn_blocking(move || {
            ledger.record_now(requester_agent_id, team_id, task_id, now_ms())
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
    record_in(
        config_path,
        SentTask {
            origin: origin.to_string(),
            requester_agent_id,
            team_id,
            task_id,
            created_at_ms,
            settled: false,
        },
        None,
    )
}
/// As `record`, but refused when the ledger was cleared after `generation` was read.
/// `task.settled` records it already decided (recovery only).
fn record_in(config_path: &Path, mut task: SentTask, generation: Option<u64>) -> Result<()> {
    ensure!(
        valid_id(task.requester_agent_id) && valid_id(task.team_id) && valid_id(task.task_id),
        "Invalid sent task"
    );
    task.origin = endpoint_url(&task.origin)?.to_string();
    let path = path(config_path);
    let _guard = locked(&path)?;
    let mut snapshot = read(&path)?;
    ensure!(
        generation.is_none_or(|g| g == snapshot.generation),
        "Sent task ledger was cleared after this broker started"
    );
    if snapshot.tasks.iter().any(|t| {
        t.origin == task.origin
            && t.requester_agent_id == task.requester_agent_id
            && t.task_id == task.task_id
    }) {
        return Ok(());
    }
    // A replay after the record was pruned must not lead to a second advisory.
    let reported = snapshot.events.iter().any(|e| {
        e.origin == task.origin
            && e.requester_agent_id == task.requester_agent_id
            && e.task_id == task.task_id
    });
    if snapshot.tasks.len() >= MAX_TASKS {
        // Prefer forgetting tasks already decided; otherwise the oldest record.
        let index = snapshot.tasks.iter().position(|t| t.settled).unwrap_or(0);
        snapshot.tasks.remove(index);
    }
    task.settled |= reported;
    snapshot.tasks.push(task);
    private_file::write(&path, &snapshot, MAX_BYTES)
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
                .filter(|event| is_final(event.status()))
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
    // A new generation even when the old file is unreadable, so no older record matches.
    let previous = read(&path).map(|s| s.generation).unwrap_or_default();
    private_file::write(
        &path,
        &Snapshot {
            schema_version: 1,
            generation: previous.saturating_add(1).max(now_ms()),
            ..Snapshot::default()
        },
        MAX_BYTES,
    )
}

/// A recorded task of one requester without a retained final status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Unfinished {
    pub task_id: u64,
    pub team_id: u64,
    pub created_at_ms: u64,
}

/// Recorded tasks of `agent` under `origin` that have no final status notification. With
/// `due = Some((minutes, now))`, only those the next `scan` would decide. Read-only, like
/// the update feed: the ledger and notifications are replaced atomically.
pub(crate) fn unfinished(
    config_path: &Path,
    origin: &str,
    agent: u64,
    due: Option<(u64, u64)>,
) -> Result<Vec<Unfinished>> {
    let origin = endpoint_url(origin)?.to_string();
    let snapshot = read(&path(config_path))?;
    let done: std::collections::HashSet<u64> =
        Notifications::events_config(config_path, &origin, agent)?
            .into_iter()
            .filter(|event| is_final(event.status()))
            .map(|event| event.task_id())
            .collect();
    Ok(snapshot
        .tasks
        .into_iter()
        .filter(|t| t.origin == origin && t.requester_agent_id == agent)
        .filter(|t| !done.contains(&t.task_id))
        .filter(|t| {
            due.is_none_or(|(minutes, now)| {
                minutes > 0
                    && !t.settled
                    && now
                        >= t.created_at_ms
                            .saturating_add(minutes.saturating_mul(60_000))
            })
        })
        .map(|t| Unfinished {
            task_id: t.task_id,
            team_id: t.team_id,
            created_at_ms: t.created_at_ms,
        })
        .collect())
}

/// Central confirmed a final status: the task is never reported overdue, even when the
/// rebuilt notification could not be stored.
pub(crate) fn settle(config_path: &Path, origin: &str, agent: u64, task_id: u64) -> Result<()> {
    let origin = endpoint_url(origin)?.to_string();
    let path = path(config_path);
    let _guard = locked(&path)?;
    let mut snapshot = read(&path)?;
    let Some(task) = snapshot.tasks.iter_mut().find(|t| {
        t.origin == origin && t.requester_agent_id == agent && t.task_id == task_id && !t.settled
    }) else {
        return Ok(());
    };
    task.settled = true;
    private_file::write(&path, &snapshot, MAX_BYTES)
}

/// Background worker while the broker runs. Every minute, tasks about to be decided are
/// looked up in central first, then `scan` runs. A central READY wakes a lookup of every
/// unfinished task. Failures are retried next round; passes never overlap.
pub async fn run(
    config_path: PathBuf,
    minutes: u64,
    recovery: Option<Recovery>,
    cancel: CancellationToken,
) {
    if minutes == 0 && recovery.is_none() {
        return;
    }
    let wake = recovery.as_ref().map(Recovery::wake);
    loop {
        let ready = tokio::select! {
            _ = cancel.cancelled() => return,
            _ = async {
                match &wake {
                    Some(wake) => wake.notified().await,
                    None => std::future::pending().await,
                }
            } => true,
            _ = tokio::time::sleep(SCAN_INTERVAL) => false,
        };
        // One time for the lookup and the scan, so no task becomes due in between.
        let now = now_ms();
        if let Some(recovery) = &recovery
            && (ready || minutes > 0)
        {
            let due = (!ready).then_some((minutes, now));
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = recovery.pass(&config_path, due) => {}
            }
        }
        if !ready && minutes > 0 {
            let path = config_path.clone();
            let _ = tokio::task::spawn_blocking(move || scan(&path, minutes, now)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        notifications::{CompletedPayload, StatusPayload, TaskNotification},
        task_operations::{TaskStatus, Version},
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
    fn a_record_queued_before_a_clear_does_not_restore_the_ledger() {
        let (_dir, config) = profile();
        // A broker started (and read the generation), then a create was queued.
        let ledger = Ledger::new(&config, ORIGIN).unwrap();
        ledger.record_now(7, 3, 10, 0).unwrap();
        // The broker stopped and the operator cleared local notifications.
        clear_config(&config).unwrap();
        // The queued write runs only now: it is refused and the ledger stays empty.
        assert!(ledger.record_now(7, 3, 11, 0).is_err());
        assert!(read(&path(&config)).unwrap().tasks.is_empty());
        // A broker started after the clear records normally, and a second clear moves on.
        let restarted = Ledger::new(&config, ORIGIN).unwrap();
        restarted.record_now(7, 3, 12, 0).unwrap();
        assert_eq!(read(&path(&config)).unwrap().tasks.len(), 1);
        let before = read(&path(&config)).unwrap().generation;
        clear_config(&config).unwrap();
        assert!(read(&path(&config)).unwrap().generation > before);
        assert!(restarted.record_now(7, 3, 13, 0).is_err());
    }

    #[test]
    fn a_ledger_that_was_unreadable_at_start_never_records_even_after_a_clear() {
        let (_dir, config) = profile();
        // Readable JSON that fails validation, written with private permissions.
        private_file::write(
            &path(&config),
            &serde_json::json!({"schemaVersion": 2, "tasks": [], "events": []}),
            MAX_BYTES,
        )
        .unwrap();
        let ledger = Ledger::new(&config, ORIGIN).unwrap();
        assert!(ledger.record_now(7, 3, 10, 0).is_err());
        // Clearing repairs the file, but this broker's queued writes still lose.
        clear_config(&config).unwrap();
        assert!(ledger.record_now(7, 3, 11, 0).is_err());
        assert!(read(&path(&config)).unwrap().tasks.is_empty());
        let restarted = Ledger::new(&config, ORIGIN).unwrap();
        restarted.record_now(7, 3, 12, 0).unwrap();
        assert_eq!(read(&path(&config)).unwrap().tasks.len(), 1);
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
            generation: 0,
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
