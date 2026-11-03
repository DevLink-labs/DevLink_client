#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, login_at, run_at, text};
use devlink_broker::{
    agent_profile, central::ConnectionStatus, config, credentials, local::Runtime, private_file,
    teams,
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Output,
    sync::atomic::Ordering,
    time::Duration,
};

fn data(output: Output) -> Value {
    assert!(output.status.success(), "{}", text(&output));
    assert_no_secrets(&output);
    serde_json::from_slice(&output.stdout).unwrap()
}
fn deny(output: Output, reason: &str) {
    assert!(!output.status.success(), "unexpected success");
    assert_no_secrets(&output);
    assert!(text(&output).contains(reason), "{}", text(&output));
}
fn selected(f: &Fixture) {
    data(f.login());
    data(f.run(&["team", "select", "--id", "71"], None));
}
fn create(f: &Fixture) -> Output {
    f.run(
        &[
            "agent",
            "create",
            "--name",
            "My Agent",
            "--tool",
            "codex",
            "--project-name",
            "My project",
            "--project",
            f._root.path().to_str().unwrap(),
        ],
        None,
    )
}
fn profile(path: &Path) -> Value {
    private_file::read(&agent_profile::path(path), 16384)
        .unwrap()
        .unwrap()
}
struct Stop(PathBuf);
impl Drop for Stop {
    fn drop(&mut self) {
        let _ = run_at(&self.0, &["stop"], None);
    }
}
async fn connected(path: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(r) = Runtime::load(path)
            && let Ok(h) = r.health().await
            && h.central_server == ConnectionStatus::Connected
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "broker did not connect"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creates_once_with_verified_selected_team_and_reuses_owned_id_without_rotation() {
    let f = Fixture::new().await;
    deny(create(&f), "USER_LOGIN_REQUIRED");
    data(f.login());
    deny(create(&f), "TEAM_SELECTION_REQUIRED");
    data(f.run(&["team", "select", "--id", "71"], None));
    let first = data(create(&f));
    assert_eq!(first["state"], "ready");
    assert_eq!(first["agent"]["agentId"], 201);
    assert_eq!(first["agent"]["ownerUserId"], 51);
    assert_eq!(first["agent"]["teamId"], 71);
    let before = profile(&f.path);
    credentials::verify_private_permissions(&agent_profile::path(&f.path)).unwrap();
    assert_eq!(data(create(&f))["reused"], true);
    assert_eq!(
        data(f.run(&["agent", "use", "--id", "201"], None))["reused"],
        true
    );
    assert_eq!(data(f.run(&["agent", "status"], None))["verified"], true);
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(before, profile(&f.path));
    assert!(!before.to_string().contains(account_server::COOKIE));
    let conflict = std::process::Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(&f.path)
        .arg("start")
        .env("DEVLINK_TOKEN", "synthetic-conflicting-env")
        .output()
        .unwrap();
    assert!(!text(&conflict).contains("synthetic-conflicting-env"));
    deny(conflict, "conflicts with the bound Agent");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_create_is_never_replayed_and_explicit_id_recovers_the_known_server_agent() {
    let f = Fixture::new().await;
    selected(&f);
    f.state.create_mode.store(1, Ordering::SeqCst);
    deny(create(&f), "not confirmed");
    assert_eq!(profile(&f.path)["state"]["kind"], "create_pending");
    deny(create(&f), "AGENT_CREATION_UNKNOWN");
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert!(
        data(f.run(&["agent", "list"], None))
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["agentId"] == 201)
    );
    data(f.run(
        &[
            "agent",
            "use",
            "--id",
            "201",
            "--project",
            f._root.path().to_str().unwrap(),
        ],
        None,
    ));
    assert_eq!(profile(&f.path)["state"]["kind"], "needs_token");
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 0);
    data(f.run(&["agent", "token"], None));
    assert_eq!(profile(&f.path)["state"]["kind"], "ready");
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definite_rejection_allows_retry_but_uncertain_token_issue_requires_explicit_rotation() {
    let f = Fixture::new().await;
    selected(&f);
    f.state.create_mode.store(2, Ordering::SeqCst);
    deny(create(&f), "VALIDATION_FAILED");
    assert_eq!(profile(&f.path)["state"]["kind"], "vacant");
    f.state.create_mode.store(0, Ordering::SeqCst);
    f.state.token_mode.store(1, Ordering::SeqCst);
    deny(create(&f), "CENTRAL_REQUEST_REJECTED");
    assert_eq!(profile(&f.path)["state"]["agent"]["agentId"], 201);
    assert_eq!(data(create(&f))["state"], "token_required");
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
    f.state.token_mode.store(0, Ordering::SeqCst);
    data(f.run(&["agent", "token"], None));
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_owner_team_and_unverified_selection_cannot_rebind_or_issue_credentials() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let before = profile(&f.path);
    for id in ["302", "303"] {
        deny(
            f.run(
                &[
                    "agent",
                    "use",
                    "--id",
                    id,
                    "--project",
                    f._root.path().to_str().unwrap(),
                ],
                None,
            ),
            "ACCESS_DENIED",
        );
    }
    assert_eq!(before, profile(&f.path));
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
    let mut selection = teams::stored_selection(&f.path).unwrap().unwrap();
    selection.user_id = 999;
    private_file::write(&teams::selection_path(&f.path), &selection, 16384).unwrap();
    deny(f.run(&["agent", "token"], None), "current central user");
    assert_eq!(before, profile(&f.path));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_token_principal_is_never_published_and_corrupt_profile_needs_explicit_use() {
    let f = Fixture::new().await;
    selected(&f);
    f.state.wrong_context.store(true, Ordering::SeqCst);
    deny(create(&f), "another Agent");
    assert_eq!(profile(&f.path)["state"]["kind"], "needs_token");
    assert!(profile(&f.path)["state"].get("token").is_none());
    f.state.wrong_context.store(false, Ordering::SeqCst);
    data(f.run(&["agent", "token"], None));
    private_file::write(
        &agent_profile::path(&f.path),
        &json!({"malformed":"safe-private-file"}),
        16384,
    )
    .unwrap();
    deny(create(&f), "Invalid private snapshot");
    data(f.run(
        &[
            "agent",
            "use",
            "--id",
            "201",
            "--project",
            f._root.path().to_str().unwrap(),
        ],
        None,
    ));
    assert_eq!(profile(&f.path)["state"]["kind"], "needs_token");
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_broker_uses_profile_without_environment_and_keeps_four_credentials_separate() {
    let f = Fixture::new().await;
    let _stop = Stop(f.path.clone());
    selected(&f);
    data(create(&f));
    let token = profile(&f.path)["state"]["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let started = f.run(&["start"], None);
    assert!(started.status.success(), "{}", text(&started));
    assert_no_secrets(&started);
    connected(&f.path).await;
    let runtime = Runtime::load(&f.path).unwrap();
    let mcp = credentials::load(&f.path).unwrap();
    assert_ne!(mcp.local_token, runtime.control_token);
    assert_ne!(mcp.local_token, token);
    assert_ne!(runtime.control_token, token);
    let file = std::fs::read_to_string(credentials::path(&f.path)).unwrap();
    for secret in [
        &token,
        &runtime.control_token,
        &account_server::COOKIE.to_owned(),
    ] {
        assert!(!file.contains(secret));
    }
    assert_eq!(file.matches("localToken").count(), 1);
    assert_eq!(
        text(&f.run(&["mcp", "path"], None)).trim(),
        credentials::path(&f.path).to_str().unwrap()
    );
    deny(f.run(&["agent", "token"], None), "Stop the broker");
    deny(
        f.run(
            &[
                "agent",
                "use",
                "--id",
                "301",
                "--project",
                f._root.path().to_str().unwrap(),
            ],
            None,
        ),
        "Stop the broker",
    );
    let r = f.run(&["stop"], None);
    assert!(r.status.success(), "{}", text(&r));
    data(f.run(&["agent", "token"], None));
    assert_ne!(
        credentials::load(&f.path).unwrap().local_token,
        mcp.local_token
    );
    assert_eq!(profile(&f.path)["state"]["agent"]["agentId"], 201);
    assert!(f.run(&["start"], None).status.success());
    connected(&f.path).await;
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_local_profiles_of_one_user_cannot_run_brokers_at_the_same_time() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let _stop_a = Stop(f.path.clone());
    let second = f._root.path().join("second");
    std::fs::create_dir(&second).unwrap();
    let other = second.join("config.json");
    config::atomic_json(&other, &config::Config::load(&f.path).unwrap()).unwrap();
    data(login_at(&other));
    data(run_at(&other, &["team", "select", "--id", "71"], None));
    data(run_at(
        &other,
        &[
            "agent",
            "use",
            "--id",
            "301",
            "--project",
            second.to_str().unwrap(),
        ],
        None,
    ));
    data(run_at(&other, &["agent", "token"], None));
    let _stop_b = Stop(other.clone());
    assert!(f.run(&["start"], None).status.success());
    connected(&f.path).await;
    deny(run_at(&other, &["broker"], None), "Another broker");
    assert!(!devlink_broker::local::running(&other).unwrap());
    assert!(f.run(&["stop"], None).status.success());
    assert!(run_at(&other, &["start"], None).status.success());
    connected(&other).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_user_login_cannot_replace_live_actor_and_stopped_switch_invalidates_local_agent_token()
 {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let _stop = Stop(f.path.clone());
    assert!(f.run(&["start"], None).status.success());
    connected(&f.path).await;
    f.state.user_id.store(52, Ordering::SeqCst);
    deny(f.login(), "Stop the broker");
    assert_eq!(profile(&f.path)["state"]["kind"], "ready");
    assert!(f.run(&["stop"], None).status.success());
    data(f.login());
    assert_eq!(profile(&f.path)["state"]["kind"], "needs_token");
    assert!(devlink_broker::broker::central_token_for(&f.path).is_err());
    data(f.run(&["team", "select", "--id", "71"], None));
    deny(f.run(&["agent", "token"], None), "differs from bound Agent");
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_broker_starting_before_runtime_publication_blocks_credential_mutation() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let before = profile(&f.path);
    let guard = config::lock(&config::state_directory(&f.path).join("broker.lock")).unwrap();
    deny(f.run(&["agent", "token"], None), "Stop the broker");
    assert_eq!(before, profile(&f.path));
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
    drop(guard);
    f.state.pause_token.store(true, Ordering::SeqCst);
    let path = f.path.clone();
    let issuing = tokio::task::spawn_blocking(move || run_at(&path, &["agent", "token"], None));
    tokio::time::timeout(Duration::from_secs(15), f.state.token_entered.notified())
        .await
        .unwrap();
    let broker_blocked =
        config::lock(&config::state_directory(&f.path).join("broker.lock")).is_err();
    f.state.token_resume.notify_one();
    data(issuing.await.unwrap());
    assert!(
        broker_blocked,
        "Direct broker startup must be excluded until token publication completes"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_failure_is_not_pending_and_empty_unknown_creation_requires_explicit_risk_acknowledgement()
 {
    let f = Fixture::new().await;
    selected(&f);
    f.state.csrf_failure.store(true, Ordering::SeqCst);
    deny(create(&f), "CENTRAL_REQUEST_REJECTED");
    assert!(!agent_profile::path(&f.path).exists());
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 0);
    f.state.csrf_failure.store(false, Ordering::SeqCst);
    f.state.create_mode.store(3, Ordering::SeqCst);
    deny(create(&f), "not confirmed");
    deny(create(&f), "AGENT_CREATION_UNKNOWN");
    assert!(f.state.agents.lock().unwrap().is_empty());
    deny(
        f.run(&["agent", "reset-pending"], None),
        "--accept-duplicate-risk",
    );
    assert_eq!(profile(&f.path)["state"]["kind"], "create_pending");
    let reset = data(f.run(&["agent", "reset-pending", "--accept-duplicate-risk"], None));
    assert_eq!(reset["creationRetried"], false);
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    f.state.create_mode.store(0, Ordering::SeqCst);
    data(create(&f));
    deny(
        f.run(&["agent", "reset-pending", "--accept-duplicate-risk"], None),
        "known Agent IDs must be reused",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switching_selected_team_blocks_startup_before_ws_and_keeps_the_agent_binding() {
    let f = Fixture::new().await;
    let _stop = Stop(f.path.clone());
    selected(&f);
    data(create(&f));
    let before = profile(&f.path);
    data(f.run(
        &["team", "join", "--code-stdin"],
        Some(&format!("{}\n", account_server::INVITE)),
    ));
    data(f.run(&["team", "select", "--id", "72"], None));
    let start = f.run(&["start"], None);
    assert!(!start.status.success());
    assert_no_secrets(&start);
    assert!(
        !f.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("ws:"))
    );
    assert_eq!(before, profile(&f.path));
    data(f.run(&["team", "select", "--id", "71"], None));
    assert!(f.run(&["start"], None).status.success());
    connected(&f.path).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_listing_defaults_to_bound_agent_without_contacting_central_or_accepting_tasks() {
    use devlink_broker::{
        inbox::Inbox,
        protocol::{TaskCreatedMessage, TaskCreatedPayload},
    };
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let inbox = Inbox::open(&f.path).unwrap();
    let origin = config::Config::load(&f.path).unwrap().server_url;
    for id in [201, 202] {
        inbox
            .record(
                origin.clone(),
                id,
                TaskCreatedMessage {
                    event_id: format!("bound-{id}"),
                    payload: TaskCreatedPayload {
                        task_id: id,
                        team_id: 71,
                        requester_agent_id: 301,
                        receiver_agent_id: id,
                        prompt: "Inspect only".into(),
                        created_at: "2026-10-30T09:00:00".into(),
                    },
                },
            )
            .await
            .unwrap();
    }
    let calls = f.state.calls.lock().unwrap().len();
    let tasks = data(f.run(&["inbox", "list"], None));
    assert_eq!(tasks.as_array().unwrap().len(), 1);
    assert_eq!(tasks[0]["message"]["payload"]["receiver_agent_id"], 201);
    assert_eq!(f.state.calls.lock().unwrap().len(), calls);
    assert!(!Runtime::path(&f.path).exists());
}
