mod analyze;
mod git;

use crate::{
    agent_profile::InspectionBinding,
    central::{CancellationToken, CentralState, ConnectionStatus},
    task_client::TaskClient,
};
use anyhow::{Context, Result, ensure};
use git::GitProject;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time::{MissedTickBehavior, interval};

const VERSION: u32 = 1;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceConfig {
    branch_name: String,
    repo_kind: String,
    publisher: bool,
    revision: u64,
    analysis_commit_sha: Option<String>,
    analysis_version: Option<u32>,
}

pub async fn dependency_repo_key(path: &Path, remote: &str) -> Result<String> {
    Ok(GitProject::open(path, remote).await?.key)
}

pub(crate) async fn run(
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
    cancel: CancellationToken,
) {
    tokio::select! {
        _ = cancel.cancelled() => {},
        result = run_project(binding, remote, central, client) => {
            if result.is_err() { eprintln!("Dependency tracking could not open the bound Git project."); }
        },
    }
}

async fn run_project(
    binding: InspectionBinding,
    remote: String,
    central: CentralState,
    client: TaskClient,
) -> Result<()> {
    let git = Arc::new(GitProject::open(Path::new(&binding.project_root), &remote).await?);
    let route = format!("/api/v1/teams/{}/git", binding.team_id);
    let ready = || async {
        central
            .with_ready_context(|status, identity, _| {
                status == ConnectionStatus::Connected
                    && identity.is_some_and(|identity| {
                        identity.team_id == Some(binding.team_id)
                            && identity.agent_id == binding.agent_id
                            && identity.user_id == Some(binding.user_id)
                    })
            })
            .await
    };
    let publish = async {
        let mut tick = interval(Duration::from_secs(60));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut failed = false;
        loop {
            tick.tick().await;
            if !ready().await {
                continue;
            }
            let result = publish_snapshot(&git, &client, &route, &ready).await;
            if result.is_err() && !failed {
                eprintln!(
                    "Dependency graph analysis or upload failed; the last successful graph is retained."
                );
            }
            failed = result.is_err();
        }
    };
    // Temporary tree-only collector on a tree-specific path. The request body already follows the
    // IDX-3-02 working-file contract, so once that upload lands this block and `GitProject::working`
    // are removed and the tree keeps reading the same `working_files` rows.
    let observe_for_tree = async {
        let mut tick = interval(Duration::from_secs(3));
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut previous = None;
        let path = format!("{route}/dependency-graph/working-files/{}", git.key);
        loop {
            tick.tick().await;
            if !ready().await {
                continue;
            }
            let Ok((branch, files)) = git.working().await else {
                continue;
            };
            let observation = json!({
                "branch": branch,
                "files": files.into_iter().map(|(relative_path, change_type)| json!({
                    "relativePath": relative_path, "changeType": change_type
                })).collect::<Vec<_>>(),
            });
            if previous
                .as_ref()
                .is_some_and(|(value, sent): &(Value, Instant)| {
                    value == &observation && sent.elapsed() < Duration::from_secs(30)
                })
            {
                continue;
            }
            if ready().await
                && send(&client, Method::PUT, &path, Some(observation.clone()))
                    .await
                    .is_ok()
            {
                previous = Some((observation, Instant::now()));
            }
        }
    };
    tokio::join!(publish, observe_for_tree);
    Ok(())
}

async fn publish_snapshot<F, R>(
    git: &GitProject,
    client: &TaskClient,
    route: &str,
    ready: &F,
) -> Result<()>
where
    F: Fn() -> R,
    R: std::future::Future<Output = bool>,
{
    let path = format!("{route}/dependency-sources/{}", git.key);
    let (status, reply) = client
        .request_http(Method::GET, &path, &[], None)
        .await
        .map_err(|_| anyhow::anyhow!("Graph configuration unavailable"))?;
    if status == 404 {
        return Ok(());
    }
    ensure!(
        status == 200 && reply.success,
        "Graph configuration rejected"
    );
    let config: SourceConfig = serde_json::from_value(reply.data.context("Missing graph source")?)?;
    if !config.publisher {
        return Ok(());
    }
    let sha = git.sha(&config.branch_name).await?;
    if config.analysis_commit_sha.as_deref() == Some(&sha)
        && config.analysis_version == Some(VERSION)
    {
        return Ok(());
    }
    if let Some(previous) = &config.analysis_commit_sha {
        ensure!(
            (previous.len() == 40 || previous.len() == 64)
                && previous.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Invalid previous graph revision"
        );
        git.require_ancestor(previous, &sha).await?;
    }
    let files = git.snapshot(&sha).await?;
    let kind = config.repo_kind;
    let graph = tokio::task::spawn_blocking(move || analyze::analyze(&files, &kind)).await??;
    if !ready().await {
        return Ok(());
    }
    send(client, Method::POST, &format!("{route}/file-dependencies:batch"), Some(json!({
        "repoKey": git.key, "branchName": config.branch_name, "expectedRevision": config.revision,
        "analysisCommitSha": sha, "analysisVersion": VERSION, "nodes": graph.nodes, "edges": graph.edges,
    }))).await
}

async fn send(client: &TaskClient, method: Method, path: &str, body: Option<Value>) -> Result<()> {
    let (status, reply) = client
        .request_graph_http(method, path, body)
        .await
        .map_err(|_| anyhow::anyhow!("Graph request failed"))?;
    ensure!(
        (200..300).contains(&status) && reply.success,
        "Graph request rejected"
    );
    Ok(())
}
