//! IDX-3-01 commit history sync (S15P31D108-185).
//!
//! When the broker starts, and then every 60 seconds, it lists the non-merge commits of the
//! last 90 days reachable from `HEAD` but not from the last synced commit, oldest first, and
//! uploads them in `commits:batch` bodies built by `git_scope` (IDX-3-03). The list is read in
//! chunks; the checkpoint moves to the last commit of a chunk only after every batch of that
//! chunk was answered, and to `HEAD` after the whole list. Central de-duplicates by team,
//! repository and SHA and reports conflicting input, which is skipped. Authors are the bound
//! user (SET-3-03) or unverified; when that attribution changes, the history is sent again so
//! central can fill authors it stored as unverified.
use crate::{
    agent_profile::InspectionBinding,
    central::{CancellationToken, CentralState},
    config,
    dependency_graph::{
        bound_ready,
        git::{GitProject, OutputLimitExceeded},
    },
    git_identity::{IdentityCache, IdentityCheck, IdentityState},
    git_scope::{Author, CommitBatch, CommitChange, CommitRecord, RelativePath, RepoKey},
    private_file,
    task_client::TaskClient,
};
use anyhow::{Result, anyhow, bail, ensure};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{MissedTickBehavior, interval};

pub const INTERVAL: Duration = Duration::from_secs(60);
/// Commits older than this are not uploaded. It matches central's recommendation window
/// (`RecommendationScorePolicy.COMMIT_WINDOW`), which ignores older commits.
pub const HISTORY: Duration = Duration::from_secs(90 * 24 * 60 * 60);
/// Commits read from Git and checkpointed together.
const CHUNK: usize = 1000;
const STATE_FILE: &str = "commit-sync.json";
const STATE_LIMIT: usize = 4096;
const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Checkpoint {
    schema_version: u32,
    /// Central server, team and user the range was uploaded for; another binding of the same
    /// profile starts over.
    origin: String,
    team_id: u64,
    user_id: u64,
    repo_key: String,
    commit_sha: String,
    /// `unregistered` or the SET-3-03 fingerprint the range was attributed with.
    identity: String,
    /// Changes when Git history is deepened, even if HEAD stays the same.
    shallow_fingerprint: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Central or the Git email check is not ready; nothing was sent.
    NotReady,
    UpToDate,
    Synced {
        commits: usize,
        /// Commits in rejected batches or too large for any batch; not retried.
        skipped: usize,
    },
}

pub(crate) struct CommitSync {
    git: GitProject,
    binding: InspectionBinding,
    central: CentralState,
    client: TaskClient,
    identity: IdentityCache,
    repo_key: RepoKey,
    route: String,
    state_path: PathBuf,
    chunk: usize,
}

impl CommitSync {
    pub(crate) async fn open(
        config_path: &Path,
        binding: InspectionBinding,
        remote: &str,
        central: CentralState,
        client: TaskClient,
        identity: IdentityCache,
    ) -> Result<Self> {
        let git = GitProject::open(Path::new(&binding.project_root), remote).await?;
        let repo_key = RepoKey::new(&git.key)?;
        let route = format!("/api/v1/teams/{}/git/commits:batch", binding.team_id);
        Ok(Self {
            git,
            binding,
            central,
            client,
            identity,
            repo_key,
            route,
            state_path: config::state_directory(config_path).join(STATE_FILE),
            chunk: CHUNK,
        })
    }

    pub(crate) async fn sync_once(&self) -> Result<Outcome> {
        if !bound_ready(&self.central, &self.binding).await {
            return Ok(Outcome::NotReady);
        }
        // Wait for a fresh Git email check so the user's own commits are not stored as
        // unverified only because the comparison had not arrived yet. The whole run uses this
        // one result, so an expiry or refresh during collection cannot mix attributions.
        let Some(check) = self.identity.check().await else {
            return Ok(Outcome::NotReady);
        };
        let identity = match check.state() {
            IdentityState::Unknown => return Ok(Outcome::NotReady),
            IdentityState::Unregistered => "unregistered".to_owned(),
            IdentityState::Registered(fingerprint) => fingerprint,
        };
        let Some(tip) = self.git.head().await? else {
            return Ok(Outcome::UpToDate);
        };
        let shallow = self.git.shallow_boundaries().await?;
        let mut digest = Sha256::new();
        for sha in &shallow {
            digest.update(sha.as_bytes());
            digest.update(b"\n");
        }
        let shallow_fingerprint = format!("{:x}", digest.finalize());
        let since = self
            .load()
            .filter(|saved| {
                saved.identity == identity && saved.shallow_fingerprint == shallow_fingerprint
            })
            .map(|saved| saved.commit_sha);
        if since.as_deref() == Some(tip.as_str()) {
            return Ok(Outcome::UpToDate);
        }
        let after = SystemTime::now()
            .checked_sub(HISTORY)
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |time| time.as_secs());
        let mut ids = match &since {
            // A checkpoint that history rewriting removed falls back to the whole window.
            Some(old) => match self.git.commit_ids(Some(old), &tip, after).await {
                Ok(ids) => ids,
                Err(_) => self.git.commit_ids(None, &tip, after).await?,
            },
            None => self.git.commit_ids(None, &tip, after).await?,
        };
        ids.retain(|sha| !shallow.contains(sha));
        let (mut sent, mut skipped) = (0, 0);
        for chunk in ids.chunks(self.chunk) {
            let Some(chunk_skipped) = self.upload(chunk, &check, &shallow).await? else {
                return Ok(Outcome::NotReady);
            };
            skipped += chunk_skipped;
            sent += chunk.len();
            // Every ancestor of the chunk's last commit was listed before it, so the next
            // range can start there even if a later chunk fails.
            if let Some(last) = chunk.last() {
                self.save(last, &identity, &shallow_fingerprint)?;
            }
        }
        self.save(&tip, &identity, &shallow_fingerprint)?;
        Ok(Outcome::Synced {
            commits: sent - skipped.min(sent),
            skipped,
        })
    }

    /// Sends one chunk; `None` if central disconnected meanwhile, else the skipped count.
    async fn upload(
        &self,
        ids: &[String],
        check: &IdentityCheck,
        shallow: &BTreeSet<String>,
    ) -> Result<Option<usize>> {
        let mut pending = vec![ids];
        let mut skipped = 0;
        while let Some(ids) = pending.pop() {
            let commits = match self.git.commits(ids).await {
                Ok(commits) => commits,
                Err(error) if error.is::<OutputLimitExceeded>() => {
                    if ids.len() == 1 {
                        skipped += 1;
                    } else {
                        let (first, second) = ids.split_at(ids.len() / 2);
                        pending.push(second);
                        pending.push(first);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
            // If a fetch changed the shallow boundary while Git was reading this chunk,
            // discard its file lists and rebuild the range on the next sync.
            ensure!(
                self.git.shallow_boundaries().await? == *shallow,
                "Git shallow boundaries changed during commit collection"
            );
            let Some(part_skipped) = self.upload_records(ids.len(), commits, check).await? else {
                return Ok(None);
            };
            skipped += part_skipped;
        }
        Ok(Some(skipped))
    }

    async fn upload_records(
        &self,
        count: usize,
        commits: Vec<crate::dependency_graph::git::RawCommit>,
        check: &IdentityCheck,
    ) -> Result<Option<usize>> {
        let mut records = Vec::with_capacity(count);
        for commit in commits {
            let author = Author::from_attribution(&check.attribute(&commit.author_email));
            let files = commit.files.iter().filter_map(|(status, path)| {
                Some((
                    RelativePath::new(path).ok()?,
                    CommitChange::from_status(status)?,
                ))
            });
            if let Ok(record) = CommitRecord::new(&commit.sha, &commit.committed_at, author, files)
            {
                records.push(record);
            }
        }
        let invalid = count - records.len().min(count);
        let (batches, oversized) = CommitBatch::split(&self.repo_key, records)?;
        let mut skipped = invalid + oversized.len();
        for batch in &batches {
            if !bound_ready(&self.central, &self.binding).await {
                return Ok(None);
            }
            let (status, reply) = self
                .client
                .request_graph_http(
                    Method::POST,
                    &self.route,
                    Some(serde_json::to_value(batch)?),
                )
                .await
                .map_err(|_| anyhow!("Commit history upload failed"))?;
            match status {
                // Conflicts come back inside a successful reply and are skipped by central.
                200..=299 if reply.success => {}
                // The same body would be rejected again; do not block later history on it.
                400 | 413 => skipped += batch.commits().len(),
                _ => bail!("Commit history upload rejected"),
            }
        }
        Ok(Some(skipped))
    }

    fn load(&self) -> Option<Checkpoint> {
        private_file::read::<Checkpoint>(&self.state_path, STATE_LIMIT)
            .ok()
            .flatten()
            .filter(|saved| {
                saved.schema_version == SCHEMA_VERSION
                    && saved.origin == self.binding.origin
                    && saved.team_id == self.binding.team_id
                    && saved.user_id == self.binding.user_id
                    && saved.repo_key == self.repo_key.as_str()
            })
    }

    fn save(&self, commit_sha: &str, identity: &str, shallow_fingerprint: &str) -> Result<()> {
        let checkpoint = Checkpoint {
            schema_version: SCHEMA_VERSION,
            origin: self.binding.origin.clone(),
            team_id: self.binding.team_id,
            user_id: self.binding.user_id,
            repo_key: self.repo_key.as_str().to_owned(),
            commit_sha: commit_sha.to_owned(),
            identity: identity.to_owned(),
            shallow_fingerprint: shallow_fingerprint.to_owned(),
        };
        private_file::write(&self.state_path, &checkpoint, STATE_LIMIT)
    }
}

pub(crate) async fn run(
    config_path: PathBuf,
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
    identity: IdentityCache,
    cancel: CancellationToken,
) {
    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = sync(config_path, binding, remote, central, client, identity) => {},
    }
}

async fn sync(
    config_path: PathBuf,
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
    identity: IdentityCache,
) {
    let Ok(commits) =
        CommitSync::open(&config_path, binding, &remote, central, client, identity).await
    else {
        eprintln!(
            "Commit history sync is off: the bound project is not a Git repository with a supported '{remote}' remote."
        );
        return;
    };
    // The first tick runs at once; until central and the Git email check are ready, check
    // again every few seconds instead of waiting a whole interval.
    let mut tick = interval(INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut failed = false;
    loop {
        tick.tick().await;
        let mut result = commits.sync_once().await;
        while matches!(result, Ok(Outcome::NotReady)) {
            tokio::time::sleep(Duration::from_secs(5)).await;
            result = commits.sync_once().await;
        }
        if result.is_err() && !failed {
            eprintln!("Commit history upload failed; the range is sent again on the next sync.");
        }
        failed = result.is_err();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::central::{CentralIdentity, ConnectionStatus};
    use axum::{
        Json, Router,
        extract::{Path as UrlPath, State},
        http::StatusCode,
        response::IntoResponse,
        routing::post,
    };
    use serde_json::{Value, json};
    use std::{
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU16, AtomicUsize, Ordering},
        },
    };
    use tokio::sync::RwLock;

    const ME: &str = "me@example.test";

    #[derive(Clone, Default)]
    struct Central {
        bodies: Arc<Mutex<Vec<(u64, Value)>>>,
        status: Arc<AtomicU16>,
        /// Requests from this index on fail with 503.
        fail_from: Arc<AtomicUsize>,
    }

    async fn upload(
        State(central): State<Central>,
        UrlPath(team): UrlPath<u64>,
        Json(body): Json<Value>,
    ) -> impl IntoResponse {
        let index = {
            let mut bodies = central.bodies.lock().unwrap();
            bodies.push((team, body));
            bodies.len() - 1
        };
        let status = if index >= central.fail_from.load(Ordering::SeqCst) {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::from_u16(central.status.load(Ordering::SeqCst)).unwrap()
        };
        let reply = if status.is_success() {
            json!({"success": true, "data": {"conflictCount": 0, "conflicts": []}})
        } else {
            json!({"success": false, "code": "REJECTED"})
        };
        (status, Json(reply))
    }

    async fn serve(central: Central) -> String {
        central.status.store(200, Ordering::SeqCst);
        central.fail_from.store(usize::MAX, Ordering::SeqCst);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/api/v1/teams/{team}/git/commits:batch", post(upload))
            .with_state(central);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("ws://{address}")
    }

    fn git(root: &Path, email: &str, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c"])
            .arg(format!("user.email={email}"))
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn commit(root: &Path, email: &str, file: &str, text: &str) -> String {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
        git(root, email, &["add", "-A"]);
        git(root, email, &["commit", "-q", "-m", file]);
        git(root, email, &["rev-parse", "HEAD"])
    }

    /// A commit authored and committed at `date`.
    fn commit_at(root: &Path, file: &str, date: &str) -> String {
        std::fs::write(root.join(file), file).unwrap();
        git(root, ME, &["add", "-A"]);
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=t", "-c"])
            .arg(format!("user.email={ME}"))
            .args(["commit", "-q", "-m", file])
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date)
            .status()
            .unwrap();
        assert!(status.success());
        git(root, ME, &["rev-parse", "HEAD"])
    }

    struct Fixture {
        repo: tempfile::TempDir,
        profile: tempfile::TempDir,
        central: Central,
        url: String,
        identity: IdentityCache,
        sync: CommitSync,
    }

    async fn fixture() -> Fixture {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        git(root, ME, &["-c", "init.defaultBranch=main", "init", "-q"]);
        git(
            root,
            ME,
            &["remote", "add", "origin", "git@github.com:acme/history.git"],
        );
        let profile = tempfile::tempdir().unwrap();
        crate::credentials::set_private_permissions(profile.path()).unwrap();
        let config_path = profile.path().join("config.json");
        let central = Central::default();
        let url = serve(central.clone()).await;
        let identity = IdentityCache::default();
        let sync = open(root, &config_path, &url, &identity, "http://127.0.0.1/", 7).await;
        Fixture {
            repo,
            profile,
            central,
            url,
            identity,
            sync,
        }
    }

    /// A sync for `team` on `origin` that central reports as connected.
    async fn open(
        root: &Path,
        config_path: &Path,
        url: &str,
        identity: &IdentityCache,
        origin: &str,
        team: u64,
    ) -> CommitSync {
        let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
        let session = state.begin_session().await;
        assert!(session.set_status(ConnectionStatus::Connected).await);
        assert!(
            session
                .set_identity(CentralIdentity {
                    connection_id: "c".into(),
                    agent_id: 12,
                    user_id: Some(3),
                    team_id: Some(team),
                })
                .await
        );
        let binding = InspectionBinding {
            origin: origin.into(),
            user_id: 3,
            team_id: team,
            agent_id: 12,
            project_root: root.to_str().unwrap().to_owned(),
            tool: crate::sessions::ToolType::Codex,
        };
        CommitSync::open(
            config_path,
            binding,
            "origin",
            state,
            TaskClient::new(url, "token").unwrap(),
            identity.clone(),
        )
        .await
        .unwrap()
    }

    impl Fixture {
        async fn clone_history(&self, filter: &str) -> (tempfile::TempDir, CommitSync) {
            let clone = tempfile::tempdir().unwrap();
            let url = url::Url::from_directory_path(self.repo.path()).unwrap();
            git(
                clone.path(),
                ME,
                &["clone", "-q", "--no-checkout", filter, url.as_str(), "."],
            );
            git(
                clone.path(),
                ME,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "git@github.com:acme/history.git",
                ],
            );
            let sync = open(
                clone.path(),
                &self.profile.path().join("config.json"),
                &self.url,
                &self.identity,
                "http://127.0.0.1/",
                7,
            )
            .await;
            (clone, sync)
        }

        async fn reopen(&self, origin: &str, team: u64) -> CommitSync {
            let config_path = self.profile.path().join("config.json");
            open(
                self.repo.path(),
                &config_path,
                &self.url,
                &self.identity,
                origin,
                team,
            )
            .await
        }
        fn sent_to(&self) -> Vec<(u64, Value)> {
            std::mem::take(&mut *self.central.bodies.lock().unwrap())
        }
        fn sent(&self) -> Vec<Value> {
            self.sent_to()
                .into_iter()
                .map(|(team, body)| {
                    assert_eq!(team, 7);
                    body
                })
                .collect()
        }
        fn shas(&self) -> Vec<String> {
            self.sent()
                .iter()
                .flat_map(|body| body["commits"].as_array().unwrap().clone())
                .map(|commit| commit["commitSha"].as_str().unwrap().to_owned())
                .collect()
        }
    }

    #[tokio::test]
    async fn deepening_a_shallow_clone_revisits_skipped_commits_at_the_same_head() {
        let f = fixture().await;
        let root = f.repo.path();
        let first = commit(root, ME, "old.txt", "old");
        let boundary = commit(root, ME, "new.txt", "new");
        let tip = commit(root, ME, "tip.txt", "tip");
        f.identity.set_for_test(None).await;
        let (clone, sync) = f.clone_history("--depth=2").await;

        assert_eq!(
            sync.git.shallow_boundaries().await.unwrap(),
            [boundary.clone()].into()
        );
        sync.sync_once().await.unwrap();
        assert_eq!(f.shas().as_slice(), std::slice::from_ref(&tip));
        assert_eq!(sync.sync_once().await.unwrap(), Outcome::UpToDate);

        let mut legacy = serde_json::to_value(sync.load().unwrap()).unwrap();
        legacy["schemaVersion"] = json!(1);
        legacy.as_object_mut().unwrap().remove("shallowFingerprint");
        private_file::write(&sync.state_path, &legacy, STATE_LIMIT).unwrap();
        sync.sync_once().await.unwrap();
        assert_eq!(f.shas().as_slice(), std::slice::from_ref(&tip));

        let url = url::Url::from_directory_path(root).unwrap();
        git(
            clone.path(),
            ME,
            &["fetch", "-q", "--unshallow", url.as_str()],
        );
        assert_eq!(
            sync.git.head().await.unwrap().as_deref(),
            Some(tip.as_str())
        );
        let check = f.identity.check().await.unwrap();
        assert!(
            sync.upload(
                std::slice::from_ref(&tip),
                &check,
                &[boundary.clone()].into()
            )
            .await
            .is_err()
        );
        assert!(f.sent().is_empty());
        sync.sync_once().await.unwrap();
        let commits = f
            .sent()
            .into_iter()
            .flat_map(|body| body["commits"].as_array().unwrap().clone())
            .collect::<Vec<_>>();
        assert_eq!(
            commits
                .iter()
                .map(|commit| commit["commitSha"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [&first, &boundary, &tip]
        );
        assert_eq!(
            commits[1]["files"],
            json!([{"relativePath": "new.txt", "changeType": "ADDED"}])
        );
    }

    #[tokio::test]
    async fn blobless_clone_uploads_the_same_renames_as_a_full_clone_without_fetching_blobs() {
        let f = fixture().await;
        let root = f.repo.path();
        git(root, ME, &["config", "uploadpack.allowFilter", "true"]);
        let first = commit(root, ME, "old.txt", &"same line\n".repeat(40));
        commit(root, ME, "exact-old.txt", "unchanged\n");
        git(root, ME, &["mv", "old.txt", "new.txt"]);
        git(root, ME, &["mv", "exact-old.txt", "exact-new.txt"]);
        let renamed = commit(root, ME, "new.txt", &("same line\n".repeat(40) + "edit\n"));
        let tip = commit(root, ME, "tip.txt", "tip");
        f.identity.set_for_test(None).await;
        f.sync.sync_once().await.unwrap();
        let full = f.sent();
        let (clone, sync) = f.clone_history("--filter=blob:none").await;
        let objects = || {
            git(
                clone.path(),
                ME,
                &["rev-list", "--objects", "--missing=print", "HEAD"],
            )
        };
        let before = objects();
        assert!(before.lines().any(|line| line.starts_with('?')));
        // Use a fresh checkpoint for the new clone of the same repository.
        std::fs::remove_file(&sync.state_path).unwrap();
        sync.sync_once().await.unwrap();
        let partial = f.sent();
        assert_eq!(partial, full);
        assert_eq!(objects(), before);
        let commits = partial[0]["commits"].as_array().unwrap();
        assert_eq!(commits[0]["commitSha"], first);
        assert_eq!(commits.last().unwrap()["commitSha"], tip);
        let renamed = commits
            .iter()
            .find(|commit| commit["commitSha"] == renamed)
            .unwrap();
        assert_eq!(
            renamed["files"],
            json!([
                {"relativePath": "exact-new.txt", "changeType": "RENAMED"},
                {"relativePath": "new.txt", "changeType": "ADDED"},
                {"relativePath": "old.txt", "changeType": "DELETED"}
            ])
        );
    }

    #[tokio::test]
    async fn unusual_git_offsets_are_uploaded_as_utc_with_other_commits() {
        let f = fixture().await;
        let root = f.repo.path();
        let normal = commit(root, ME, "normal.txt", "normal");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let unusual = commit_at(root, "unusual.txt", &format!("{now} +2000"));
        assert!(git(root, ME, &["show", "-s", "--format=%cI", &unusual]).ends_with("+20:00"));
        f.identity.set_for_test(None).await;
        assert_eq!(
            f.sync.sync_once().await.unwrap(),
            Outcome::Synced {
                commits: 2,
                skipped: 0
            }
        );
        let bodies = f.sent();
        let commits = bodies[0]["commits"].as_array().unwrap();
        assert_eq!(commits[0]["commitSha"], normal);
        assert_eq!(commits[1]["commitSha"], unusual);
        let normalized = commits[1]["committedAt"].as_str().unwrap();
        assert!(normalized.ends_with('Z'));
        let parsed =
            time::OffsetDateTime::parse(normalized, &time::format_description::well_known::Rfc3339)
                .unwrap();
        assert_eq!(parsed.unix_timestamp(), now as i64);
    }

    #[tokio::test]
    async fn treeless_clone_retries_an_unavailable_remote_and_fetches_trees_in_one_request() {
        let f = fixture().await;
        let root = f.repo.path();
        git(root, ME, &["config", "uploadpack.allowFilter", "true"]);
        git(
            root,
            ME,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        );
        commit(root, ME, "old.txt", "old");
        for step in 0..4 {
            commit(root, ME, &format!("dir/step{step}.txt"), "step");
        }
        commit(root, ME, "dir/new.txt", "new");
        f.identity.set_for_test(None).await;
        f.sync.sync_once().await.unwrap();
        let full = f.sent();
        let (clone, sync) = f.clone_history("--filter=tree:0").await;
        let missing = url::Url::from_directory_path(root.join("missing-remote")).unwrap();
        git(
            clone.path(),
            ME,
            &["remote", "set-url", "origin", missing.as_str()],
        );
        std::fs::remove_file(&sync.state_path).unwrap();
        assert!(sync.sync_once().await.is_err());
        assert!(sync.load().is_none());
        assert!(f.sent().is_empty());

        let source = url::Url::from_directory_path(root).unwrap();
        git(
            clone.path(),
            ME,
            &["remote", "set-url", "origin", source.as_str()],
        );
        // Every fetch from the promisor remote adds one pack. Lazy reads would add one per tree.
        let packs = || {
            std::fs::read_dir(clone.path().join(".git/objects/pack"))
                .unwrap()
                .filter(|entry| {
                    entry
                        .as_ref()
                        .unwrap()
                        .path()
                        .extension()
                        .is_some_and(|ext| ext == "pack")
                })
                .count()
        };
        let before = packs();
        sync.sync_once().await.unwrap();
        assert_eq!(packs(), before + 1);
        assert_eq!(f.sent(), full);
        let object = git(root, ME, &["rev-parse", "HEAD:dir/new.txt"]);
        let missing_objects = git(
            clone.path(),
            ME,
            &["rev-list", "--objects", "--missing=print", "HEAD"],
        );
        assert!(
            missing_objects
                .lines()
                .any(|line| line.strip_prefix('?') == Some(object.as_str()))
        );
        assert_eq!(sync.sync_once().await.unwrap(), Outcome::UpToDate);
    }

    #[tokio::test]
    async fn sends_history_once_with_own_or_unverified_authors_then_only_new_commits() {
        let f = fixture().await;
        let root = f.repo.path();
        // User configuration must not hide the files of the root commit.
        git(root, ME, &["config", "log.showRoot", "false"]);
        let first = commit(root, ME, "src/login.ts", "a");
        let other = commit(root, "teammate@example.test", "docs/한글.md", "b");
        git(root, ME, &["mv", "src/login.ts", "src/auth.ts"]);
        git(root, ME, &["commit", "-q", "-m", "rename"]);
        let renamed = git(root, ME, &["rev-parse", "HEAD"]);
        git(root, ME, &["switch", "-q", "-c", "side"]);
        let side = commit(root, "teammate@example.test", "side.ts", "s");
        git(root, ME, &["switch", "-q", "main"]);
        git(root, ME, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);

        // Before the Git email check arrives nothing is sent.
        assert_eq!(f.sync.sync_once().await.unwrap(), Outcome::NotReady);
        assert!(f.sent().is_empty());

        f.identity.set_for_test(Some((3, ME))).await;
        assert_eq!(
            f.sync.sync_once().await.unwrap(),
            Outcome::Synced {
                commits: 4,
                skipped: 0
            }
        );
        let body = f.sent().remove(0);
        let key = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(b"github.com/acme/history")
        );
        assert_eq!(body["repoKey"], key);
        let commits = body["commits"].as_array().unwrap();
        // Oldest first; the merge commit is not sent.
        let shas: Vec<_> = commits
            .iter()
            .map(|c| c["commitSha"].as_str().unwrap())
            .collect();
        assert!(shas.iter().position(|s| *s == first) < shas.iter().position(|s| *s == renamed));
        assert_eq!(
            [&first, &other, &renamed, &side].map(|sha| shas.contains(&sha.as_str())),
            [true; 4]
        );
        let by_sha = |sha: &str| {
            commits
                .iter()
                .find(|c| c["commitSha"] == sha)
                .unwrap()
                .clone()
        };
        assert_eq!(by_sha(&first)["authorUserId"], 3);
        assert_eq!(
            by_sha(&first)["files"],
            json!([{"relativePath": "src/login.ts", "changeType": "ADDED"}])
        );
        assert_eq!(by_sha(&other)["authorUserId"], Value::Null);
        assert_eq!(
            by_sha(&other)["files"],
            json!([{"relativePath": "docs/한글.md", "changeType": "ADDED"}])
        );
        assert_eq!(
            by_sha(&renamed)["files"],
            json!([{"relativePath": "src/auth.ts", "changeType": "RENAMED"}])
        );
        let text = body.to_string();
        for private in [
            ME,
            "teammate@example.test",
            "github.com",
            root.to_str().unwrap(),
        ] {
            assert!(!text.contains(private), "{private} leaked");
        }

        // Nothing new: no request.
        assert_eq!(f.sync.sync_once().await.unwrap(), Outcome::UpToDate);
        assert!(f.sent().is_empty());
        let next = commit(root, ME, "src/next.ts", "n");
        f.sync.sync_once().await.unwrap();
        assert_eq!(f.shas(), [next]);
    }

    #[tokio::test]
    async fn checkpoint_waits_for_every_batch_and_skips_only_rejected_input() {
        let f = fixture().await;
        let root = f.repo.path();
        f.identity.set_for_test(Some((3, ME))).await;
        let first = commit(root, ME, "a.ts", "a");

        // Central unavailable: the range stays and is sent again.
        f.central.status.store(503, Ordering::SeqCst);
        assert!(f.sync.sync_once().await.is_err());
        assert_eq!(f.shas(), std::slice::from_ref(&first));
        f.central.status.store(200, Ordering::SeqCst);
        f.sync.sync_once().await.unwrap();
        assert_eq!(f.shas(), [first]);

        // Rejected input would fail again, so it is skipped and history moves on.
        let rejected = commit(root, ME, "b.ts", "b");
        f.central.status.store(400, Ordering::SeqCst);
        assert_eq!(
            f.sync.sync_once().await.unwrap(),
            Outcome::Synced {
                commits: 0,
                skipped: 1
            }
        );
        assert_eq!(f.shas(), [rejected]);
        f.central.status.store(200, Ordering::SeqCst);
        assert_eq!(f.sync.sync_once().await.unwrap(), Outcome::UpToDate);
    }

    #[tokio::test]
    async fn attribution_change_or_lost_checkpoint_resends_the_window() {
        let f = fixture().await;
        let root = f.repo.path();
        let mine = commit(root, ME, "a.ts", "a");
        f.identity.set_for_test(None).await;
        f.sync.sync_once().await.unwrap();
        assert_eq!(f.sent()[0]["commits"][0]["authorUserId"], Value::Null);

        // After the email is registered, central can fill the stored unverified author.
        f.identity.set_for_test(Some((3, ME))).await;
        f.sync.sync_once().await.unwrap();
        let body = f.sent().remove(0);
        assert_eq!(body["commits"][0]["commitSha"], mine.as_str());
        assert_eq!(body["commits"][0]["authorUserId"], 3);
        assert_eq!(f.sync.sync_once().await.unwrap(), Outcome::UpToDate);

        // Rewritten history removed the checkpoint commit: fall back to the whole window.
        git(root, ME, &["commit", "-q", "--amend", "-m", "amended"]);
        let amended = git(root, ME, &["rev-parse", "HEAD"]);
        git(root, ME, &["reflog", "expire", "--expire=now", "--all"]);
        git(root, ME, &["gc", "-q", "--prune=now"]);
        f.sync.sync_once().await.unwrap();
        assert_eq!(f.shas(), [amended]);
    }

    #[tokio::test]
    async fn recent_history_is_checkpointed_per_chunk_without_losing_older_commits() {
        let mut f = fixture().await;
        f.sync.chunk = 2;
        let root = f.repo.path().to_owned();
        f.identity.set_for_test(Some((3, ME))).await;
        let base = commit(&root, ME, "base.ts", "b");
        // Outside central's 90-day window: never sent, and its skewed date does not hide the
        // recent commit before it.
        commit_at(&root, "old.ts", "2020-01-01T00:00:00Z");
        let mut new = vec![base];
        new.extend((0..5).map(|n| commit(&root, ME, &format!("{n}.ts"), "n")));

        // The third chunk fails; the first two stay synced.
        f.central.fail_from.store(2, Ordering::SeqCst);
        assert!(f.sync.sync_once().await.is_err());
        assert_eq!(f.shas(), new);
        f.central.fail_from.store(usize::MAX, Ordering::SeqCst);
        assert_eq!(
            f.sync.sync_once().await.unwrap(),
            Outcome::Synced {
                commits: 2,
                skipped: 0
            }
        );
        assert_eq!(f.shas(), new[4..]);
        assert_eq!(f.sync.sync_once().await.unwrap(), Outcome::UpToDate);

        // A new range larger than one chunk is sent whole, oldest first.
        let more: Vec<_> = (5..10)
            .map(|n| commit(&root, ME, &format!("{n}.ts"), "n"))
            .collect();
        f.sync.sync_once().await.unwrap();
        assert_eq!(f.shas(), more);
    }

    #[tokio::test]
    async fn another_team_or_server_on_the_same_profile_starts_over() {
        let f = fixture().await;
        let root = f.repo.path();
        f.identity.set_for_test(None).await;
        let first = commit(root, ME, "a.ts", "a");
        f.sync.sync_once().await.unwrap();
        assert_eq!(f.shas(), std::slice::from_ref(&first));

        let other_team = f.reopen("http://127.0.0.1/", 8).await;
        other_team.sync_once().await.unwrap();
        let sent = f.sent_to();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, 8);
        assert_eq!(sent[0].1["commits"][0]["commitSha"], first.as_str());

        let other_server = f.reopen("http://other.test/", 8).await;
        other_server.sync_once().await.unwrap();
        assert_eq!(f.sent_to().len(), 1);
        assert_eq!(other_server.sync_once().await.unwrap(), Outcome::UpToDate);
    }

    #[tokio::test]
    async fn submodule_changes_do_not_follow_user_ignore_settings() {
        let f = fixture().await;
        let root = f.repo.path();
        let first = commit(root, ME, "a.ts", "a");
        git(
            root,
            ME,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{first},sub"),
            ],
        );
        git(root, ME, &["commit", "-q", "-m", "submodule"]);
        git(root, ME, &["config", "diff.ignoreSubmodules", "all"]);
        let sha = git(root, ME, &["rev-parse", "HEAD"]);
        f.identity.set_for_test(None).await;
        f.sync.sync_once().await.unwrap();
        let bodies = f.sent();
        let commit = bodies
            .iter()
            .flat_map(|body| body["commits"].as_array().unwrap())
            .find(|commit| commit["commitSha"] == sha)
            .unwrap();
        assert_eq!(
            commit["files"],
            json!([{"relativePath":"sub","changeType":"ADDED"}])
        );
    }

    #[tokio::test]
    async fn history_filter_includes_the_boundary_and_ancestors_before_a_skewed_date() {
        let f = fixture().await;
        let root = f.repo.path();
        let boundary = commit_at(root, "boundary.ts", "2024-01-01T00:00:00Z");
        commit_at(root, "old.ts", "2020-01-01T00:00:00Z");
        let tip = commit_at(root, "recent.ts", "2024-02-01T00:00:00Z");
        let after = 1_704_067_200;
        assert_eq!(
            f.sync.git.commit_ids(None, &tip, after).await.unwrap(),
            [boundary.clone(), tip.clone()]
        );
        assert_eq!(
            f.sync
                .git
                .commit_ids(Some(&boundary), &tip, after)
                .await
                .unwrap(),
            [tip]
        );
    }

    #[tokio::test]
    async fn a_commit_larger_than_the_git_output_limit_does_not_block_later_history() {
        use std::{io::Write, process::Stdio};
        let f = fixture().await;
        let root = f.repo.path();
        let base = commit(root, ME, "base.ts", "b");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut commands = format!(
            "blob\nmark :1\ndata 1\nx\ncommit refs/heads/large\ncommitter t <{ME}> {now} +0000\ndata 5\nlarge\nfrom {base}\n"
        );
        let prefix = format!("{}/{}/", "a".repeat(180), "b".repeat(180));
        for i in 0..35_000 {
            commands.push_str(&format!("M 100644 :1 {prefix}{i:05}{}\n", "c".repeat(125)));
        }
        commands.push_str(&format!("\ncommit refs/heads/large\ncommitter t <{ME}> {now} +0000\ndata 5\nsmall\nM 100644 :1 next.ts\n\n"));
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["fast-import", "--quiet"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(commands.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        git(root, ME, &["symbolic-ref", "HEAD", "refs/heads/large"]);
        let tip = git(root, ME, &["rev-parse", "HEAD"]);
        f.identity.set_for_test(None).await;
        assert_eq!(
            f.sync.sync_once().await.unwrap(),
            Outcome::Synced {
                commits: 2,
                skipped: 1
            }
        );
        assert_eq!(f.shas(), [base, tip]);
        assert_eq!(f.sync.sync_once().await.unwrap(), Outcome::UpToDate);
        assert!(f.sent().is_empty());
    }
}
