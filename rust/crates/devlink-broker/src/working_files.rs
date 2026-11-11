//! IDX-3-02 working-file collection (S15P31D108-186).
//!
//! Every 30 seconds the broker reads the bound project's uncommitted files and replaces the
//! bound user's list for that repository on central. An empty list is still an observation;
//! central expires an observation 90 seconds after it arrives.
//! `find_agent` asks for one more observation right before its central call and continues with
//! the last valid one if that fails. Bodies come from `git_scope` (IDX-3-03), so only the
//! branch, relative paths and change types leave the PC. Uploads are conditional on central's
//! revision (protocol 2) so a delayed older request cannot replace a newer list.
use crate::{
    agent_profile::InspectionBinding,
    central::{CancellationToken, CentralState},
    dependency_graph::{bound_ready, git::GitProject},
    git_scope::WorkingFiles,
    task_client::TaskClient,
};
use anyhow::{Context, Result, anyhow, ensure};
use reqwest::Method;
use serde_json::Value;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    time::{MissedTickBehavior, interval, timeout},
};
use uuid::Uuid;

/// The periodic observation interval, inside central's 90-second expiry.
pub const INTERVAL: Duration = Duration::from_secs(30);
/// Budget for the re-check right before `find_agent`; past it central uses the last observation.
pub const REFRESH_LIMIT: Duration = Duration::from_secs(3);
/// Remote whose URL hash identifies the repository when `dependencyRemote` is not configured.
pub const DEFAULT_REMOTE: &str = "origin";

/// Central working-file protocol with revision-conditional uploads.
const PROTOCOL_VERSION: u64 = 2;

struct Collector {
    git: GitProject,
    binding: InspectionBinding,
    central: CentralState,
    client: TaskClient,
    path: String,
    /// The current list was cut to central's limit and that was already reported.
    cut_noted: bool,
}

impl Collector {
    /// Every upload carries the revision central reported right before the Git read, so a
    /// delayed older request is rejected instead of replacing a newer list (F8). After a
    /// conflict the revision and the working tree are read again once.
    async fn publish(&mut self) -> Result<()> {
        ensure!(
            self.attempt().await? || self.attempt().await?,
            "Working-file revision changed again; retrying later"
        );
        Ok(())
    }

    /// Returns false on a revision conflict and true on a protocol 2 acknowledgment.
    async fn attempt(&mut self) -> Result<bool> {
        ensure!(
            bound_ready(&self.central, &self.binding).await,
            "Central is not ready for the bound Agent"
        );
        // Every upload looks the revision up first instead of reusing an acknowledged one. A
        // central without revisions, such as one rolled back to an older release, fails the
        // lookup and so never receives a PUT whose `If-Match` it would ignore. The body is read
        // after the lookup, so any list central accepted after that read invalidates it.
        let revision = self.fetch_revision().await?;
        let body = self.observe().await?;
        let (status, reply) = self
            .client
            .request_graph_http_if_match(Method::PUT, &self.path, Some(&revision), Some(body))
            .await
            .map_err(|_| anyhow!("Working-file upload failed"))?;
        if status == 412 {
            return Ok(false);
        }
        ensure!(
            (200..300).contains(&status) && reply.success,
            "Working-file upload rejected"
        );
        // Only a protocol 2 acknowledgment counts as accepted.
        revision_of(reply.data.as_ref())?;
        Ok(true)
    }

    /// The current working tree as an upload body.
    async fn observe(&mut self) -> Result<Value> {
        let (branch, files, omitted) = self.git.working().await?;
        // Over central's limit the list keeps tracked changes first (see `GitProject::working`).
        // Say so once per stretch, since it usually means a missing `.gitignore` entry.
        if omitted > 0 && !self.cut_noted {
            eprintln!(
                "More than {} changed files: {omitted} are left out of the working-file upload, untracked files first. Check .gitignore.",
                crate::git_scope::MAX_WORKING_FILES
            );
        }
        self.cut_noted = omitted > 0;
        // A branch name central would reject is reported like a detached HEAD.
        let branch = branch.filter(|name| WorkingFiles::branch_allowed(name));
        Ok(serde_json::to_value(WorkingFiles::new(
            branch.as_deref(),
            files,
        )?)?)
    }

    /// Central's current revision for the bound user and repository. A server without
    /// conditional uploads is an error; the collector never falls back to unprotected uploads.
    async fn fetch_revision(&self) -> Result<String> {
        let (status, reply) = self
            .client
            .request_graph_http(Method::GET, &self.path, None)
            .await
            .map_err(|_| anyhow!("Working-file revision lookup failed"))?;
        ensure!(
            status == 200 && reply.success,
            "Central does not offer working-file revisions"
        );
        revision_of(reply.data.as_ref())
    }
}

/// `{"protocolVersion": 2, "revision": "<uuid>"}` from a revision lookup or an acknowledgment.
fn revision_of(data: Option<&Value>) -> Result<String> {
    let data = data.context("Central did not return a working-file revision")?;
    ensure!(
        data["protocolVersion"].as_u64() == Some(PROTOCOL_VERSION),
        "Unsupported working-file protocol"
    );
    let revision = data["revision"]
        .as_str()
        .and_then(|revision| Uuid::try_parse(revision).ok())
        .context("Invalid working-file revision")?;
    Ok(revision.hyphenated().to_string())
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
    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = collect(binding, remote, central, client, refresher, INTERVAL) => {},
    }
}

async fn collect(
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
    refresher: WorkingFileRefresher,
    period: Duration,
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
        cut_noted: false,
    };
    let (sender, mut requests) = mpsc::channel::<oneshot::Sender<bool>>(1);
    let _ = refresher.0.set(sender);
    let mut tick = interval(period);
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
        let result = collector.publish().await;
        if result.is_err() && !failed {
            eprintln!(
                "Working-file upload failed; central keeps the last observation until it expires."
            );
        }
        failed = result.is_err();
        if let Some(reply) = reply {
            let _ = reply.send(result.is_ok());
        } else if failed {
            // Wait a full period after a failed periodic attempt instead of catching up.
            tick.reset();
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
        routing::{get, put},
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
        if_match: Option<String>,
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

    const ROUTE: &str = "/api/v1/teams/{team}/git/working-files/{key}";

    /// Central fake for the revision protocol (protocolVersion 2). A PUT whose `If-Match`
    /// names the current revision stores the list and rotates the revision; any other PUT gets
    /// 412 and changes nothing. `log` records each request in arrival order.
    struct Central {
        uploads: mpsc::UnboundedSender<Upload>,
        revision: std::sync::Mutex<String>,
        log: std::sync::Mutex<Vec<String>>,
        /// When each lookup arrived.
        lookup_times: std::sync::Mutex<Vec<std::time::Instant>>,
        /// Another writer moves the revision right after the next lookup is answered.
        race_next_lookup: std::sync::atomic::AtomicBool,
        /// Written into the working tree before the next 412 is returned.
        write_on_conflict: std::sync::Mutex<Option<std::path::PathBuf>>,
        /// Rolled back to a release before revisions: no lookup, and a PUT is stored without
        /// looking at `If-Match` and acknowledged with `data: null`.
        legacy: std::sync::atomic::AtomicBool,
    }

    impl Central {
        fn new(uploads: mpsc::UnboundedSender<Upload>) -> Arc<Self> {
            Arc::new(Self {
                uploads,
                revision: std::sync::Mutex::new(Uuid::new_v4().to_string()),
                log: std::sync::Mutex::default(),
                lookup_times: std::sync::Mutex::default(),
                race_next_lookup: Default::default(),
                write_on_conflict: Default::default(),
                legacy: Default::default(),
            })
        }

        fn current(&self) -> String {
            self.revision.lock().unwrap().clone()
        }

        /// Another writer for the same user and repository moved the revision.
        fn rotate(&self) -> String {
            let next = Uuid::new_v4().to_string();
            *self.revision.lock().unwrap() = next.clone();
            next
        }

        fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }
    }

    fn revision_body(revision: &str) -> Value {
        json!({"success": true, "data": {"protocolVersion": 2, "revision": revision}})
    }

    async fn lookup(State(central): State<Arc<Central>>) -> axum::response::Response {
        use axum::response::IntoResponse;
        central
            .lookup_times
            .lock()
            .unwrap()
            .push(std::time::Instant::now());
        if central.legacy.load(Ordering::SeqCst) {
            central.log.lock().unwrap().push("GET 405".into());
            return axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response();
        }
        central.log.lock().unwrap().push("GET".into());
        let seen = central.current();
        if central.race_next_lookup.swap(false, Ordering::SeqCst) {
            central.rotate();
        }
        Json(revision_body(&seen)).into_response()
    }

    async fn replace(
        State(central): State<Arc<Central>>,
        UrlPath((team, key)): UrlPath<(u64, String)>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        let header = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        let if_match = header("if-match");
        if central.legacy.load(Ordering::SeqCst) {
            central.log.lock().unwrap().push("PUT legacy".into());
            let _ = central.uploads.send(Upload {
                team,
                key,
                authorization: header("authorization"),
                if_match,
                body,
            });
            return Json(json!({"success": true, "data": null})).into_response();
        }
        let accepted = {
            let mut revision = central.revision.lock().unwrap();
            let matches = if_match.as_deref() == Some(format!("\"{revision}\"").as_str());
            central.log.lock().unwrap().push(format!(
                "PUT {} {}",
                if_match.as_deref().unwrap_or("-"),
                if matches { 200 } else { 412 }
            ));
            matches.then(|| {
                *revision = Uuid::new_v4().to_string();
                revision.clone()
            })
        };
        let Some(next) = accepted else {
            if let Some(path) = central.write_on_conflict.lock().unwrap().take() {
                std::fs::write(path, "changed before the conflict reply").unwrap();
            }
            return (
                axum::http::StatusCode::PRECONDITION_FAILED,
                Json(json!({
                    "success": false, "data": null,
                    "code": "WORKING_FILES_REVISION_MISMATCH", "message": "changed"
                })),
            )
                .into_response();
        };
        let _ = central.uploads.send(Upload {
            team,
            key,
            authorization: header("authorization"),
            if_match,
            body,
        });
        Json(revision_body(&next)).into_response()
    }

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("ws://{address}")
    }

    async fn central_with(state: Arc<Central>) -> String {
        serve(
            Router::new()
                .route(ROUTE, get(lookup).put(replace))
                .with_state(state),
        )
        .await
    }

    async fn central(uploads: mpsc::UnboundedSender<Upload>) -> String {
        central_with(Central::new(uploads)).await
    }

    async fn collector(root: &Path, url: &str) -> Collector {
        let git = GitProject::open(root, DEFAULT_REMOTE).await.unwrap();
        let path = format!("/api/v1/teams/7/git/working-files/{}", git.key);
        Collector {
            git,
            binding: binding(root),
            central: connected(3).await,
            client: TaskClient::new(url, TOKEN).unwrap(),
            path,
            cut_noted: false,
        }
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
        let if_match = first.if_match.as_deref().unwrap();
        assert!(Uuid::try_parse(&if_match[1..if_match.len() - 1]).is_ok());
        assert!(if_match.starts_with('"') && if_match.ends_with('"'));
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

        std::fs::write(repo.path().join(".git/HEAD"), b"ref: refs/heads/bad-\xff\n").unwrap();
        std::fs::write(repo.path().join("new-note.txt"), "note").unwrap();
        assert!(refresher.refresh(REFRESH_LIMIT).await);
        let observation = uploads.recv().await.unwrap();
        assert_eq!(observation.body["branch"], Value::Null);
        assert!(
            observation.body["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["relativePath"] == "new-note.txt")
        );

        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn periodically_resends_a_reverted_list_after_the_new_list_loses_its_ack() {
        use axum::body::Body;
        use axum::response::{IntoResponse, Response};

        async fn accept(
            state: State<Arc<Central>>,
            path: UrlPath<(u64, String)>,
            headers: HeaderMap,
            Json(body): Json<Value>,
        ) -> Response {
            let lose_ack = body["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["relativePath"] == "transient.txt");
            let response = replace(state, path, headers, Json(body)).await;
            if lose_ack && response.status().is_success() {
                let lost = std::io::Error::other("response lost after save");
                Response::new(Body::from_stream(futures_util::stream::once(async {
                    Err::<String, _>(lost)
                })))
            } else {
                response.into_response()
            }
        }
        let repo = repository();
        let root = repo.path();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let state = Central::new(sender);
        let url = serve(
            Router::new()
                .route(ROUTE, get(lookup).put(accept))
                .with_state(state.clone()),
        )
        .await;
        let mut collector = collector(root, &url).await;

        collector.publish().await.unwrap();
        let transient = root.join("transient.txt");
        std::fs::write(&transient, "temporary").unwrap();
        assert!(collector.publish().await.is_err());
        std::fs::remove_file(transient).unwrap();
        collector.publish().await.unwrap();
        let observations: Vec<Value> = std::iter::from_fn(|| uploads.try_recv().ok())
            .map(|upload| upload.body)
            .collect();
        assert_eq!(observations.len(), 3);
        assert_ne!(observations[0], observations[1]);
        assert_eq!(observations[0], observations[2]);
        // Every upload, including the one after the lost acknowledgment, looked the revision up
        // right before it and was accepted.
        assert_eq!(
            state
                .log()
                .iter()
                .map(|line| &line[..3])
                .collect::<Vec<_>>(),
            ["GET", "PUT", "GET", "PUT", "GET", "PUT"]
        );
        assert!(
            state
                .log()
                .iter()
                .all(|line| line == "GET" || line.ends_with(" 200"))
        );
    }

    #[tokio::test]
    async fn a_conflict_reads_a_new_revision_and_the_tree_again_before_one_retry() {
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let state = Central::new(sender);
        let url = central_with(state.clone()).await;
        let mut collector = collector(repo.path(), &url).await;
        let initial = state.current();

        collector.publish().await.unwrap();
        let first = uploads.recv().await.unwrap();
        assert_eq!(first.if_match, Some(format!("\"{initial}\"")));

        // Another writer moves the revision between the lookup and the upload, and the working
        // tree changes after the rejected body was read, before the 412 arrives.
        std::fs::write(repo.path().join("next.ts"), "n").unwrap();
        let looked_up = state.current();
        state.race_next_lookup.store(true, Ordering::SeqCst);
        *state.write_on_conflict.lock().unwrap() = Some(repo.path().join("late.ts"));
        collector.publish().await.unwrap();
        let retried = uploads.recv().await.unwrap();
        let log = state.log();
        assert_eq!(log.len(), 6, "{log:?}");
        assert_eq!(log[2], "GET");
        assert_eq!(log[3], format!("PUT \"{looked_up}\" 412"));
        assert_eq!(log[4], "GET");
        assert!(log[5].ends_with(" 200"));
        assert_ne!(retried.if_match, Some(format!("\"{looked_up}\"")));
        // The retry read the tree again: it carries the file written before the 412 reply.
        let files = retried.body["files"].as_array().unwrap();
        assert!(files.contains(&json!({"relativePath": "next.ts", "changeType": "UNTRACKED"})));
        assert!(files.contains(&json!({"relativePath": "late.ts", "changeType": "UNTRACKED"})));
    }

    #[tokio::test]
    async fn a_rollback_to_a_central_without_revisions_stops_uploads_before_any_put() {
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let state = Central::new(sender);
        let url = central_with(state.clone()).await;
        let mut collector = collector(repo.path(), &url).await;
        collector.publish().await.unwrap();
        uploads.recv().await.unwrap();

        // Central is rolled back while the collector keeps running; the tree changes and the
        // periodic upload is due as well.
        state.legacy.store(true, Ordering::SeqCst);
        std::fs::write(repo.path().join("after-rollback.ts"), "r").unwrap();
        assert!(collector.publish().await.is_err());
        assert!(collector.publish().await.is_err());
        assert!(
            !state.log().iter().any(|line| line == "PUT legacy"),
            "{:?}",
            state.log()
        );
        assert!(uploads.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_second_conflict_fails_the_publish_after_one_retry() {
        // Another writer always wins right after each lookup.
        async fn racing(State(central): State<Arc<Central>>) -> Json<Value> {
            central.log.lock().unwrap().push("GET".into());
            let seen = central.current();
            central.rotate();
            Json(revision_body(&seen))
        }
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let state = Central::new(sender);
        let url = serve(
            Router::new()
                .route(ROUTE, get(racing).put(replace))
                .with_state(state.clone()),
        )
        .await;
        let mut collector = collector(repo.path(), &url).await;

        assert!(collector.publish().await.is_err());
        let log = state.log();
        assert_eq!(log.len(), 4, "{log:?}");
        assert_eq!(log[0], "GET");
        assert!(log[1].ends_with(" 412"));
        assert_eq!(log[2], "GET");
        assert!(log[3].ends_with(" 412"));
        assert!(uploads.try_recv().is_err());
    }

    #[tokio::test]
    async fn find_agent_route_uploads_the_current_working_files_before_asking_central() {
        const LOCAL: &str = "local-mcp-token";
        async fn me() -> Json<Value> {
            Json(
                json!({"success":true,"data":{"agentId":12,"teamId":7,"ownerUserId":3,"name":"Me",
                "roleDescription":null,"toolType":"CODEX","projectName":"Demo","status":"ONLINE",
                "lastSeenAt":null,"createdAt":"2026-10-29T00:00:00"}}),
            )
        }
        async fn recommend(State(central): State<Arc<Central>>) -> Json<Value> {
            central.log.lock().unwrap().push("RECOMMEND".into());
            Json(
                json!({"success":true,"data":{"status":"INSUFFICIENT_EVIDENCE",
                "gitCoverage":{"state":"AVAILABLE","reason":null,"workingState":"FRESH"},
                "evaluatedAt":"2026-11-11T00:00:00Z","candidates":[],
                "insufficientReason":"NO_ELIGIBLE_AGENTS"}}),
            )
        }
        let repo = repository();
        let profile = tempfile::tempdir().unwrap();
        let config = profile.path().join("config.json");
        crate::config::init(&config).unwrap();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let state = Central::new(sender);
        let central_url = serve(
            Router::new()
                .route(ROUTE, get(lookup).put(replace))
                .route("/api/v1/agents/me", get(me))
                .route(
                    "/api/v1/teams/{team}/agents/recommendations",
                    axum::routing::post(recommend),
                )
                .with_state(state.clone()),
        )
        .await;
        let central = connected(3).await;
        let client = TaskClient::new(&central_url, TOKEN).unwrap();
        let refresher = WorkingFileRefresher::default();
        let cancel = CancellationToken::new();

        // The collector checks only every minute, so the second upload below can only come from
        // the re-check of the find_agent route.
        let collector = collect(
            binding(repo.path()),
            DEFAULT_REMOTE.into(),
            central.clone(),
            client.clone(),
            refresher.clone(),
            Duration::from_secs(60),
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
        timeout(Duration::from_secs(10), uploads.recv())
            .await
            .unwrap()
            .unwrap();

        let sessions = crate::sessions::SessionManager::new(
            Arc::new(crate::sessions::ReadyIdentityProvider::new(central.clone())),
            central.clone(),
        );
        let context = crate::context_api::ContextReader::new(
            central.clone(),
            sessions.clone(),
            config.clone(),
        )
        .unwrap();
        let token = Arc::new(LOCAL.to_owned());
        let app = crate::session_api::router(token.clone(), sessions.clone()).merge(
            crate::agent_api::router_with_working_files(
                token,
                sessions,
                client,
                cancel.clone(),
                context,
                refresher,
            ),
        );
        let broker = serve(app).await.replace("ws://", "http://");
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let post = |path: &str, body: Value| {
            http.post(format!("{broker}{path}"))
                .bearer_auth(LOCAL)
                .json(&body)
                .send()
        };
        let session: Value = post(
            "/v1/sessions",
            json!({"requestId":"claim","instanceId":"host","mode":"fresh","agentId":"12"}),
        )
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        let mut context = session["data"]["principal"].clone();
        context["sessionId"] = session["data"]["sessionId"].clone();

        std::fs::write(repo.path().join("for-find.ts"), "f").unwrap();
        let before = state.log().len();
        let response = post(
            "/v1/agents/find",
            json!({"requestId":Uuid::new_v4().to_string(),"mcpRequestId":7,
                "operation":"find_agent","arguments":{"question":"로그인 오류"},"context":context}),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), 200, "{:?}", response.text().await);
        let log = state.log();
        assert_eq!(
            log[before..]
                .iter()
                .map(|line| &line[..3])
                .collect::<Vec<_>>(),
            ["GET", "PUT", "REC"],
            "{log:?}"
        );
        let refreshed = uploads.try_recv().unwrap();
        assert!(
            refreshed.body["files"]
                .as_array()
                .unwrap()
                .contains(&json!({"relativePath": "for-find.ts", "changeType": "UNTRACKED"}))
        );
        cancel.cancel();
        task.await.unwrap();
    }

    fn lookups(state: &Central) -> usize {
        state
            .log()
            .iter()
            .filter(|line| line.starts_with("GET"))
            .count()
    }

    #[tokio::test]
    async fn a_failed_periodic_attempt_waits_for_the_next_period() {
        let repo = repository();
        let (sender, _uploads) = mpsc::unbounded_channel();
        let state = Central::new(sender);
        state.legacy.store(true, Ordering::SeqCst);
        let url = central_with(state.clone()).await;
        let period = Duration::from_secs(1);
        let worker = tokio::spawn(collect(
            binding(repo.path()),
            DEFAULT_REMOTE.into(),
            connected(3).await,
            TaskClient::new(&url, TOKEN).unwrap(),
            WorkingFileRefresher::default(),
            period,
        ));
        timeout(Duration::from_secs(10), async {
            while lookups(&state) < 3 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("three revision lookups");
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        let times = state.lookup_times.lock().unwrap();
        assert!(times[1] - times[0] >= period);
        assert!(times[2] - times[1] >= period);
    }

    #[tokio::test]
    async fn central_without_revisions_never_receives_an_unprotected_upload() {
        async fn old_revision(State(central): State<Arc<Central>>) -> Json<Value> {
            central.log.lock().unwrap().push("GET".into());
            Json(
                json!({"success": true, "data": {"protocolVersion": 1, "revision": central.current()}}),
            )
        }
        async fn no_revision(State(central): State<Arc<Central>>) -> Json<Value> {
            central.log.lock().unwrap().push("GET".into());
            Json(json!({"success": true, "data": null}))
        }
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let state = Central::new(sender);
        let servers = [
            // An older central has only the PUT route.
            Router::new().route(ROUTE, put(replace)),
            Router::new().route(ROUTE, get(old_revision).put(replace)),
            Router::new().route(ROUTE, get(no_revision).put(replace)),
        ];
        for app in servers {
            let url = serve(app.with_state(state.clone())).await;
            let mut collector = collector(repo.path(), &url).await;
            assert!(collector.publish().await.is_err());
        }
        assert!(!state.log().iter().any(|line| line.starts_with("PUT")));
        assert!(uploads.try_recv().is_err());
    }

    #[tokio::test]
    async fn too_many_changes_upload_the_tracked_changes_and_fill_up_with_untracked_files() {
        let repo = repository();
        let modules = repo.path().join("node_modules");
        std::fs::create_dir(&modules).unwrap();
        for i in 0..crate::git_scope::MAX_WORKING_FILES + 5 {
            std::fs::write(modules.join(format!("m{i:05}.js")), "").unwrap();
        }
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let url = central(sender).await;
        let mut collector = collector(repo.path(), &url).await;

        collector.publish().await.unwrap();
        assert!(collector.cut_noted);
        let files = uploads.recv().await.unwrap().body["files"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(files.len(), crate::git_scope::MAX_WORKING_FILES);
        // The real work in the fixture repository survives the cut.
        for change in [
            json!({"relativePath": "gone.ts", "changeType": "DELETED"}),
            json!({"relativePath": "src/a.ts", "changeType": "MODIFIED"}),
            json!({"relativePath": "src/new.ts", "changeType": "ADDED"}),
        ] {
            assert!(files.contains(&change), "{change}");
        }

        // Back under the limit, the whole list is sent again and the notice resets.
        std::fs::remove_dir_all(&modules).unwrap();
        collector.publish().await.unwrap();
        assert!(!collector.cut_noted);
        assert_eq!(
            uploads.recv().await.unwrap().body["files"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
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
        // Wait until the collector is running, so the refusal below comes from the identity
        // check and not from a collector that has not started yet.
        timeout(Duration::from_secs(10), async {
            while refresher.0.get().is_none() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the collector registers its re-check channel");
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
    async fn each_period_replaces_the_list_even_when_unchanged() {
        let repo = repository();
        let (sender, mut uploads) = mpsc::unbounded_channel();
        let url = central(sender).await;
        let worker = tokio::spawn(collect(
            binding(repo.path()),
            DEFAULT_REMOTE.into(),
            connected(3).await,
            TaskClient::new(&url, TOKEN).unwrap(),
            WorkingFileRefresher::default(),
            Duration::from_secs(1),
        ));

        let first = timeout(Duration::from_secs(10), uploads.recv())
            .await
            .unwrap()
            .unwrap();
        let unchanged = timeout(Duration::from_secs(4), uploads.recv())
            .await
            .expect("the next period must renew the observation")
            .unwrap();
        assert_eq!(unchanged.body, first.body);

        std::fs::write(repo.path().join("later.ts"), "l").unwrap();
        let changed = timeout(Duration::from_secs(4), uploads.recv())
            .await
            .unwrap()
            .unwrap();
        let files = changed.body["files"].as_array().unwrap();
        assert_eq!(files.len(), 5);
        assert!(files.contains(&json!({"relativePath": "later.ts", "changeType": "UNTRACKED"})));

        std::fs::remove_file(repo.path().join("later.ts")).unwrap();
        git(repo.path(), &["checkout", "-q", "-b", "another-branch"]);
        let reverted = timeout(Duration::from_secs(4), uploads.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reverted.body["branch"], "another-branch");
        assert_eq!(reverted.body["files"], first.body["files"]);

        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
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
        Json(revision_body(&Uuid::new_v4().to_string()))
    }

    async fn delayed_lookup() -> Json<Value> {
        Json(revision_body(&Uuid::new_v4().to_string()))
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
            .route(ROUTE, get(delayed_lookup).put(delayed_replace))
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
                        Duration::from_secs(60),
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
