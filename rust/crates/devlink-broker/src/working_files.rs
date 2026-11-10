//! IDX-3-02 working-file collection (S15P31D108-186).
//!
//! Every 30 seconds the broker replaces the bound user's uncommitted-file list for the bound
//! project's repository on central. An empty list is still an observation; central expires an
//! observation 90 seconds after it arrives. `find_agent` asks for one more observation right
//! before its central call and continues with the last valid one if that fails. Bodies come from
//! `git_scope` (IDX-3-03), so only the branch, relative paths and change types leave the PC.
use crate::{
    agent_profile::InspectionBinding,
    central::{CancellationToken, CentralState},
    dependency_graph::{bound_ready, git::GitProject},
    git_scope::WorkingFiles,
    task_client::TaskClient,
};
use anyhow::{Result, anyhow, ensure};
use reqwest::Method;
use std::{
    path::Path,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::Mutex,
    time::{MissedTickBehavior, interval, timeout},
};

pub const INTERVAL: Duration = Duration::from_secs(30);
/// Budget for the re-check right before `find_agent`; past it central uses the last observation.
pub const REFRESH_LIMIT: Duration = Duration::from_secs(3);
/// Remote whose URL hash identifies the repository when `dependencyRemote` is not configured.
pub const DEFAULT_REMOTE: &str = "origin";

struct Collector {
    git: GitProject,
    binding: InspectionBinding,
    central: CentralState,
    client: TaskClient,
    path: String,
    // One observation at a time, so an older list never lands after a newer one.
    sending: Mutex<()>,
}

impl Collector {
    async fn publish(&self) -> Result<()> {
        let _sending = self.sending.lock().await;
        ensure!(
            bound_ready(&self.central, &self.binding).await,
            "Central is not ready for the bound Agent"
        );
        let (branch, files) = self.git.working().await?;
        // A branch name central would reject is reported like a detached HEAD.
        let branch = branch.filter(|name| WorkingFiles::branch_allowed(name));
        let body = serde_json::to_value(WorkingFiles::new(branch.as_deref(), files)?)?;
        let (status, reply) = self
            .client
            .request_graph_http(Method::PUT, &self.path, Some(body))
            .await
            .map_err(|_| anyhow!("Working-file upload failed"))?;
        ensure!(
            (200..300).contains(&status) && reply.success,
            "Working-file upload rejected"
        );
        Ok(())
    }
}

/// Handle `find_agent` uses for the re-check. Without a running collector it does nothing.
#[derive(Clone, Default)]
pub struct WorkingFileRefresher(Arc<OnceLock<Arc<Collector>>>);

impl WorkingFileRefresher {
    /// Publishes a fresh observation within `limit`. Never fails the caller; returns whether a
    /// new observation reached central.
    pub async fn refresh(&self, limit: Duration) -> bool {
        match self.0.get() {
            Some(collector) => matches!(timeout(limit, collector.publish()).await, Ok(Ok(()))),
            None => false,
        }
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
        _ = collect(binding, remote, central, client, refresher) => {},
    }
}

async fn collect(
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
    refresher: WorkingFileRefresher,
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
    let collector = Arc::new(Collector {
        git,
        binding,
        central,
        client,
        path,
        sending: Mutex::new(()),
    });
    let _ = refresher.0.set(collector.clone());
    let mut tick = interval(INTERVAL);
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut failed = false;
    loop {
        tick.tick().await;
        if !bound_ready(&collector.central, &collector.binding).await {
            continue;
        }
        let result = collector.publish().await;
        if result.is_err() && !failed {
            eprintln!(
                "Working-file upload failed; central keeps the last observation until it expires."
            );
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
        http::HeaderMap,
        routing::put,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::{collections::BTreeSet, process::Command};
    use tokio::sync::{RwLock, mpsc};

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

        // Clean tree: an empty list is still an observation.
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "work"]);
        assert!(refresher.refresh(REFRESH_LIMIT).await);
        let refreshed = uploads.recv().await.unwrap();
        assert_eq!(refreshed.body, json!({"branch": "main", "files": []}));

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
}
