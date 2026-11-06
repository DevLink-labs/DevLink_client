//! Question/answer arrival records for tasks this Agent takes part in.
//!
//! Central `GET /api/v1/agents/me/task-messages` is the source of truth. WebSocket
//! `TASK_QUESTION_CREATED`·`TASK_ANSWER_CREATED` notices only wake the pull; their arrival
//! order or count is never used to infer events. Each page and its `nextCursor` are stored
//! in one private snapshot, so a failed write retries from the previous cursor and eventId
//! removes duplicates. Message bodies are never fetched or stored here: Agents read them with
//! `get_task_result`. Nothing here ACKs, accepts, answers or changes a task.
use crate::{
    central::{CentralState, ConnectionStatus, endpoint_url},
    config, private_file,
    protocol::TaskMessageType,
    task_client::{TaskClient, TaskError, central_error, numeric_id},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Retained arrival records across all scopes. At the limit half is retired, stale scopes
/// first; get_task_result stays authoritative for retired history.
pub const MAX_EVENTS: usize = 4096;
const EVICT_BATCH: usize = MAX_EVENTS / 2;
/// Central signs cursors up to 64 KiB; anything longer is a protocol error.
const MAX_CURSOR: usize = 64 * 1024;
const MAX_CREATED_AT: usize = 64;
const PAGE_LIMIT: usize = 100;
/// Pages per round. A longer backlog continues in the next round instead of starving others.
const MAX_PAGES: usize = 32;
const PATH: &str = "/api/v1/agents/me/task-messages";

/// One question or answer that arrived on a shared task. IDs and order only, no body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskMessage {
    pub event_id: String,
    pub task_id: u64,
    pub sequence: String,
    pub event_type: TaskMessageType,
    pub created_at: String,
}
impl TaskMessage {
    fn validate(&self) -> Result<()> {
        ensure!(
            numeric_id(&self.event_id).is_some()
                && numeric_id(&self.task_id.to_string()).is_some()
                && numeric_id(&self.sequence).is_some()
                && !self.created_at.trim().is_empty()
                && self.created_at.len() <= MAX_CREATED_AT,
            "Invalid task message"
        );
        Ok(())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Scope {
    origin: String,
    agent_id: u64,
    /// Central nextCursor saved with the events it covers. `None` restarts from the beginning.
    cursor: Option<String>,
    events: Vec<TaskMessage>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    schema_version: u32,
    scopes: Vec<Scope>,
}
fn empty() -> Snapshot {
    Snapshot {
        schema_version: 1,
        scopes: Vec::new(),
    }
}
fn cursor_valid(cursor: &str) -> bool {
    !cursor.is_empty()
        && cursor.len() <= MAX_CURSOR
        && cursor
            .bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'\\'))
}
fn read(path: &Path) -> Result<Snapshot> {
    let snapshot: Snapshot = private_file::read(path, MAX_BYTES)?.unwrap_or_else(empty);
    ensure!(
        snapshot.schema_version == 1,
        "Invalid task message snapshot"
    );
    let mut scopes = HashSet::new();
    let mut total = 0;
    for scope in &snapshot.scopes {
        ensure!(
            numeric_id(&scope.agent_id.to_string()).is_some()
                && endpoint_url(&scope.origin)?.as_str() == scope.origin
                && scope.cursor.as_deref().is_none_or(cursor_valid)
                && scopes.insert((&scope.origin, scope.agent_id)),
            "Invalid task message scope"
        );
        let mut seen = HashSet::new();
        for event in &scope.events {
            event.validate()?;
            ensure!(
                seen.insert(&event.event_id),
                "Duplicate stored task message"
            );
        }
        total += scope.events.len();
    }
    ensure!(total <= MAX_EVENTS, "Task message retention exceeded");
    Ok(snapshot)
}

/// Retire at least `amount` of the oldest records before a page is stored. Scopes other than
/// the current one (left by a re-registered Agent or an old origin) go first, largest first;
/// only then the current scope's oldest records. The page's own new records are pushed after
/// this, so they are never retired before anyone sees them. Returns the number retired.
fn evict(scopes: &mut [Scope], current: usize, amount: usize) -> usize {
    let mut order: Vec<usize> = (0..scopes.len()).filter(|i| *i != current).collect();
    order.sort_by_key(|i| std::cmp::Reverse(scopes[*i].events.len()));
    order.push(current);
    let mut freed = 0;
    for index in order {
        if freed >= amount {
            break;
        }
        let count = scopes[index].events.len().min(amount - freed);
        scopes[index].events.drain(..count);
        freed += count;
    }
    freed
}

/// Keep the encoded snapshot within `limit` by forgetting whole scopes, least recently
/// pulled first. Scopes are kept in pull order with the current one last, so the current
/// scope is never forgotten and every other cursor survives until the file would really
/// overflow. Only a forgotten scope restarts from the beginning if its Agent returns.
fn fit(snapshot: &mut Snapshot, limit: usize) -> Result<usize> {
    let mut size = serde_json::to_vec(&*snapshot)
        .context("Cannot encode task message snapshot")?
        .len();
    let mut forgotten = 0;
    while size > limit && snapshot.scopes.len() > 1 {
        let scope = snapshot.scopes.remove(0);
        // One array element plus its separating comma.
        size -= serde_json::to_vec(&scope)
            .context("Cannot encode task message scope")?
            .len()
            + 1;
        forgotten += 1;
    }
    Ok(forgotten)
}

#[derive(Default)]
struct Counters {
    rejected: AtomicU64,
    failed_pulls: AtomicU64,
    last_error: std::sync::Mutex<Option<&'static str>>,
}

/// Health counters. A rejection or failed pull means central may hold more than local data.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskMessageDiagnostics {
    /// Invalid notices plus arrivals dropped as conflicting replays.
    pub rejected_events: u64,
    pub failed_pulls: u64,
    pub last_error: Option<String>,
}

/// Private store plus the pull trigger shared by the central socket and the sync worker.
#[derive(Clone)]
pub struct TaskMessages {
    path: PathBuf,
    writer: Arc<tokio::sync::Mutex<()>>,
    stopped: Arc<AtomicBool>,
    wake: Arc<Notify>,
    counters: Arc<Counters>,
}

/// Outcome of storing one page: newly retained, dropped and retired (oldest) records, and
/// least recently pulled scopes forgotten to keep the file within its size limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub added: usize,
    pub dropped: usize,
    pub evicted: usize,
    pub forgotten_scopes: usize,
}

impl TaskMessages {
    pub fn path(config_path: &Path) -> PathBuf {
        config::state_directory(config_path).join("task-messages.json")
    }
    /// A corrupt snapshot is reported, never overwritten, and does not stop the broker.
    pub fn open(config_path: &Path) -> Result<Self> {
        let store = Self {
            path: Self::path(config_path),
            writer: Arc::new(tokio::sync::Mutex::new(())),
            stopped: Arc::new(AtomicBool::new(false)),
            wake: Arc::new(Notify::new()),
            counters: Arc::new(Counters::default()),
        };
        if read(&store.path).is_err() {
            store.fail("TASK_MESSAGE_STORE_INVALID");
        }
        Ok(store)
    }
    pub fn stop_accepting(&self) {
        self.stopped.store(true, Ordering::Release);
        // Release a worker waiting for a trigger so shutdown does not wait for the timer.
        self.wake.notify_one();
    }
    /// Request a pull. Signals arriving while one runs coalesce into one more round.
    pub fn signal(&self) {
        self.wake.notify_one();
    }
    /// Count an invalid or misrouted notice. It does not trigger a pull.
    pub fn reject(&self) {
        self.counters.rejected.fetch_add(1, Ordering::Relaxed);
    }
    fn fail(&self, code: &'static str) {
        self.counters.failed_pulls.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.counters.last_error.lock() {
            *last = Some(code);
        }
    }
    pub fn diagnostics(&self) -> TaskMessageDiagnostics {
        TaskMessageDiagnostics {
            rejected_events: self.counters.rejected.load(Ordering::Relaxed),
            failed_pulls: self.counters.failed_pulls.load(Ordering::Relaxed),
            last_error: self
                .counters
                .last_error
                .lock()
                .ok()
                .and_then(|last| last.map(str::to_owned)),
        }
    }

    /// Stored central cursor for this scope.
    pub async fn cursor(&self, origin: &str, agent_id: u64) -> Result<Option<String>> {
        let origin = endpoint_url(origin)?.to_string();
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            Ok(read(&path)?
                .scopes
                .into_iter()
                .find(|s| s.origin == origin && s.agent_id == agent_id)
                .and_then(|s| s.cursor))
        })
        .await
        .context("Task message worker failed")?
    }

    /// Store one validated page and its nextCursor in a single snapshot replace.
    /// Known eventIds are skipped; a conflicting replay is dropped and counted so the cursor
    /// still advances instead of pulling the same page forever. A full store retires half,
    /// stale scopes first, before the page is pushed, so every new arrival is kept.
    pub async fn apply(
        &self,
        origin: &str,
        agent_id: u64,
        items: Vec<TaskMessage>,
        next_cursor: String,
    ) -> Result<Applied> {
        let origin = endpoint_url(origin)?.to_string();
        ensure!(
            numeric_id(&agent_id.to_string()).is_some()
                && cursor_valid(&next_cursor)
                && items.len() <= PAGE_LIMIT,
            "Invalid task message page"
        );
        for item in &items {
            item.validate()?;
        }
        let permit = self.writer.clone().lock_owned().await;
        let store = self.clone();
        let applied = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ensure!(
                !store.stopped.load(Ordering::Acquire),
                "Task message store stopped"
            );
            let _disk = config::lock(&store.path.with_extension("lock"))?;
            let mut snapshot = read(&store.path)?;
            let total: usize = snapshot.scopes.iter().map(|s| s.events.len()).sum();
            // Keep scopes in pull order: the current one moves last, so `fit` forgets the
            // least recently pulled scope first. No scope count limit: an Agent re-registered
            // or pointed at a new server many times still syncs.
            let scope = match snapshot
                .scopes
                .iter()
                .position(|s| s.origin == origin && s.agent_id == agent_id)
            {
                Some(index) => snapshot.scopes.remove(index),
                None => Scope {
                    origin,
                    agent_id,
                    cursor: None,
                    events: Vec::new(),
                },
            };
            snapshot.scopes.push(scope);
            let index = snapshot.scopes.len() - 1;
            let mut applied = Applied::default();
            let fresh: HashSet<&str> = items
                .iter()
                .map(|item| item.event_id.as_str())
                .filter(|id| {
                    !snapshot.scopes[index]
                        .events
                        .iter()
                        .any(|e| e.event_id == *id)
                })
                .collect();
            if total + fresh.len() > MAX_EVENTS {
                // Retire half at once so feed cursors restart only once per batch.
                let amount = (total + fresh.len() - MAX_EVENTS).max(EVICT_BATCH);
                applied.evicted = evict(&mut snapshot.scopes, index, amount);
            }
            for item in items {
                let events = &mut snapshot.scopes[index].events;
                match events.iter().find(|e| e.event_id == item.event_id) {
                    Some(existing) if *existing == item => {}
                    Some(_) => applied.dropped += 1,
                    None => {
                        events.push(item);
                        applied.added += 1;
                    }
                }
            }
            snapshot.scopes[index].cursor = Some(next_cursor);
            applied.forgotten_scopes = fit(&mut snapshot, MAX_BYTES)?;
            ensure!(
                !store.stopped.load(Ordering::Acquire),
                "Task message store stopped"
            );
            private_file::write(&store.path, &snapshot, MAX_BYTES)?;
            Ok(applied)
        })
        .await
        .context("Task message worker failed")??;
        if applied.dropped > 0 {
            self.counters
                .rejected
                .fetch_add(applied.dropped as u64, Ordering::Relaxed);
        }
        Ok(applied)
    }

    /// Update-feed source in stored order. Read-only.
    pub fn events_config(
        config_path: &Path,
        origin: &str,
        agent_id: u64,
    ) -> Result<Vec<TaskMessage>> {
        let origin = endpoint_url(origin)?.to_string();
        Ok(read(&Self::path(config_path))?
            .scopes
            .into_iter()
            .find(|s| s.origin == origin && s.agent_id == agent_id)
            .map(|s| s.events)
            .unwrap_or_default())
    }

    /// Caller holds stopped-broker/control locks. Drops retained records but keeps each
    /// central cursor, so freed space fills with new arrivals instead of a full resync.
    pub fn clear_config(config_path: &Path) -> Result<()> {
        let path = Self::path(config_path);
        let _disk = config::lock(&path.with_extension("lock"))?;
        let mut snapshot = read(&path).unwrap_or_else(|_| empty());
        for scope in &mut snapshot.scopes {
            scope.events.clear();
        }
        private_file::write(&path, &snapshot, MAX_BYTES)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WirePage {
    items: Vec<WireItem>,
    next_cursor: String,
    has_next: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireItem {
    event_id: String,
    task_id: u64,
    sequence: String,
    event_type: TaskMessageType,
    created_at: String,
}

struct Page {
    items: Vec<TaskMessage>,
    next_cursor: String,
    has_next: bool,
}

fn protocol() -> TaskError {
    TaskError::unknown("CENTRAL_PROTOCOL_ERROR")
}

async fn fetch(client: &TaskClient, cursor: Option<&str>) -> Result<Page, TaskError> {
    let mut query = vec![("limit".to_owned(), PAGE_LIMIT.to_string())];
    if let Some(cursor) = cursor {
        query.push(("cursor".to_owned(), cursor.to_owned()));
    }
    let (status, envelope) = client
        .request_http(reqwest::Method::GET, PATH, &query, None)
        .await?;
    if status != 200 || !envelope.success {
        return Err(central_error(status, envelope.code.as_deref()));
    }
    let page: WirePage =
        serde_json::from_value(envelope.data.ok_or_else(protocol)?).map_err(|_| protocol())?;
    let items: Vec<TaskMessage> = page
        .items
        .into_iter()
        .map(|item| TaskMessage {
            event_id: item.event_id,
            task_id: item.task_id,
            sequence: item.sequence,
            event_type: item.event_type,
            created_at: item.created_at,
        })
        .collect();
    let mut ids = HashSet::new();
    if items.len() > PAGE_LIMIT
        || !cursor_valid(&page.next_cursor)
        || items
            .iter()
            .any(|item| item.validate().is_err() || !ids.insert(item.event_id.clone()))
        // hasNext without progress would spin; central always returns items before hasNext.
        || (page.has_next && items.is_empty())
    {
        return Err(protocol());
    }
    Ok(Page {
        items,
        next_cursor: page.next_cursor,
        has_next: page.has_next,
    })
}

/// Agent of the current Connected generation, if any.
async fn connected_agent(central: &CentralState) -> Option<u64> {
    central
        .with_ready_context(|status, identity, _| {
            (status == ConnectionStatus::Connected)
                .then(|| identity.map(|id| id.agent_id))
                .flatten()
        })
        .await
}

/// Background puller. Runs on each signal (READY, notice) and on a jittered safety timer
/// while connected; failures leave the stored cursor unchanged for the next round.
pub async fn run(
    store: TaskMessages,
    central: CentralState,
    client: TaskClient,
    origin: String,
    interval: Duration,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = store.wake.notified() => {}
            _ = tokio::time::sleep(jitter(interval)) => {}
        }
        if cancel.is_cancelled() || store.stopped.load(Ordering::Acquire) {
            return;
        }
        let Some(agent) = connected_agent(&central).await else {
            continue;
        };
        let round = tokio::select! {
            _ = cancel.cancelled() => return,
            round = sync_round(&store, &central, &client, &origin, agent) => round,
        };
        match round {
            Ok(true) => store.signal(),
            Ok(false) => {}
            Err(code) => store.fail(code),
        }
    }
}

/// ±1/6 of the interval (60s → 50–70s) so many brokers do not pull in lockstep.
fn jitter(interval: Duration) -> Duration {
    let millis = interval.as_millis().min(u64::MAX as u128) as u64;
    let spread = millis / 6;
    if spread == 0 {
        return interval;
    }
    let offset = (uuid::Uuid::new_v4().as_u128() % (2 * spread as u128 + 1)) as u64;
    Duration::from_millis(millis - spread + offset)
}

/// Read pages until hasNext=false. Returns `Ok(true)` when the page budget ran out.
async fn sync_round(
    store: &TaskMessages,
    central: &CentralState,
    client: &TaskClient,
    origin: &str,
    agent: u64,
) -> Result<bool, &'static str> {
    let mut cursor = store
        .cursor(origin, agent)
        .await
        .map_err(|_| "TASK_MESSAGE_STORE_UNAVAILABLE")?;
    let mut restarted = false;
    for _ in 0..MAX_PAGES {
        let page = match fetch(client, cursor.as_deref()).await {
            Ok(page) => page,
            // An old-version, rotated-key or other-scope cursor: resync from the beginning
            // once and rely on eventId deduplication.
            Err(error)
                if cursor.is_some()
                    && !restarted
                    && matches!(error.code, "VALIDATION_FAILED" | "FORBIDDEN") =>
            {
                restarted = true;
                cursor = None;
                continue;
            }
            Err(error) => return Err(error.code),
        };
        // The Bearer token is fixed, but store only under the identity still connected.
        // A plain disconnect is not a failure: the next READY pulls again from this cursor.
        match connected_agent(central).await {
            Some(current) if current == agent => {}
            Some(_) => return Err("CENTRAL_IDENTITY_CHANGED"),
            None => return Ok(false),
        }
        // A server that repeats the same cursor with hasNext would spin through the budget.
        if page.has_next && cursor.as_deref() == Some(page.next_cursor.as_str()) {
            return Err("CENTRAL_PROTOCOL_ERROR");
        }
        store
            .apply(origin, agent, page.items, page.next_cursor.clone())
            .await
            .map_err(|_| "TASK_MESSAGE_STORE_UNAVAILABLE")?;
        if !page.has_next {
            return Ok(false);
        }
        cursor = Some(page.next_cursor);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: u64, kind: TaskMessageType) -> TaskMessage {
        TaskMessage {
            event_id: id.to_string(),
            task_id: 100,
            sequence: id.to_string(),
            event_type: kind,
            created_at: "2026-11-03T05:30:00Z".into(),
        }
    }

    #[tokio::test]
    async fn pages_dedupe_by_event_id_and_keep_cursor_with_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = TaskMessages::open(&path).unwrap();
        let origin = "ws://127.0.0.1:9";
        assert_eq!(store.cursor(origin, 12).await.unwrap(), None);
        let first = store
            .apply(
                origin,
                12,
                vec![
                    message(1, TaskMessageType::Question),
                    message(2, TaskMessageType::Answer),
                ],
                "c1".into(),
            )
            .await
            .unwrap();
        assert_eq!(first.added, 2);
        // A replayed page (e.g. after resync) adds nothing; the cursor still advances.
        let replay = store
            .apply(
                "ws://127.0.0.1:9/ws/broker",
                12,
                vec![message(2, TaskMessageType::Answer)],
                "c2".into(),
            )
            .await
            .unwrap();
        assert_eq!(replay, Applied::default());
        assert_eq!(
            store.cursor(origin, 12).await.unwrap().as_deref(),
            Some("c2")
        );
        // Another Agent is a separate scope.
        assert_eq!(store.cursor(origin, 13).await.unwrap(), None);
        let mut conflict = message(1, TaskMessageType::Question);
        conflict.sequence = "9".into();
        let dropped = store
            .apply(origin, 12, vec![conflict], "c3".into())
            .await
            .unwrap();
        assert_eq!(dropped.dropped, 1);
        assert_eq!(store.diagnostics().rejected_events, 1);
        let events = TaskMessages::events_config(&path, origin, 12).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, "1");
        crate::credentials::verify_private_permissions(&TaskMessages::path(&path)).unwrap();
        TaskMessages::clear_config(&path).unwrap();
        assert!(
            TaskMessages::events_config(&path, origin, 12)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.cursor(origin, 12).await.unwrap().as_deref(),
            Some("c3")
        );
    }

    #[tokio::test]
    async fn retention_retires_oldest_half_stale_scope_first_and_keeps_corrupt_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let origin = endpoint_url("ws://127.0.0.1:9").unwrap();
        let full = Snapshot {
            schema_version: 1,
            scopes: vec![Scope {
                origin: origin.clone(),
                agent_id: 12,
                cursor: Some("c0".into()),
                events: (1..=MAX_EVENTS as u64)
                    .map(|id| message(id, TaskMessageType::Question))
                    .collect(),
            }],
        };
        private_file::write(&TaskMessages::path(&path), &full, MAX_BYTES).unwrap();
        let store = TaskMessages::open(&path).unwrap();
        let applied = store
            .apply(
                &origin,
                12,
                vec![message(MAX_EVENTS as u64 + 1, TaskMessageType::Answer)],
                "c1".into(),
            )
            .await
            .unwrap();
        // The oldest half retires at once; the new arrival is kept.
        assert_eq!(
            applied,
            Applied {
                added: 1,
                dropped: 0,
                evicted: EVICT_BATCH,
                forgotten_scopes: 0,
            }
        );
        let events = TaskMessages::events_config(&path, &origin, 12).unwrap();
        assert_eq!(events.len(), MAX_EVENTS - EVICT_BATCH + 1);
        assert_eq!(events[0].event_id, (EVICT_BATCH + 1).to_string());
        assert_eq!(
            events.last().unwrap().event_id,
            (MAX_EVENTS + 1).to_string()
        );
        assert_eq!(
            store.cursor(&origin, 12).await.unwrap().as_deref(),
            Some("c1")
        );
        assert_eq!(store.diagnostics().rejected_events, 0);
        // A stale scope (re-registered Agent) holding most slots is retired before the
        // small current one, so the current scope is not emptied on every arrival.
        let mut mixed = full;
        mixed.scopes[0].agent_id = 13;
        let current: Vec<TaskMessage> = mixed.scopes[0].events.drain(..96).collect();
        mixed.scopes.push(Scope {
            origin: origin.clone(),
            agent_id: 12,
            cursor: Some("c1".into()),
            events: current,
        });
        private_file::write(&TaskMessages::path(&path), &mixed, MAX_BYTES).unwrap();
        let applied = store
            .apply(
                &origin,
                12,
                vec![message(MAX_EVENTS as u64 + 2, TaskMessageType::Answer)],
                "c2".into(),
            )
            .await
            .unwrap();
        assert_eq!(
            applied,
            Applied {
                added: 1,
                dropped: 0,
                evicted: EVICT_BATCH,
                forgotten_scopes: 0,
            }
        );
        assert_eq!(
            TaskMessages::events_config(&path, &origin, 12)
                .unwrap()
                .len(),
            97
        );
        assert_eq!(
            TaskMessages::events_config(&path, &origin, 13)
                .unwrap()
                .len(),
            MAX_EVENTS - 96 - EVICT_BATCH
        );
        assert_eq!(store.diagnostics().rejected_events, 0);
        assert_eq!(
            store.cursor(&origin, 12).await.unwrap().as_deref(),
            Some("c2")
        );

        private_file::write(
            &TaskMessages::path(&path),
            &serde_json::json!({"secret":"internal"}),
            MAX_BYTES,
        )
        .unwrap();
        let before = std::fs::read(TaskMessages::path(&path)).unwrap();
        let store = TaskMessages::open(&path).unwrap();
        assert_eq!(
            store.diagnostics().last_error.as_deref(),
            Some("TASK_MESSAGE_STORE_INVALID")
        );
        let error = store
            .apply(&origin, 12, vec![], "c2".into())
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("internal"));
        assert_eq!(before, std::fs::read(TaskMessages::path(&path)).unwrap());
        store.stop_accepting();
    }

    #[test]
    fn eviction_clears_other_scopes_first_and_never_retires_the_new_page() {
        let scope = |agent_id, ids: std::ops::Range<u64>| Scope {
            origin: "ws://127.0.0.1:9/ws/broker".into(),
            agent_id,
            cursor: None,
            events: ids
                .map(|id| message(id, TaskMessageType::Question))
                .collect(),
        };
        // A small stale scope is emptied before the current one, even though it is smaller.
        let mut scopes = vec![scope(13, 1..101), scope(12, 101..4097)];
        assert_eq!(evict(&mut scopes, 1, EVICT_BATCH), EVICT_BATCH);
        assert!(scopes[0].events.is_empty());
        assert_eq!(scopes[1].events.len(), 4096 - 100 - (EVICT_BATCH - 100));
        assert_eq!(
            scopes[1].events[0].event_id,
            (100 + EVICT_BATCH - 100 + 1).to_string()
        );
        // Larger stale scopes go first.
        let mut scopes = vec![scope(14, 1..11), scope(12, 11..21), scope(13, 21..61)];
        assert_eq!(evict(&mut scopes, 1, 45), 45);
        assert_eq!(
            scopes.iter().map(|s| s.events.len()).collect::<Vec<_>>(),
            [5, 10, 0]
        );
    }

    #[tokio::test]
    async fn a_full_page_arriving_at_the_limit_is_kept_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let origin = endpoint_url("ws://127.0.0.1:9").unwrap();
        let mut stale = Scope {
            origin: origin.clone(),
            agent_id: 13,
            cursor: None,
            events: (1..=4090)
                .map(|id| message(id, TaskMessageType::Question))
                .collect(),
        };
        let current = Scope {
            origin: origin.clone(),
            agent_id: 12,
            cursor: None,
            events: stale.events.split_off(4000),
        };
        let snapshot = Snapshot {
            schema_version: 1,
            scopes: vec![stale, current],
        };
        private_file::write(&TaskMessages::path(&path), &snapshot, MAX_BYTES).unwrap();
        let store = TaskMessages::open(&path).unwrap();
        let page: Vec<_> = (5001..5001 + PAGE_LIMIT as u64)
            .map(|id| message(id, TaskMessageType::Answer))
            .collect();
        let applied = store.apply(&origin, 12, page, "c1".into()).await.unwrap();
        assert_eq!(applied.added, PAGE_LIMIT);
        assert_eq!(applied.evicted, EVICT_BATCH);
        let events = TaskMessages::events_config(&path, &origin, 12).unwrap();
        // The current scope keeps its old 90 and the whole new page; the stale scope paid.
        assert_eq!(events.len(), 90 + PAGE_LIMIT);
        assert_eq!(
            TaskMessages::events_config(&path, &origin, 13)
                .unwrap()
                .len(),
            4000 - EVICT_BATCH
        );
    }

    #[tokio::test]
    async fn new_scopes_past_sixteen_still_sync_and_keep_every_earlier_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let store = TaskMessages::open(&path).unwrap();
        let origin = endpoint_url("ws://127.0.0.1:9").unwrap();
        // Sixteen re-registered Agents (or test servers) leave empty pulls behind.
        for agent in 1..=16 {
            store
                .apply(&origin, agent, vec![], format!("old-{agent}"))
                .await
                .unwrap();
        }
        let other = endpoint_url("ws://127.0.0.1:10").unwrap();
        store
            .apply(&other, 1, vec![], "other".into())
            .await
            .unwrap();
        // The next Agent's arrivals are stored like any other.
        let applied = store
            .apply(
                &origin,
                17,
                vec![
                    message(1, TaskMessageType::Question),
                    message(2, TaskMessageType::Answer),
                ],
                "c1".into(),
            )
            .await
            .unwrap();
        assert_eq!(
            applied,
            Applied {
                added: 2,
                ..Applied::default()
            }
        );
        assert_eq!(
            TaskMessages::events_config(&path, &origin, 17)
                .unwrap()
                .len(),
            2
        );
        for agent in 1..=16 {
            assert_eq!(
                store.cursor(&origin, agent).await.unwrap(),
                Some(format!("old-{agent}"))
            );
        }
        assert_eq!(
            store.cursor(&other, 1).await.unwrap().as_deref(),
            Some("other")
        );
        // Scopes are kept in pull order: a pulled scope moves last.
        store
            .apply(&origin, 1, vec![], "old-1b".into())
            .await
            .unwrap();
        let order: Vec<_> = read(&TaskMessages::path(&path))
            .unwrap()
            .scopes
            .iter()
            .map(|s| (s.origin == origin, s.agent_id))
            .collect();
        assert_eq!(order.len(), 18);
        assert_eq!(order[0], (true, 2));
        assert_eq!(order[16..], [(true, 17), (true, 1)]);
    }

    #[test]
    fn size_limit_forgets_least_recently_pulled_scopes_never_the_current_one() {
        let scope = |agent_id, cursor: &str| Scope {
            origin: "ws://127.0.0.1:9/ws/broker".into(),
            agent_id,
            cursor: Some(cursor.into()),
            events: vec![message(agent_id, TaskMessageType::Question)],
        };
        let snapshot = || Snapshot {
            schema_version: 1,
            scopes: vec![
                scope(1, &"a".repeat(1000)),
                scope(2, "b"),
                scope(3, &"c".repeat(1000)),
            ],
        };
        let size = |s: &Snapshot| serde_json::to_vec(s).unwrap().len();
        let agents = |s: &Snapshot| s.scopes.iter().map(|s| s.agent_id).collect::<Vec<_>>();
        // Within the limit nothing is forgotten.
        let mut within = snapshot();
        let limit = size(&within);
        assert_eq!(fit(&mut within, limit).unwrap(), 0);
        assert_eq!(agents(&within), [1, 2, 3]);
        // One byte over forgets only the least recently pulled scope; the size math is exact.
        let mut over = snapshot();
        assert_eq!(fit(&mut over, limit - 1).unwrap(), 1);
        assert_eq!(agents(&over), [2, 3]);
        let mut exact = snapshot();
        exact.scopes.remove(0);
        assert_eq!(size(&over), size(&exact));
        // The running size is exact: a limit equal to the size without the first scope
        // forgets one, one byte less forgets two.
        assert_eq!(fit(&mut snapshot(), size(&exact)).unwrap(), 1);
        assert_eq!(fit(&mut snapshot(), size(&exact) - 1).unwrap(), 2);
        // However small the limit, the current (last) scope is kept.
        let mut tiny = snapshot();
        assert_eq!(fit(&mut tiny, 1).unwrap(), 2);
        assert_eq!(agents(&tiny), [3]);
    }

    #[test]
    fn cursor_and_jitter_bounds() {
        assert!(cursor_valid("H4sIAAAA.sig-_"));
        for invalid in ["", "a b", "a\"b", "a\\b", "가"] {
            assert!(!cursor_valid(invalid), "{invalid}");
        }
        assert!(!cursor_valid(&"x".repeat(MAX_CURSOR + 1)));
        for _ in 0..100 {
            let wait = jitter(Duration::from_secs(60));
            assert!((Duration::from_secs(50)..=Duration::from_secs(70)).contains(&wait));
        }
    }
}
