//! IDX-3-02 working-file collection (S15P31D108-186).
//!
//! Every 3 seconds the broker reads the bound project's uncommitted files. A changed list
//! replaces the bound user's list for that repository on central at once, so the dependency
//! tree shows work within seconds; an unchanged list is sent again every 30 seconds. An empty
//! list is still an observation; central expires an observation 90 seconds after it arrives.
//! `find_agent` asks for one more observation right before its central call and continues with
//! the last valid one if that fails. Bodies come from `git_scope` (IDX-3-03), so only the
//! branch, relative paths and change types leave the PC.
use crate::{
    agent_profile::InspectionBinding,
    central::{CancellationToken, CentralState},
    dependency_graph::{bound_ready, git::GitProject},
    git_scope::WorkingFiles,
    task_client::TaskClient,
};
use anyhow::{Result, anyhow, ensure};
use reqwest::Method;
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{Instant, MissedTickBehavior, interval, timeout},
};

/// How often the working tree is read; a changed list is sent at once.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(3);
/// An unchanged list is sent again after this long, well inside central's 90-second expiry.
pub const INTERVAL: Duration = Duration::from_secs(30);
/// Budget for the re-check right before `find_agent`; past it central uses the last observation.
pub const REFRESH_LIMIT: Duration = Duration::from_secs(3);
/// Remote whose URL hash identifies the repository when `dependencyRemote` is not configured.
pub const DEFAULT_REMOTE: &str = "origin";

/// The last observation central accepted from this collector.
struct Sent {
    body: Value,
    at: Instant,
}

struct Collector {
    git: GitProject,
    binding: InspectionBinding,
    central: CentralState,
    client: TaskClient,
    path: String,
    resend: Duration,
    last: Option<Sent>,
}

impl Collector {
    /// Reads the working tree and sends it unless it equals the last accepted list sent less
    /// than `resend` ago. `force` always sends. Returns whether a request was sent.
    async fn publish(&mut self, force: bool) -> Result<bool> {
        ensure!(
            bound_ready(&self.central, &self.binding).await,
            "Central is not ready for the bound Agent"
        );
        let (branch, files) = self.git.working().await?;
        // A branch name central would reject is reported like a detached HEAD.
        let branch = branch.filter(|name| WorkingFiles::branch_allowed(name));
        let body = serde_json::to_value(WorkingFiles::new(branch.as_deref(), files)?)?;
        if !force
            && self
                .last
                .as_ref()
                .is_some_and(|sent| sent.body == body && sent.at.elapsed() < self.resend)
        {
            return Ok(false);
        }
        let (status, reply) = self
            .client
            .request_graph_http(Method::PUT, &self.path, Some(body.clone()))
            .await
            .map_err(|_| anyhow!("Working-file upload failed"))?;
        ensure!(
            (200..300).contains(&status) && reply.success,
            "Working-file upload rejected"
        );
        self.last = Some(Sent {
            body,
            at: Instant::now(),
        });
        Ok(true)
    }
}

/// Handle `find_agent` uses for the re-check. Without a running collector it does nothing.
#[derive(Clone, Default)]
pub struct WorkingFileRefresher(Arc<OnceLock<mpsc::Sender<oneshot::Sender<bool>>>>);

impl WorkingFileRefresher {
    /// Publishes a fresh observation within `limit`. Never fails the caller; returns whether a
    /// new observation reached central.
    pub async fn refresh(&self, limit: Duration) -> bool {
        let Some(sender) = self.0.get() else {
            return false;
        };
        // 호출자 제한 시간은 응답 대기에만 적용하고 이미 시작한 발행은 수집기가 완료
        timeout(limit, async {
            let (reply, result) = oneshot::channel();
            if sender.send(reply).await.is_err() {
                return false;
            }
            result.await.unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }
}

pub(crate) async fn run(
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
    refresher: WorkingFileRefresher,
    cancel: CancellationToken,
) {
    let timing = (CHECK_INTERVAL, INTERVAL);
    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = collect(binding, remote, central, client, refresher, timing) => {},
    }
}

async fn collect(
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
    refresher: WorkingFileRefresher,
    (check, resend): (Duration, Duration),
) {
    let Ok(git) = GitProject::open(Path::new(&binding.project_root), &remote).await else {
        eprintln!(
            "Working-file collection is off: the bound project is not a Git repository with a supported '{remote}' remote."
        );
        return;
    };
    let path = format!(
        "/api/v1/teams/{}/git/working-files/{}",
        binding.team_id, git.key
    );
    let mut collector = Collector {
        git,
        binding,
        central,
        client,
        path,
        resend,
        last: None,
    };
    let (sender, mut requests) = mpsc::channel::<oneshot::Sender<bool>>(1);
    let _ = refresher.0.set(sender);
    let mut tick = interval(check);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut failed = false;
    loop {
        let reply = tokio::select! {
            _ = tick.tick() => None,
            Some(reply) = requests.recv() => {
                if reply.is_closed() {
                    continue;
                }
                Some(reply)
            }
        };
        if !bound_ready(&collector.central, &collector.binding).await {
            if let Some(reply) = reply {
                let _ = reply.send(false);
            }
            continue;
        }
        let result = collector.publish(reply.is_some()).await;
        if result.is_err() && !failed {
            eprintln!(
                "Working-file upload failed; central keeps the last observation until it expires."
            );
        }
        failed = result.is_err();
        if let Some(reply) = reply {
            let _ = reply.send(matches!(result, Ok(true)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::central::{CentralIdentity, ConnectionStatus};
    use axum::{
        Json, Router,
        extract::{Path as UrlPath, State},
        http::HeaderMap,
        routing::put,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{
        collections::BTreeSet,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::{Mutex, Notify, RwLock, mpsc};

    const TOKEN: &str = "agent-token";

    struct Upload {
        team: u64,
        key: String,
        authorization: Option<String>,
        body: Value,
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Repository with one modified, one added, one untracked and one deleted file.
    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["-c", "init.defaultBranch=main", "init", "-q"]);
        git(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/Acme/Demo.git",
            ],
        );
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.ts"), "a").unwrap();
        std::fs::write(root.join("gone.ts"), "g").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "init"]);
        std::fs::write(root.join("src/a.ts"), "a2").unwrap();
        std::fs::write(root.join("src/new.ts"), "n").unwrap();
        git(root, &["add", "src/new.ts"]);
        std::fs::write(root.join("notes.md"), "u").unwrap();
        std::fs::remove_file(root.join("gone.ts")).unwrap();
        dir
    }

    async fn replace(
        State(uploads): State<mpsc::UnboundedSender<Upload>>,
        UrlPath((team, key)): UrlPath<(u64, String)>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let authorization = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        uploads
            .send(Upload {
                team,
                key,
                authorization,
                body,
            })
            .unwrap();
        Json(json!({"success": true, "data": null}))
    }

    async fn central(uploads: mpsc::UnboundedSender<Upload>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/v1/teams/{team}/git/working-files/{key}", put(replace))
            .with_state(uploads);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("ws://{address}")
    }

    fn binding(root: &Path) -> InspectionBinding {
        InspectionBinding {
            origin: "http://127.0.0.1/".into(),
            user_id: 3,
            team_id: 7,
            agent_id: 12,
            project_root: root.to_str().unwrap().to_owned(),
            tool: crate::sessions::ToolType::Codex,
        }
    }

    async fn connected(user: u64) -> CentralState {
        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        let session = state.begin_session().await;
        assert!(session.set_status(ConnectionStatus::Connected).await);
        assert!(
            session
                .set_identity(CentralIdentity {
                    connection_id: "c".into(),
                    agent_id: 12,
                    user_id: Some(user),
                    team_id: Some(7),
                })
                .await
        );
        state
    }

    fn keys(value: &Value, found: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => map.iter().for_each(|(key, value)| {
                found.insert(key.clone());
                keys(value, found);
            }),
            Value::Array(items) => items.iter().for_each(|item| keys(item, found)),
            _ => {}
        }
    }

    #[tokio::test]
    async fn publishes_the_full_list_on_start_and_again_right_before_find_agent() {
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let url = central(sender).await;
        let refresher = WorkingFileRefresher::default();
        assert!(!refresher.refresh(REFRESH_LIMIT).await, "no collector yet");
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            binding(repo.path()),
            DEFAULT_REMOTE.into(),
            connected(3).await,
            TaskClient::new(&url, TOKEN).unwrap(),
            refresher.clone(),
            cancel.clone(),
        ));

        let first = tokio::time::timeout(Duration::from_secs(10), uploads.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.team, 7);
        assert_eq!(
            first.key,
            format!("{:x}", Sha256::digest(b"github.com/Acme/Demo")),
            "same repository key rule as the dependency graph and commit history"
        );
        assert_eq!(first.authorization.as_deref(), Some("Bearer agent-token"));
        assert_eq!(
            first.body,
            json!({"branch": "main", "files": [
                {"relativePath": "gone.ts", "changeType": "DELETED"},
                {"relativePath": "notes.md", "changeType": "UNTRACKED"},
                {"relativePath": "src/a.ts", "changeType": "MODIFIED"},
                {"relativePath": "src/new.ts", "changeType": "ADDED"}
            ]})
        );
        let mut found = BTreeSet::new();
        keys(&first.body, &mut found);
        assert_eq!(
            found,
            ["branch", "files", "relativePath", "changeType"]
                .map(String::from)
                .into()
        );
        let text = first.body.to_string();
        assert!(!text.contains(repo.path().to_str().unwrap()));
        assert!(!text.contains("github.com"));

        // Git accepts this branch, while central rejects its spacing combining mark.
        git(repo.path(), &["checkout", "-q", "-b", "feature/का"]);
        assert!(refresher.refresh(REFRESH_LIMIT).await);
        let observation = uploads.recv().await.unwrap();
        assert_eq!(observation.body["branch"], Value::Null);
        assert_eq!(observation.body["files"], first.body["files"]);

        // Clean tree: an empty list is still an observation.
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "work"]);
        assert!(refresher.refresh(REFRESH_LIMIT).await);
        let refreshed = uploads.recv().await.unwrap();
        assert_eq!(refreshed.body, json!({"branch": null, "files": []}));

        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn waits_for_the_bound_identity_and_skips_projects_without_a_remote() {
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let url = central(sender).await;
        let refresher = WorkingFileRefresher::default();
        let cancel = CancellationToken::new();
        // Central reports another user: nothing may be published as the bound Agent.
        let task = tokio::spawn(run(
            binding(repo.path()),
            DEFAULT_REMOTE.into(),
            connected(4).await,
            TaskClient::new(&url, TOKEN).unwrap(),
            refresher.clone(),
            cancel.clone(),
        ));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!refresher.refresh(REFRESH_LIMIT).await);
        assert!(uploads.try_recv().is_err());
        cancel.cancel();
        task.await.unwrap();

        let plain = tempfile::tempdir().unwrap();
        let refresher = WorkingFileRefresher::default();
        run(
            binding(plain.path()),
            DEFAULT_REMOTE.into(),
            connected(3).await,
            TaskClient::new(&url, TOKEN).unwrap(),
            refresher.clone(),
            CancellationToken::new(),
        )
        .await;
        assert!(!refresher.refresh(REFRESH_LIMIT).await);
        assert!(uploads.try_recv().is_err());
    }

    #[tokio::test]
    async fn sends_a_change_at_the_next_check_and_an_unchanged_list_only_after_the_interval() {
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let url = central(sender).await;
        let cancel = CancellationToken::new();
        let (check, resend) = (Duration::from_millis(100), Duration::from_secs(2));
        let collector = collect(
            binding(repo.path()),
            DEFAULT_REMOTE.into(),
            connected(3).await,
            TaskClient::new(&url, TOKEN).unwrap(),
            WorkingFileRefresher::default(),
            (check, resend),
        );
        let task = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                tokio::select! {
                    _ = cancel.cancelled() => {},
                    _ = collector => {},
                }
            }
        });
        let first = tokio::time::timeout(Duration::from_secs(10), uploads.recv())
            .await
            .unwrap()
            .unwrap();
        let sent_at = Instant::now();
        assert_eq!(first.body["files"].as_array().unwrap().len(), 4);

        // Unchanged lists are read every check but not sent again before the interval.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(uploads.try_recv().is_err());

        // A change goes out at the next check, long before the interval.
        std::fs::write(repo.path().join("later.ts"), "l").unwrap();
        let changed = tokio::time::timeout(Duration::from_millis(1_000), uploads.recv())
            .await
            .expect("a change must not wait for the interval")
            .unwrap();
        assert!(sent_at.elapsed() < resend);
        let files = changed.body["files"].as_array().unwrap();
        assert_eq!(files.len(), 5);
        assert!(files.contains(&json!({"relativePath": "later.ts", "changeType": "UNTRACKED"})));

        // Still unchanged: the same list is sent again once the interval passes.
        let changed_at = Instant::now();
        let again = tokio::time::timeout(Duration::from_secs(5), uploads.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(changed_at.elapsed() >= resend - check * 2);
        assert_eq!(again.body, changed.body);

        cancel.cancel();
        task.await.unwrap();
    }

    struct DelayedServer {
        count: AtomicUsize,
        arrivals: mpsc::UnboundedSender<Value>,
        release: Notify,
        latest: Mutex<Option<Value>>,
    }

    async fn delayed_replace(
        State(server): State<Arc<DelayedServer>>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        let index = server.count.fetch_add(1, Ordering::SeqCst);
        server.arrivals.send(body.clone()).unwrap();
        if index == 1 {
            server.release.notified().await;
        }
        *server.latest.lock().await = Some(body);
        Json(json!({"success": true, "data": null}))
    }

    async fn cancelled_refresh_preserves_upload_order(abort_caller: bool) {
        let repo = repository();
        let (sender, mut arrivals) = mpsc::unbounded_channel();
        let remote = Arc::new(DelayedServer {
            count: AtomicUsize::new(0),
            arrivals: sender,
            release: Notify::new(),
            latest: Mutex::new(None),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route(
                "/api/v1/teams/{team}/git/working-files/{key}",
                put(delayed_replace),
            )
            .with_state(remote.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let refresher = WorkingFileRefresher::default();
        let cancel = CancellationToken::new();
        let worker = tokio::spawn({
            let cancel = cancel.clone();
            let refresher = refresher.clone();
            let binding = binding(repo.path());
            let state = connected(3).await;
            async move {
                tokio::select! {
                    _ = cancel.cancelled() => {},
                    _ = collect(
                        binding, DEFAULT_REMOTE.into(), state,
                        TaskClient::new(&url, TOKEN).unwrap(), refresher,
                        (Duration::from_secs(60), INTERVAL),
                    ) => {},
                }
            }
        });
        timeout(Duration::from_secs(10), arrivals.recv())
            .await
            .unwrap()
            .unwrap();
        std::fs::write(repo.path().join("slow.ts"), "slow").unwrap();
        let old = tokio::spawn({
            let refresher = refresher.clone();
            async move {
                refresher
                    .refresh(if abort_caller {
                        Duration::from_secs(10)
                    } else {
                        Duration::from_millis(50)
                    })
                    .await
            }
        });
        let older_body = timeout(Duration::from_secs(10), arrivals.recv())
            .await
            .unwrap()
            .unwrap();
        if abort_caller {
            old.abort();
            assert!(old.await.unwrap_err().is_cancelled());
        } else {
            assert!(!old.await.unwrap());
        }
        // 시작하지 못한 만료 요청은 나중에 Git 조회나 업로드를 실행하지 않음
        assert!(!refresher.refresh(Duration::from_millis(20)).await);
        std::fs::write(repo.path().join("newest.ts"), "new").unwrap();
        let newest = tokio::spawn({
            let refresher = refresher.clone();
            async move { refresher.refresh(Duration::from_secs(5)).await }
        });
        assert!(
            timeout(Duration::from_millis(100), arrivals.recv())
                .await
                .is_err(),
            "the older upload must finish before another observation starts"
        );
        remote.release.notify_one();
        let newer_body = timeout(Duration::from_secs(5), arrivals.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(newest.await.unwrap());
        assert_ne!(newer_body, older_body);
        assert!(
            newer_body["files"]
                .as_array()
                .unwrap()
                .contains(&json!({"relativePath": "newest.ts", "changeType": "UNTRACKED"}))
        );
        assert_eq!(*remote.latest.lock().await, Some(newer_body));
        assert_eq!(remote.count.load(Ordering::SeqCst), 3);
        cancel.cancel();
        worker.await.unwrap();
        assert!(!refresher.refresh(Duration::from_millis(50)).await);
        server.abort();
    }

    #[tokio::test]
    async fn refresh_deadline_does_not_cancel_or_overtake_an_inflight_upload() {
        cancelled_refresh_preserves_upload_order(false).await;
    }

    #[tokio::test]
    async fn aborted_caller_and_expired_queue_do_not_cancel_or_overtake_an_upload() {
        cancelled_refresh_preserves_upload_order(true).await;
    }
}
