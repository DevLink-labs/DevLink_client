#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, login_at, run_at, text};
use devlink_broker::{
    agent_profile, central::ConnectionStatus, config, credentials, local::Runtime, private_file,
    teams,
};
use devlink_cli::setup::{
    ErrorCode, Outcome, SetupContext, SetupStages, Step, StepExecutor, StepResult,
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
async fn setup_start_uses_shared_control_and_cli_reuses_then_stops_the_same_broker() {
    use devlink_broker::broker_control;
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let _cleanup = Stop(f.path.clone());
    let before = profile(&f.path);
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages =
        SetupStages::default().with_broker_executable(env!("CARGO_BIN_EXE_devlink").into());
    assert!(matches!(
        stages.execute(Step::StartBroker, &context).await,
        StepResult::Complete {
            outcome: Outcome::Applied,
            reused: false
        }
    ));
    let started = stages.broker().unwrap();
    let instance = started.health.instance_id.clone();
    let pid = started.health.pid;
    let output = f.run(&["start"], None);
    assert!(output.status.success(), "{}", text(&output));
    assert!(text(&output).contains("already running"));
    assert!(matches!(
        stages.execute(Step::StartBroker, &context).await,
        StepResult::Complete {
            outcome: Outcome::NotApplied,
            reused: true
        }
    ));
    assert_eq!(stages.broker().unwrap().health.instance_id, instance);
    let broker_control::Status::Running { health } = broker_control::status(&f.path).await.unwrap()
    else {
        panic!("broker stopped")
    };
    assert_eq!(health.pid, pid);
    let public = serde_json::to_string(&health).unwrap();
    let runtime = Runtime::load(&f.path).unwrap();
    assert!(!public.contains(&runtime.control_token));
    deny(f.run(&["agent", "token"], None), "Stop the broker");
    assert_eq!(before, profile(&f.path));
    assert!(f.run(&["stop"], None).status.success());
    assert!(matches!(
        broker_control::status(&f.path).await.unwrap(),
        broker_control::Status::Stopped
    ));
    assert_eq!(
        broker_control::stop(&f.path).await.unwrap(),
        broker_control::StopResult::AlreadyStopped
    );
    data(f.run(&["agent", "token"], None));
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_start_preserves_a_live_broker_when_identity_cannot_be_verified() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let _cleanup = Stop(f.path.clone());
    assert!(f.run(&["start"], None).status.success());
    let mut runtime = Runtime::load(&f.path).unwrap();
    let original = runtime.control_token.clone();
    runtime.control_token = "invalid-private-control-marker".into();
    config::atomic_json(&Runtime::path(&f.path), &runtime).unwrap();
    let mut stages =
        SetupStages::default().with_broker_executable(env!("CARGO_BIN_EXE_devlink").into());
    let result = stages
        .execute(
            Step::StartBroker,
            &SetupContext {
                config_path: f.path.clone(),
            },
        )
        .await;
    // Restore before assertions so fixture shutdown is always able to authenticate.
    runtime.control_token = original;
    config::atomic_json(&Runtime::path(&f.path), &runtime).unwrap();
    let StepResult::Failed(error) = result else {
        panic!("unverified identity was reused")
    };
    assert_eq!(error.code, ErrorCode::BrokerStartFailed);
    assert_eq!(error.outcome, Outcome::NotApplied);
    let safe = serde_json::to_string(&error).unwrap();
    assert!(!safe.contains(&runtime.control_token));
    assert!(!safe.contains("invalid-private-control-marker"));
    assert!(devlink_broker::local::running(&f.path).unwrap());
    assert_eq!(
        Runtime::load(&f.path)
            .unwrap()
            .health()
            .await
            .unwrap()
            .instance_id,
        runtime.instance_id
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_start_reports_port_conflict_without_exposing_credentials() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let _cleanup = Stop(f.path.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    config::set(
        &f.path,
        "localPort",
        &listener.local_addr().unwrap().port().to_string(),
    )
    .unwrap();
    let before = profile(&f.path);
    let mut stages =
        SetupStages::default().with_broker_executable(env!("CARGO_BIN_EXE_devlink").into());
    let StepResult::Failed(error) = stages
        .execute(
            Step::StartBroker,
            &SetupContext {
                config_path: f.path.clone(),
            },
        )
        .await
    else {
        panic!("occupied port started")
    };
    assert_eq!(error.code, ErrorCode::BrokerStartFailed);
    assert_eq!(error.outcome, Outcome::NotApplied);
    assert_eq!(error.location.as_ref().unwrap().field, Some("localPort"));
    assert!(
        error
            .message()
            .contains("port is in use or the OS denied the bind")
    );
    assert!(
        !serde_json::to_string(&error)
            .unwrap()
            .contains(before["state"]["token"].as_str().unwrap())
    );
    assert!(!devlink_broker::local::running(&f.path).unwrap());
    assert_eq!(before, profile(&f.path));
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
async fn different_agents_of_one_user_can_be_bound_and_run_while_another_is_connected() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let _stop_a = Stop(f.path.clone());
    assert!(f.run(&["start"], None).status.success());
    connected(&f.path).await;
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
    assert!(run_at(&other, &["start"], None).status.success());
    connected(&other).await;
    connected(&f.path).await;
    assert_ne!(
        Runtime::load(&f.path).unwrap().port,
        Runtime::load(&other).unwrap().port
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_agent_in_another_profile_cannot_run_or_rotate_a_live_credential() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let _stop = Stop(f.path.clone());
    let other = f._root.path().join("duplicate/config.json");
    std::fs::create_dir(other.parent().unwrap()).unwrap();
    config::atomic_json(&other, &config::Config::load(&f.path).unwrap()).unwrap();
    data(login_at(&other));
    data(run_at(&other, &["team", "select", "--id", "71"], None));
    private_file::write(&agent_profile::path(&other), &profile(&f.path), 16384).unwrap();
    assert!(f.run(&["start"], None).status.success());
    connected(&f.path).await;
    let issued = f.state.token_calls.load(Ordering::SeqCst);
    deny(run_at(&other, &["broker"], None), "Another broker");
    deny(run_at(&other, &["agent", "token"], None), "Another broker");
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), issued);
    assert!(!devlink_broker::local::running(&other).unwrap());
    connected(&f.path).await;
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

fn update(f: &Fixture, args: &[&str]) -> Output {
    let mut all = vec!["agent", "update"];
    all.extend_from_slice(args);
    f.run(&all, None)
}
fn update_calls(f: &Fixture) -> Vec<String> {
    f.state
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.starts_with("agent_update"))
        .cloned()
        .collect()
}

/// S15P31D108-146: only changed project/role fields reach central; identity and token stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_changes_only_requested_metadata_of_the_bound_agent_without_new_agent_or_token() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let before = profile(&f.path);
    deny(update(&f, &[]), "--project-name");
    deny(
        update(&f, &["--project-name", "   "]),
        "Invalid Agent update",
    );
    deny(
        update(&f, &["--role-description", &"r".repeat(256)]),
        "Invalid Agent update",
    );
    // Central isBlank would store U+001F as a cleared role.
    deny(
        update(&f, &["--role-description", "\u{1f}"]),
        "Invalid Agent update",
    );
    assert!(update_calls(&f).is_empty());
    let result = data(update(
        &f,
        &[
            "--project-name",
            "New project",
            "--role-description",
            "Reviewer",
        ],
    ));
    assert_eq!(result["updated"], true);
    assert_eq!(
        result["changedFields"],
        json!(["projectName", "roleDescription"])
    );
    assert_eq!(result["credentialsRotated"], false);
    assert_eq!(result["localProfileSynced"], true);
    assert_eq!(result["agent"]["agentId"], 201);
    assert_eq!(result["agent"]["name"], "My Agent");
    assert_eq!(result["agent"]["toolType"], "CODEX");
    assert_eq!(result["agent"]["projectName"], "New project");
    assert_eq!(result["agent"]["roleDescription"], "Reviewer");
    let after = profile(&f.path);
    assert_eq!(after["state"]["kind"], "ready");
    assert_eq!(after["state"]["token"], before["state"]["token"]);
    assert_eq!(after["projectRoot"], before["projectRoot"]);
    assert_eq!(after["state"]["agent"]["projectName"], "New project");
    assert_eq!(after["state"]["agent"]["roleDescription"], "Reviewer");
    let unchanged = std::fs::read(agent_profile::path(&f.path)).unwrap();
    let same = data(update(&f, &["--role-description", "Reviewer"]));
    assert_eq!(same["updated"], false);
    assert_eq!(same["changedFields"], json!([]));
    assert_eq!(
        std::fs::read(agent_profile::path(&f.path)).unwrap(),
        unchanged,
        "a no-op update must not rewrite the profile"
    );
    data(update(
        &f,
        &["--project-name", "Third", "--role-description", "Reviewer"],
    ));
    assert_eq!(
        update_calls(&f),
        [
            "agent_update:projectName,roleDescription",
            "agent_update:projectName"
        ]
    );
    assert_eq!(data(f.run(&["agent", "status"], None))["verified"], true);
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_failures_keep_the_profile_and_never_issue_credentials() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let before = profile(&f.path);
    for (mode, reason) in [
        (2, "outcome is unknown"),
        (3, "ACCESS_DENIED"),
        (1, "outcome is unknown"),
    ] {
        f.state.update_mode.store(mode, Ordering::SeqCst);
        // Mode 2 applies centrally before its mismatched reply, so each value is new.
        deny(
            update(&f, &["--project-name", &format!("Rejected {mode}")]),
            reason,
        );
        assert_eq!(before, profile(&f.path), "mode {mode}");
    }
    f.state.update_mode.store(0, Ordering::SeqCst);
    data(f.run(&["auth", "logout"], None));
    let calls = update_calls(&f).len();
    deny(
        update(&f, &["--project-name", "After logout"]),
        "USER_LOGIN_REQUIRED",
    );
    assert_eq!(update_calls(&f).len(), calls);
    assert_eq!(before, profile(&f.path));
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
}

/// S15P31D108-183: part and role tags follow the central rule. An omitted field keeps
/// the stored value and only an explicit empty list removes the tags.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_changes_part_and_role_tags_keeps_omitted_fields_and_clears_tags_explicitly() {
    let f = Fixture::new().await;
    selected(&f);
    data(create(&f));
    let before = profile(&f.path);
    assert_eq!(before["state"]["agent"]["part"], "OTHER");
    assert_eq!(before["state"]["agent"]["roles"], json!([]));
    deny(update(&f, &["--part", "DESIGN"]), "Choose FRONTEND");
    deny(
        update(&f, &["--roles", "login", "--clear-roles"]),
        "cannot be used with",
    );
    let eleven = (0..11)
        .map(|i| format!("tag{i}"))
        .collect::<Vec<_>>()
        .join(",");
    // A tag of U+0001 alone trims to nothing in central; a control character inside a
    // tag is refused as well.
    for roles in [
        eleven.as_str(),
        "login,  ",
        "",
        &"r".repeat(65),
        "\u{1}",
        "pay\u{7f}ment",
    ] {
        deny(update(&f, &["--roles", roles]), "Invalid Agent update");
    }
    assert!(update_calls(&f).is_empty());
    let tags = json!(["로그인", "결제 API"]);
    let result = data(update(
        &f,
        &["--part", "BACKEND", "--roles", " 로그인 ,결제 API,로그인"],
    ));
    assert_eq!(result["updated"], true);
    assert_eq!(result["changedFields"], json!(["part", "roles"]));
    assert_eq!(result["credentialsRotated"], false);
    assert_eq!(result["localProfileSynced"], true);
    assert_eq!(result["agent"]["part"], "BACKEND");
    assert_eq!(result["agent"]["roles"], tags);
    let after = profile(&f.path);
    assert_eq!(after["state"]["agent"]["part"], "BACKEND");
    assert_eq!(after["state"]["agent"]["roles"], tags);
    assert_eq!(after["state"]["token"], before["state"]["token"]);
    // Fields that were not passed are not sent and keep the central value.
    let kept = data(update(&f, &["--project-name", "Renamed"]));
    assert_eq!(kept["changedFields"], json!(["projectName"]));
    assert_eq!(kept["agent"]["part"], "BACKEND");
    assert_eq!(kept["agent"]["roles"], tags);
    // The same tags after trimming are not a change.
    let unchanged = std::fs::read(agent_profile::path(&f.path)).unwrap();
    let same = data(update(
        &f,
        &["--part", "BACKEND", "--roles", "로그인, 결제 API "],
    ));
    assert_eq!(same["updated"], false);
    assert_eq!(same["changedFields"], json!([]));
    assert_eq!(
        std::fs::read(agent_profile::path(&f.path)).unwrap(),
        unchanged
    );
    let cleared = data(update(&f, &["--clear-roles"]));
    assert_eq!(cleared["changedFields"], json!(["roles"]));
    assert_eq!(cleared["agent"]["roles"], json!([]));
    assert_eq!(cleared["agent"]["part"], "BACKEND");
    assert_eq!(profile(&f.path)["state"]["agent"]["roles"], json!([]));
    assert_eq!(data(update(&f, &["--clear-roles"]))["updated"], false);
    assert_eq!(
        update_calls(&f),
        [
            "agent_update:part,roles",
            "agent_update:projectName",
            "agent_update:roles"
        ]
    );
    // An older central has no part/roles and ignores them, so a real change must not be
    // reported as a success. Its missing fields read as OTHER and no tags, so asking for
    // exactly those defaults finds nothing to send.
    f.state.update_mode.store(5, Ordering::SeqCst);
    let stale = profile(&f.path);
    let calls = update_calls(&f).len();
    deny(update(&f, &["--part", "INFRA"]), "outcome is unknown");
    deny(update(&f, &["--roles", "login"]), "outcome is unknown");
    assert_eq!(update_calls(&f).len(), calls + 2);
    assert_eq!(stale, profile(&f.path));
    let defaults = data(update(&f, &["--part", "OTHER", "--clear-roles"]));
    assert_eq!(defaults["updated"], false);
    assert_eq!(update_calls(&f).len(), calls + 2);
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
}

async fn local(
    http: &reqwest::Client,
    mcp: &credentials::McpCredentials,
    path: &str,
    body: &Value,
) -> (u16, Value) {
    let response = http
        .post(format!("{}{path}", mcp.broker_url))
        .bearer_auth(&mcp.local_token)
        .json(body)
        .send()
        .await
        .unwrap();
    let status = response.status().as_u16();
    let value: Value = response.json().await.unwrap();
    for secret in [
        account_server::COOKIE,
        account_server::CSRF,
        account_server::AGENT_SECRET,
        mcp.local_token.as_str(),
    ] {
        assert!(!value.to_string().contains(secret), "{value}");
    }
    (status, value)
}

/// The MCP route edits only the verified session Agent through the CLI user login,
/// and the broker context then reports the new central values.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_update_agent_uses_the_cli_login_for_the_verified_session_agent_only() {
    let f = Fixture::new().await;
    let _stop = Stop(f.path.clone());
    selected(&f);
    data(create(&f));
    let token = profile(&f.path)["state"]["token"].clone();
    let started = f.run(&["start"], None);
    assert!(started.status.success(), "{}", text(&started));
    connected(&f.path).await;
    let mcp = credentials::load(&f.path).unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let (status, claimed) = local(
        &http,
        &mcp,
        "/v1/sessions",
        &json!({"requestId":"claim-update","instanceId":"update-instance","mode":"fresh","agentId":"201"}),
    )
    .await;
    assert_eq!(status, 200, "{claimed}");
    let mut context = claimed["data"]["principal"].clone();
    context["sessionId"] = claimed["data"]["sessionId"].clone();
    let request = |id: &str, args: Value| json!({"requestId":id,"operation":"update_agent","arguments":args,"context":context});
    for args in [
        json!({}),
        json!({"projectName":"   "}),
        json!({"projectName":"DevLink","name":"renamed"}),
        json!({"projectName":"DevLink","agentId":"301"}),
    ] {
        let (status, rejected) =
            local(&http, &mcp, "/v1/agents/update", &request("bad", args)).await;
        assert_eq!(status, 400, "{rejected}");
        assert_eq!(rejected["error"]["code"], "VALIDATION_FAILED");
    }
    assert!(update_calls(&f).is_empty());
    let (status, updated) = local(
        &http,
        &mcp,
        "/v1/agents/update",
        &request("update-1", json!({"projectName":"Broker project"})),
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    let result = &updated["data"];
    assert_eq!(result["agent"]["agentId"], "201");
    assert_eq!(result["agent"]["teamId"], "71");
    assert_eq!(result["agent"]["name"], "My Agent");
    assert_eq!(result["agent"]["toolType"], "codex");
    assert_eq!(result["agent"]["projectName"], "Broker project");
    assert_eq!(result["changedFields"], json!(["projectName"]));
    assert_eq!(result["credentialsRotated"], false);
    assert_eq!(result["localProfileSynced"], true);
    assert_eq!(update_calls(&f), ["agent_update:projectName"]);
    let after = profile(&f.path);
    assert_eq!(after["state"]["token"], token);
    assert_eq!(after["state"]["agent"]["projectName"], "Broker project");
    let read = http
        .get(format!("{}/v1/context", mcp.broker_url))
        .bearer_auth(&mcp.local_token)
        .header("x-request-id", "context-after-update")
        .header("x-instance-id", "update-instance")
        .send()
        .await
        .unwrap();
    assert_eq!(read.status().as_u16(), 200);
    let read: Value = read.json().await.unwrap();
    assert_eq!(read["data"]["agent"]["projectName"], "Broker project");
    let mut spoofed = request("spoofed", json!({"projectName":"Spoofed"}));
    spoofed["context"]["agentId"] = json!("301");
    let (status, denied) = local(&http, &mcp, "/v1/agents/update", &spoofed).await;
    assert!((400..500).contains(&status), "{denied}");
    assert_eq!(denied["error"]["outcome"], "rejected");
    assert_eq!(update_calls(&f).len(), 1);
    f.state.update_mode.store(2, Ordering::SeqCst);
    let (status, unknown) = local(
        &http,
        &mcp,
        "/v1/agents/update",
        &request("update-unknown", json!({"projectName":"Unconfirmed"})),
    )
    .await;
    assert_eq!(status, 502, "{unknown}");
    assert_eq!(unknown["error"]["code"], "CENTRAL_PROTOCOL_ERROR");
    assert_eq!(unknown["error"]["outcome"], "unknown");
    // A central 5xx may already be committed; 403/404 are definite rejections.
    for (mode, status, code, outcome) in [
        (1, 502, "CENTRAL_UNAVAILABLE", "unknown"),
        (3, 403, "FORBIDDEN", "rejected"),
        (4, 404, "RESOURCE_NOT_FOUND", "rejected"),
    ] {
        f.state.update_mode.store(mode, Ordering::SeqCst);
        let (actual, response) = local(
            &http,
            &mcp,
            "/v1/agents/update",
            &request(
                &format!("mode-{mode}"),
                json!({"projectName":format!("Mode {mode}")}),
            ),
        )
        .await;
        assert_eq!(actual, status, "{response}");
        assert_eq!(response["error"]["code"], code);
        assert_eq!(response["error"]["outcome"], outcome);
    }
    f.state.update_mode.store(0, Ordering::SeqCst);
    // A CLI operation holding the account lock rejects before any central call.
    let calls = update_calls(&f).len();
    let held =
        devlink_broker::config::try_lock(&config::state_directory(&f.path).join("account.lock"))
            .unwrap()
            .unwrap();
    let (status, busy) = local(
        &http,
        &mcp,
        "/v1/agents/update",
        &request("busy", json!({"projectName":"Busy"})),
    )
    .await;
    drop(held);
    assert_eq!(status, 409, "{busy}");
    assert_eq!(busy["error"]["code"], "LOCAL_ACCOUNT_BUSY");
    assert_eq!(busy["error"]["outcome"], "rejected");
    assert_eq!(update_calls(&f).len(), calls);
    data(f.run(&["auth", "logout"], None));
    let calls = update_calls(&f).len();
    let (status, login) = local(
        &http,
        &mcp,
        "/v1/agents/update",
        &request("update-2", json!({"roleDescription":"Reviewer"})),
    )
    .await;
    assert_eq!(status, 412, "{login}");
    assert_eq!(login["error"]["code"], "USER_LOGIN_REQUIRED");
    assert_eq!(login["error"]["outcome"], "rejected");
    assert_eq!(update_calls(&f).len(), calls);
    assert_eq!(profile(&f.path)["state"]["token"], token);
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
}

async fn update_fixture() -> (
    Fixture,
    Stop,
    credentials::McpCredentials,
    reqwest::Client,
    Value,
) {
    let f = Fixture::new().await;
    let stop = Stop(f.path.clone());
    selected(&f);
    data(create(&f));
    let started = f.run(&["start"], None);
    assert!(started.status.success(), "{}", text(&started));
    connected(&f.path).await;
    let mcp = credentials::load(&f.path).unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let (status, claimed) = local(&http, &mcp, "/v1/sessions", &json!({
        "requestId":"update-claim-a", "instanceId":"update-instance-a", "mode":"fresh", "agentId":"201"
    })).await;
    assert_eq!(status, 200, "{claimed}");
    let mut context = claimed["data"]["principal"].clone();
    context["sessionId"] = claimed["data"]["sessionId"].clone();
    (f, stop, mcp, http, context)
}

fn pending_update(
    http: &reqwest::Client,
    mcp: &credentials::McpCredentials,
    context: Value,
    project: &str,
) -> tokio::task::JoinHandle<(u16, Value)> {
    let client = http.clone();
    let url = format!("{}/v1/agents/update", mcp.broker_url);
    let token = mcp.local_token.clone();
    let body = json!({"requestId":"pending-update", "operation":"update_agent",
        "arguments":{"projectName":project}, "context":context});
    tokio::spawn(async move {
        let response = client
            .post(url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap())
    })
}

async fn replace_update_session(http: &reqwest::Client, mcp: &credentials::McpCredentials) {
    let (status, response) = local(http, mcp, "/v1/sessions", &json!({
        "requestId":"update-claim-b", "instanceId":"update-instance-b", "mode":"fresh", "agentId":"201"
    })).await;
    assert_eq!(status, 200, "{response}");
}

/// S15P31D108-183: the MCP route takes part/roles, treats null like an omitted field,
/// and the registration result then shows the stored metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_update_agent_changes_part_and_roles_and_keeps_null_or_omitted_values() {
    let (f, _stop, mcp, http, context) = update_fixture().await;
    let request = |id: &str, operation: &str, args: Value| json!({"requestId":id,"operation":operation,"arguments":args,"context":context});
    let eleven = (0..11).map(|i| format!("tag{i}")).collect::<Vec<_>>();
    for args in [
        json!({"part":null}),
        json!({"roles":null}),
        json!({"part":null,"roles":null}),
        json!({"part":"DESIGN"}),
        json!({"part":"backend"}),
        json!({"part":7}),
        json!({"roles":"login"}),
        json!({"roles":["  "]}),
        json!({"roles":["r".repeat(65)]}),
        json!({"roles":["\u{1}"]}),
        json!({"roles":["pay\u{0}ment"]}),
        json!({"roles":eleven}),
    ] {
        let (status, rejected) = local(
            &http,
            &mcp,
            "/v1/agents/update",
            &request("bad-metadata", "update_agent", args.clone()),
        )
        .await;
        assert_eq!(status, 400, "{args} {rejected}");
        assert_eq!(rejected["error"]["code"], "VALIDATION_FAILED", "{args}");
    }
    assert!(update_calls(&f).is_empty());
    let tags = json!(["로그인", "결제 API"]);
    let (status, updated) = local(
        &http,
        &mcp,
        "/v1/agents/update",
        &request(
            "metadata-1",
            "update_agent",
            json!({"part":"FRONTEND","roles":[" 로그인 ","결제 API","로그인"]}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["data"]["changedFields"], json!(["part", "roles"]));
    assert_eq!(updated["data"]["agent"]["part"], "FRONTEND");
    assert_eq!(updated["data"]["agent"]["roles"], tags);
    assert_eq!(updated["data"]["credentialsRotated"], false);
    // null and omitted fields are not sent, so central keeps both values.
    let (status, kept) = local(
        &http,
        &mcp,
        "/v1/agents/update",
        &request(
            "metadata-2",
            "update_agent",
            json!({"projectName":"Kept metadata","part":null,"roles":null}),
        ),
    )
    .await;
    assert_eq!(status, 200, "{kept}");
    assert_eq!(kept["data"]["changedFields"], json!(["projectName"]));
    assert_eq!(kept["data"]["agent"]["part"], "FRONTEND");
    assert_eq!(kept["data"]["agent"]["roles"], tags);
    let (status, confirmed) = local(
        &http,
        &mcp,
        "/v1/agents/register",
        &request("metadata-register", "register_agent", json!({})),
    )
    .await;
    assert_eq!(status, 200, "{confirmed}");
    assert_eq!(confirmed["data"]["part"], "FRONTEND");
    assert_eq!(confirmed["data"]["roles"], tags);
    let (status, cleared) = local(
        &http,
        &mcp,
        "/v1/agents/update",
        &request("metadata-3", "update_agent", json!({"roles":[]})),
    )
    .await;
    assert_eq!(status, 200, "{cleared}");
    assert_eq!(cleared["data"]["changedFields"], json!(["roles"]));
    assert_eq!(cleared["data"]["agent"]["roles"], json!([]));
    assert_eq!(cleared["data"]["agent"]["part"], "FRONTEND");
    assert_eq!(
        update_calls(&f),
        [
            "agent_update:part,roles",
            "agent_update:projectName",
            "agent_update:roles"
        ]
    );
    assert_eq!(profile(&f.path)["state"]["agent"]["roles"], json!([]));
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
}

/// Issue #86: every asynchronous preflight, including CSRF, must precede the
/// final session fence. A no-op must not publish a result to a replaced owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_fences_replaced_released_changed_context_and_binding_before_dispatch() {
    for (gate, action, project) in [
        ("members", "replace", "New project"),
        ("csrf", "replace", "New project"),
        ("members", "release", "New project"),
        ("csrf", "release", "New project"),
        ("members", "context", "New project"),
        ("csrf", "context", "New project"),
        ("members", "binding", "New project"),
        ("csrf", "binding", "New project"),
        ("members", "replace", "My project"),
    ] {
        let (f, _stop, mcp, http, context) = update_fixture().await;
        let (pause, entered, resume) = if gate == "members" {
            (
                &f.state.pause_members,
                &f.state.members_entered,
                &f.state.members_resume,
            )
        } else {
            (
                &f.state.pause_csrf,
                &f.state.csrf_entered,
                &f.state.csrf_resume,
            )
        };
        pause.store(true, Ordering::SeqCst);
        let pending = pending_update(&http, &mcp, context.clone(), project);
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        match action {
            "replace" => replace_update_session(&http, &mcp).await,
            "release" => {
                let response = http
                    .delete(format!(
                        "{}/v1/sessions/{}",
                        mcp.broker_url,
                        context["sessionId"].as_str().unwrap()
                    ))
                    .bearer_auth(&mcp.local_token)
                    .json(&json!({"requestId":"release-a", "instanceId":"update-instance-a"}))
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
            }
            "context" => f.state.wrong_context.store(true, Ordering::SeqCst),
            "binding" => {
                let root = f._root.path().join("changed-binding");
                std::fs::create_dir(&root).unwrap();
                let mut changed = profile(&f.path);
                changed["projectRoot"] = json!(std::fs::canonicalize(root).unwrap());
                private_file::write(&agent_profile::path(&f.path), &changed, 16384).unwrap();
            }
            _ => unreachable!(),
        }
        let before = std::fs::read(agent_profile::path(&f.path)).unwrap();
        assert!(update_calls(&f).is_empty());
        pause.store(false, Ordering::SeqCst);
        resume.notify_one();
        let (status, response) = pending.await.unwrap();
        assert!((400..500).contains(&status), "{gate}/{action}: {response}");
        assert_eq!(
            response["error"]["outcome"], "rejected",
            "{gate}/{action}: {response}"
        );
        assert!(update_calls(&f).is_empty(), "{gate}/{action}");
        assert_eq!(std::fs::read(agent_profile::path(&f.path)).unwrap(), before);
        assert_eq!(
            f.state.agents.lock().unwrap()[&201]["projectName"],
            "My project"
        );
        assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
        assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_preserves_confirmed_result_when_session_is_replaced_after_dispatch() {
    let (f, _stop, mcp, http, context) = update_fixture().await;
    f.state.pause_update_reply.store(true, Ordering::SeqCst);
    let pending = pending_update(&http, &mcp, context, "Committed project");
    tokio::time::timeout(Duration::from_secs(5), f.state.update_entered.notified())
        .await
        .unwrap();
    replace_update_session(&http, &mcp).await;
    f.state.update_resume.notify_one();
    let (status, response) = pending.await.unwrap();
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["updated"], true);
    assert_eq!(
        response["data"]["agent"]["projectName"],
        "Committed project"
    );
    assert_eq!(update_calls(&f), ["agent_update:projectName"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_reports_unsynced_profile_without_overwriting_concurrent_local_changes() {
    let (f, _stop, mcp, http, context) = update_fixture().await;
    let token = profile(&f.path)["state"]["token"].clone();
    f.state.pause_update_reply.store(true, Ordering::SeqCst);
    let pending = pending_update(&http, &mcp, context, "Central project");
    tokio::time::timeout(Duration::from_secs(5), f.state.update_entered.notified())
        .await
        .unwrap();
    let mut changed = profile(&f.path);
    changed["state"]["agent"]["projectName"] = json!("Concurrent local project");
    private_file::write(&agent_profile::path(&f.path), &changed, 16384).unwrap();
    let before = std::fs::read(agent_profile::path(&f.path)).unwrap();
    f.state.update_resume.notify_one();
    let (status, response) = pending.await.unwrap();
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["data"]["localProfileSynced"], false);
    assert_eq!(response["data"]["updated"], true);
    assert_eq!(response["data"]["agent"]["projectName"], "Central project");
    assert_eq!(std::fs::read(agent_profile::path(&f.path)).unwrap(), before);
    assert_eq!(profile(&f.path)["state"]["token"], token);
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 1);
}
