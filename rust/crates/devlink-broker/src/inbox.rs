//! Durable TASK_CREATED inbox. Keep the upstream path/schema/API and acknowledge only persisted data.
use crate::{
    central::endpoint_url,
    config, credentials,
    protocol::{TaskAckPayload, TaskCreatedMessage},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};

const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_TASKS: usize = 1024;
const MAX_RECEIPTS: usize = 16_384;
pub const MAX_BATCH: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredTask {
    pub server_url: String,
    pub message: TaskCreatedMessage,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    server_url: String,
    agent_id: u64,
    event_id: String,
    task_id: u64,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    tasks: Vec<StoredTask>,
    // Schema 2 receipt history; schema 1 tasks-only snapshots remain readable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    receipts: Vec<Receipt>,
}
type EventKey = (String, u64, String);
impl Snapshot {
    fn events(&self) -> Result<HashMap<EventKey, u64>> {
        ensure!(
            matches!(self.schema_version, 1 | 2),
            "Unsupported inbox schema"
        );
        ensure!(
            self.schema_version == 2 || self.receipts.is_empty(),
            "Invalid legacy inbox receipt history"
        );
        ensure!(
            self.tasks.len() <= MAX_TASKS && self.receipts.len() <= MAX_RECEIPTS,
            "Inbox exceeds retention limit"
        );
        let mut tasks = HashSet::new();
        let mut events = HashMap::new();
        for task in &self.tasks {
            let m = &task.message;
            ensure!(m.is_valid(), "Invalid stored task");
            ensure!(
                endpoint_url(&task.server_url)?.as_str() == task.server_url,
                "Invalid inbox origin"
            );
            ensure!(
                tasks.insert((
                    task.server_url.clone(),
                    m.payload.receiver_agent_id,
                    m.payload.task_id
                )),
                "Duplicate stored task"
            );
            let key = (
                task.server_url.clone(),
                m.payload.receiver_agent_id,
                m.event_id.clone(),
            );
            ensure!(
                events.insert(key, m.payload.task_id).is_none(),
                "Conflicting stored event"
            );
        }
        let mut receipts = HashSet::new();
        for r in &self.receipts {
            ensure!(
                !r.event_id.trim().is_empty() && r.event_id.encode_utf16().count() <= 128,
                "Invalid stored receipt"
            );
            ensure!(
                tasks.contains(&(r.server_url.clone(), r.agent_id, r.task_id)),
                "Orphan stored receipt"
            );
            let key = (r.server_url.clone(), r.agent_id, r.event_id.clone());
            ensure!(receipts.insert(key.clone()), "Duplicate stored receipt");
            if let Some(id) = events.insert(key, r.task_id) {
                ensure!(id == r.task_id, "Conflicting stored event");
            }
        }
        Ok(events)
    }
}
struct Store {
    path: PathBuf,
    tasks: Vec<StoredTask>,
    failed: bool,
}
#[derive(Default)]
struct Diagnostics {
    rejected: AtomicU64,
    last: AtomicU8,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxDiagnostics {
    pub rejected_events: u64,
    pub last_error: Option<InboxFailure>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InboxFailure {
    StoreFailed,
    QueueFull,
}

/// Preserve upstream clone semantics: one writer permit shared by all connection generations.
#[derive(Clone)]
pub struct Inbox(
    Arc<Mutex<Store>>,
    Arc<tokio::sync::Semaphore>,
    Arc<AtomicBool>,
    Arc<Diagnostics>,
);
impl Inbox {
    #[cfg(test)]
    pub(crate) fn has_pending_write(&self) -> bool {
        self.1.available_permits() == 0
    }

    pub fn path(config_path: &Path) -> PathBuf {
        config::state_directory(config_path).join("inbox.json")
    }
    pub fn open(config_path: &Path) -> Result<Self> {
        let path = Self::path(config_path);
        let parent = path.parent().context("Inbox has no parent")?;
        credentials::ensure_safe_parent_directory(parent)?;
        credentials::set_private_permissions(parent)?;
        let tasks = read_snapshot(&path)?.unwrap_or_else(empty).tasks;
        Ok(Self(
            Arc::new(Mutex::new(Store {
                path,
                tasks,
                failed: false,
            })),
            Arc::new(tokio::sync::Semaphore::new(1)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(Diagnostics::default()),
        ))
    }
    pub fn diagnostics(&self) -> InboxDiagnostics {
        InboxDiagnostics {
            rejected_events: self.3.rejected.load(Ordering::Relaxed),
            last_error: match self.3.last.load(Ordering::Relaxed) {
                1 => Some(InboxFailure::StoreFailed),
                2 => Some(InboxFailure::QueueFull),
                _ => None,
            },
        }
    }
    pub fn reject_queue(&self) {
        self.3.rejected.fetch_add(1, Ordering::Relaxed);
        self.3.last.store(2, Ordering::Relaxed);
    }
    pub fn stop_accepting(&self) {
        self.2.store(true, Ordering::Release);
    }
    pub async fn record(
        &self,
        server_url: String,
        agent_id: u64,
        message: TaskCreatedMessage,
    ) -> Result<TaskAckPayload> {
        self.record_batch(server_url, agent_id, vec![message])
            .await?
            .pop()
            .context("Empty inbox acknowledgement")
    }
    /// A batch is one atomic snapshot. Any conflict fails the batch without a successful ACK.
    pub async fn record_batch(
        &self,
        server_url: String,
        agent_id: u64,
        messages: Vec<TaskCreatedMessage>,
    ) -> Result<Vec<TaskAckPayload>> {
        ensure!(
            !messages.is_empty() && messages.len() <= MAX_BATCH,
            "Invalid inbox batch size"
        );
        let permit = self
            .1
            .clone()
            .acquire_owned()
            .await
            .context("Inbox worker closed")?;
        let inbox = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let count = messages.len() as u64;
            let result = inbox.record_batch_sync(&server_url, agent_id, messages);
            if result.is_err() {
                inbox.3.rejected.fetch_add(count, Ordering::Relaxed);
                inbox.3.last.store(1, Ordering::Relaxed);
            }
            result
        })
        .await
        .context("Inbox worker failed")?
    }
    #[cfg(test)]
    fn record_sync(
        &self,
        server_url: &str,
        agent_id: u64,
        message: TaskCreatedMessage,
    ) -> Result<TaskAckPayload> {
        self.record_batch_sync(server_url, agent_id, vec![message])?
            .pop()
            .context("Empty inbox acknowledgement")
    }
    fn record_batch_sync(
        &self,
        server_url: &str,
        agent_id: u64,
        messages: Vec<TaskCreatedMessage>,
    ) -> Result<Vec<TaskAckPayload>> {
        let server_url = endpoint_url(server_url)?.to_string();
        let mut store = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Inbox lock failed"))?;
        ensure!(
            !store.failed,
            "Inbox requires restart after storage failure"
        );
        let parent = store.path.parent().context("Inbox has no parent")?;
        credentials::verify_parent_safety(parent)?;
        let _file_lock = config::lock(&parent.join("inbox.lock"))?;
        // A cancelled, old-generation worker cannot resurrect records after stopped-broker clear.
        ensure!(!self.2.load(Ordering::Acquire), "Inbox writer is stopped");
        let mut snapshot = read_snapshot(&store.path)?.unwrap_or_else(empty);
        ensure!(
            store.tasks.is_empty() || !snapshot.tasks.is_empty(),
            "Inbox snapshot is missing"
        );
        let mut events = snapshot.events()?;
        let mut changed = false;
        let mut acknowledgements = Vec::with_capacity(messages.len());
        for message in messages {
            ensure!(
                message.is_valid() && message.payload.receiver_agent_id == agent_id,
                "Invalid task recipient or payload"
            );
            let key = (server_url.clone(), agent_id, message.event_id.clone());
            if let Some(task_id) = events.get(&key) {
                ensure!(
                    *task_id == message.payload.task_id,
                    "Conflicting duplicate event"
                );
            }
            if let Some(existing) = snapshot.tasks.iter().find(|task| {
                task.server_url == server_url
                    && task.message.payload.receiver_agent_id == agent_id
                    && task.message.payload.task_id == message.payload.task_id
            }) {
                ensure!(
                    existing.message.payload == message.payload,
                    "Conflicting duplicate task"
                );
            } else {
                ensure!(snapshot.tasks.len() < MAX_TASKS, "Inbox is full");
                snapshot.tasks.push(StoredTask {
                    server_url: server_url.clone(),
                    message: message.clone(),
                });
                changed = true;
            }
            if let std::collections::hash_map::Entry::Vacant(entry) = events.entry(key) {
                ensure!(
                    snapshot.receipts.len() < MAX_RECEIPTS,
                    "Inbox receipt history is full"
                );
                entry.insert(message.payload.task_id);
                snapshot.receipts.push(Receipt {
                    server_url: server_url.clone(),
                    agent_id,
                    event_id: message.event_id.clone(),
                    task_id: message.payload.task_id,
                });
                changed = true;
            }
            acknowledgements.push(message.ack());
        }
        if changed {
            snapshot.schema_version = 2;
        }
        if changed && let Err(error) = write_snapshot(&store.path, &snapshot) {
            store.failed = error.published;
            anyhow::bail!("Cannot persist inbox snapshot");
        }
        store.tasks = snapshot.tasks;
        Ok(acknowledgements)
    }
    #[cfg(test)]
    pub(crate) fn pause_writes(
        &self,
    ) -> (
        std::sync::mpsc::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let store = self.0.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let (started, ready) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _guard = store.lock().unwrap();
            let _ = started.send(());
            let _ = wait.recv();
        });
        (release, ready)
    }
    pub fn list_for(&self, server_url: &str, agent_id: u64) -> Result<Vec<StoredTask>> {
        let server_url = endpoint_url(server_url)?.to_string();
        let store = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("Inbox lock failed"))?;
        Ok(store
            .tasks
            .iter()
            .filter(|task| {
                task.server_url == server_url && task.message.payload.receiver_agent_id == agent_id
            })
            .cloned()
            .collect())
    }
    /// Read-only CLI snapshot access; no directory, profile, server request, or task state is created.
    pub fn list_config(
        config_path: &Path,
        server_url: &str,
        agent_id: Option<u64>,
        limit: usize,
    ) -> Result<Vec<StoredTask>> {
        ensure!((1..=MAX_TASKS).contains(&limit), "Invalid inbox list limit");
        let origin = endpoint_url(server_url)?.to_string();
        let snapshot = read_snapshot(&Self::path(config_path))?.unwrap_or_else(empty);
        Ok(snapshot
            .tasks
            .into_iter()
            .rev()
            .filter(|t| {
                t.server_url == origin
                    && agent_id.is_none_or(|id| id == t.message.payload.receiver_agent_id)
            })
            .take(limit)
            .collect())
    }
    /// Caller must hold control.lock and broker.lock; retain strict safe-path/private-file checks.
    pub fn clear_config(config_path: &Path) -> Result<()> {
        let path = Self::path(config_path);
        let parent = path.parent().context("Inbox has no parent")?;
        if !parent.try_exists()? {
            return Ok(());
        }
        credentials::verify_parent_safety(parent)?;
        let _file_lock = config::lock(&parent.join("inbox.lock"))?;
        let _snapshot = read_snapshot(&path)?;
        if path.try_exists()? {
            fs::remove_file(&path).context("Cannot clear inbox snapshot")?;
        }
        // Only our private atomic-write leftovers; never remove other credential snapshots.
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(".inbox-") {
                credentials::ensure_safe_target_if_exists(&entry.path())?;
                credentials::verify_private_permissions(&entry.path())?;
                ensure!(entry.file_type()?.is_file(), "Unsafe inbox temporary entry");
                fs::remove_file(entry.path()).context("Cannot clear inbox temporary file")?;
            }
        }
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}
fn empty() -> Snapshot {
    Snapshot {
        schema_version: 2,
        ..Snapshot::default()
    }
}
fn read_snapshot(path: &Path) -> Result<Option<Snapshot>> {
    let parent = path.parent().context("Inbox has no parent")?;
    if fs::symlink_metadata(parent).is_ok() {
        credentials::verify_parent_safety(parent)?;
    }
    if !credentials::ensure_safe_target_if_exists(path)? {
        return Ok(None);
    }
    credentials::verify_private_permissions(path)?;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "Inbox exceeds size limit");
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("Invalid inbox JSON"))?;
    snapshot.events()?;
    Ok(Some(snapshot))
}
#[derive(Debug)]
struct SnapshotWriteError {
    published: bool,
}

#[cfg(all(test, windows))]
std::thread_local! {
    static PERSIST_RETRY_SIGNAL: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

fn persist_with_retry(mut temp: tempfile::NamedTempFile, path: &Path) -> Result<File> {
    for attempt in 0..10 {
        match temp.persist(path) {
            Ok(file) => return Ok(file),
            Err(error) => {
                let transient = cfg!(windows)
                    && (error.error.kind() == std::io::ErrorKind::PermissionDenied
                        || matches!(error.error.raw_os_error(), Some(32 | 33)));
                if !transient || attempt == 9 {
                    anyhow::bail!("Cannot replace inbox");
                }
                temp = error.file;
                #[cfg(all(test, windows))]
                PERSIST_RETRY_SIGNAL.with(|signal| {
                    if let Some(sender) = signal.borrow_mut().take() {
                        let _ = sender.send(());
                    }
                });
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
    unreachable!("bounded persistence loop always returns")
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> std::result::Result<(), SnapshotWriteError> {
    let mut published = false;
    let result = (|| -> Result<()> {
        let parent = path.parent().context("Inbox has no parent")?;
        let bytes = serde_json::to_vec(snapshot).context("Cannot encode inbox")?;
        ensure!(bytes.len() <= MAX_BYTES, "Inbox exceeds size limit");
        let mut temp = tempfile::Builder::new()
            .prefix(".inbox-")
            .tempfile_in(parent)?;
        credentials::set_private_permissions(temp.path())?;
        credentials::verify_private_permissions(temp.path())?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        let persisted = persist_with_retry(temp, path)?;
        published = true;
        credentials::verify_private_permissions(path)?;
        persisted.sync_all()?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    result.map_err(|_| SnapshotWriteError { published })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TaskCreatedPayload;

    fn task(event: &str, receiver: u64) -> TaskCreatedMessage {
        TaskCreatedMessage {
            event_id: event.into(),
            payload: TaskCreatedPayload {
                task_id: 101,
                team_id: 1,
                requester_agent_id: 10,
                receiver_agent_id: receiver,
                prompt: "테스트 작업".into(),
                created_at: "2026-10-29T09:00:00".into(),
            },
        }
    }

    #[cfg(windows)]
    #[test]
    fn occupied_windows_file_retries_then_acknowledges_only_after_reader_release() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        inbox
            .record_sync("ws://localhost:8080", 12, task("first", 12))
            .unwrap();
        // A third-party reader without FILE_SHARE_DELETE (for example a scanner).
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(Inbox::path(&path))
            .unwrap();
        let (signal, waiting) = std::sync::mpsc::channel();
        PERSIST_RETRY_SIGNAL.with(|slot| *slot.borrow_mut() = Some(signal));
        let holder = std::thread::spawn(move || {
            waiting
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            std::thread::sleep(std::time::Duration::from_millis(75));
            drop(reader);
        });
        let mut message = task("next", 12);
        message.payload.task_id = 102;
        let result = inbox.record_sync("ws://localhost:8080", 12, message);
        PERSIST_RETRY_SIGNAL.with(|slot| slot.borrow_mut().take());
        holder.join().unwrap();
        assert_eq!(result.unwrap().task_id, 102);
        assert_eq!(
            Inbox::open(&path)
                .unwrap()
                .list_for("ws://localhost:8080", 12)
                .unwrap()
                .len(),
            2
        );
    }

    #[cfg(windows)]
    #[test]
    fn exhausted_replace_does_not_ack_change_data_or_poison_unpublished_store() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        inbox
            .record_sync("ws://localhost:8080", 12, task("first", 12))
            .unwrap();
        let before = fs_read(&Inbox::path(&path));
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(Inbox::path(&path))
            .unwrap();
        let mut message = task("next", 12);
        message.payload.task_id = 102;
        assert!(
            inbox
                .record_sync("ws://localhost:8080", 12, message.clone())
                .is_err()
        );
        assert_eq!(fs_read(&Inbox::path(&path)), before);
        assert!(!inbox.0.lock().unwrap().failed);
        drop(reader);
        assert_eq!(
            inbox
                .record_sync("ws://localhost:8080", 12, message)
                .unwrap()
                .task_id,
            102
        );
    }

    #[test]
    fn legacy_snapshot_is_read_without_loss_and_new_event_history_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        let origin = "ws://localhost:8080/ws/broker";
        // Generated by running the unmodified d49cdba production Inbox in a pinned archive.
        std::fs::write(
            Inbox::path(&path),
            include_bytes!("../tests/fixtures/inbox-v1-d49cdba.json"),
        )
        .unwrap();
        credentials::set_private_permissions(&Inbox::path(&path)).unwrap();
        let before = fs_read(&Inbox::path(&path));
        drop(inbox);
        let inbox = Inbox::open(&path).unwrap();
        inbox.record_sync(origin, 12, task("legacy", 12)).unwrap();
        assert_eq!(fs_read(&Inbox::path(&path)), before);
        inbox
            .record_sync(origin, 12, task("redelivery", 12))
            .unwrap();
        drop(inbox);
        let snapshot = read_snapshot(&Inbox::path(&path)).unwrap().unwrap();
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(snapshot.tasks.len(), 1);
        let inbox = Inbox::open(&path).unwrap();
        let before = fs_read(&Inbox::path(&path));
        let mut conflict = task("redelivery", 12);
        conflict.payload.task_id = 102;
        assert!(inbox.record_sync(origin, 12, conflict).is_err());
        assert_eq!(fs_read(&Inbox::path(&path)), before);
    }

    #[tokio::test]
    async fn batch_is_atomic_and_stopped_workers_cannot_recreate_cleared_records() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        let first = task("batch", 12);
        let mut conflict = first.clone();
        conflict.payload.prompt = "conflict".into();
        assert!(
            inbox
                .record_batch(
                    "ws://localhost:8080".into(),
                    12,
                    vec![first.clone(), conflict]
                )
                .await
                .is_err()
        );
        assert!(!Inbox::path(&path).exists());
        assert_eq!(inbox.diagnostics().rejected_events, 2);
        inbox
            .record("ws://localhost:8080".into(), 12, first.clone())
            .await
            .unwrap();
        let guard = config::lock(&config::state_directory(&path).join("inbox.lock")).unwrap();
        assert!(Inbox::clear_config(&path).is_err());
        assert!(Inbox::path(&path).exists());
        drop(guard);
        inbox.stop_accepting();
        Inbox::clear_config(&path).unwrap();
        assert!(
            inbox
                .record("ws://localhost:8080".into(), 12, first)
                .await
                .is_err()
        );
        assert!(!Inbox::path(&path).exists());
    }

    #[test]
    fn read_only_listing_does_not_create_directories_and_rejects_unsafe_or_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        assert!(
            Inbox::list_config(&path, "ws://localhost:8080", None, 20)
                .unwrap()
                .is_empty()
        );
        assert!(!config::state_directory(&path).exists());
        let _inbox = Inbox::open(&path).unwrap();
        std::fs::write(Inbox::path(&path), b"bad-json").unwrap();
        credentials::set_private_permissions(&Inbox::path(&path)).unwrap();
        assert!(Inbox::list_config(&path, "ws://localhost:8080", None, 20).is_err());
        assert!(Inbox::clear_config(&path).is_err());
        assert_eq!(fs_read(&Inbox::path(&path)), b"bad-json");
    }

    #[test]
    fn restart_deduplicates_new_events_and_isolates_origin_and_agent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        inbox
            .record_sync("ws://localhost:8080", 12, task("old", 12))
            .unwrap();
        let before = fs_read(&Inbox::path(&path));
        drop(inbox);
        let inbox = Inbox::open(&path).unwrap();
        let ack = inbox
            .record_sync("ws://localhost:8080/ws/broker", 12, task("new", 12))
            .unwrap();
        assert_eq!(ack.event_id, "new");
        assert_ne!(fs_read(&Inbox::path(&path)), before); // New delivery ID is retained; no new task.
        inbox
            .record_sync("ws://localhost:8081", 12, task("other-server", 12))
            .unwrap();
        inbox
            .record_sync("ws://localhost:8080", 13, task("other-agent", 13))
            .unwrap();
        assert_eq!(inbox.list_for("ws://localhost:8080", 12).unwrap().len(), 1);
        assert_eq!(inbox.list_for("ws://localhost:8081", 12).unwrap().len(), 1);
        assert_eq!(inbox.list_for("ws://localhost:8080", 13).unwrap().len(), 1);
        assert_eq!(inbox.0.lock().unwrap().tasks.len(), 3);
        credentials::verify_private_permissions(&Inbox::path(&path)).unwrap();
    }

    #[test]
    fn invalid_recipient_and_conflicting_duplicates_preserve_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        assert!(
            inbox
                .record_sync("ws://localhost:8080", 12, task("bad-recipient", 13))
                .is_err()
        );
        assert!(!Inbox::path(&path).exists());
        inbox
            .record_sync("ws://localhost:8080", 12, task("first", 12))
            .unwrap();
        let before = fs_read(&Inbox::path(&path));
        let mut conflict = task("conflict", 12);
        conflict.payload.prompt = "different".into();
        assert!(
            inbox
                .record_sync("ws://localhost:8080", 12, conflict)
                .is_err()
        );
        assert_eq!(fs_read(&Inbox::path(&path)), before);
    }

    #[test]
    fn corrupt_or_missing_snapshot_is_not_silently_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        inbox
            .record_sync("ws://localhost:8080", 12, task("first", 12))
            .unwrap();
        std::fs::remove_file(Inbox::path(&path)).unwrap();
        assert!(
            inbox
                .record_sync("ws://localhost:8080", 12, task("duplicate", 12))
                .is_err()
        );
        std::fs::write(Inbox::path(&path), b"partial-json").unwrap();
        assert!(Inbox::open(&path).is_err());
        assert_eq!(fs_read(&Inbox::path(&path)), b"partial-json");
    }

    #[test]
    fn write_failure_does_not_publish_in_memory_and_limits_do_not_evict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let inbox = Inbox::open(&path).unwrap();
        let mut store = inbox.0.lock().unwrap();
        store.tasks = (1..=MAX_TASKS as u64)
            .map(|id| {
                let mut message = task(&format!("existing-{id}"), 12);
                message.payload.task_id = id;
                StoredTask {
                    server_url: "ws://localhost:8080/ws/broker".into(),
                    message,
                }
            })
            .collect();
        write_snapshot(
            &store.path,
            &Snapshot {
                schema_version: 1,
                tasks: store.tasks.clone(),
                receipts: Vec::new(),
            },
        )
        .unwrap();
        drop(store);
        let mut extra = task("full", 12);
        extra.payload.task_id = MAX_TASKS as u64 + 1;
        assert!(inbox.record_sync("ws://localhost:8080", 12, extra).is_err());
        assert_eq!(inbox.0.lock().unwrap().tasks.len(), MAX_TASKS);
        assert_eq!(
            Inbox::open(&path)
                .unwrap()
                .list_for("ws://localhost:8080", 12)
                .unwrap()
                .len(),
            MAX_TASKS
        );
        // A directory at the target path is rejected before any ACK can be built.
        std::fs::remove_file(Inbox::path(&path)).unwrap();
        std::fs::create_dir(Inbox::path(&path)).unwrap();
        assert!(
            inbox
                .record_sync("ws://localhost:8080", 12, task("blocked", 12))
                .is_err()
        );
    }

    fn fs_read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap()
    }
}
