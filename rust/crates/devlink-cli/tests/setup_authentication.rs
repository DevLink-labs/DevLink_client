#[path = "support/account_server.rs"]
mod account_server;
use account_server::{COOKIE, CSRF, Fixture, PASSWORD, assert_no_secrets, text};
use devlink_broker::{
    agent_profile,
    config::{self, Config},
    credentials, private_file, user_auth,
};
use devlink_cli::setup::{
    self, Completion, LoginCredentials, LoginInput, LoginInputError, Outcome, RunStatus,
    SetupContext, SetupStages, StageStatus, Step,
};
use serde_json::{Value, json};
use std::{
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

async fn fixture() -> Fixture {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    f
}

fn parsed(output: &std::process::Output, exit: i32) -> Value {
    assert_eq!(output.status.code(), Some(exit), "{}", text(output));
    assert_no_secrets(output);
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn sign_in(f: &Fixture, password: &str) -> std::process::Output {
    f.run(
        &[
            "setup",
            "--email",
            "input@example.test",
            "--password-stdin",
            "--output",
            "json",
        ],
        Some(&format!("{password}\n")),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn valid_session_reuses_server_identity_without_reading_an_open_password_pipe() {
    let f = fixture().await;
    assert!(f.login().status.success());
    f.state.calls.lock().unwrap().clear();
    let (node, entry) =
        account_server::tool_fixture::mcp_runtime(&f._root.path().join("mcp-runtime"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(&f.path)
        .args([
            "setup",
            "--email",
            "unused@example.test",
            "--password-stdin",
            "--output",
            "json",
        ])
        .args(["--tool", "codex", "--tool-path"])
        .arg(account_server::test_tool())
        .arg("--node-path")
        .arg(node)
        .arg("--mcp-entry")
        .arg(entry)
        .arg("--project")
        .arg(f._root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("valid session waited for password input");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let value = parsed(&child.wait_with_output().unwrap(), 2);
    assert_eq!(value["stages"][1]["status"], "succeeded");
    assert_eq!(value["stages"][1]["reused"], true);
    assert_eq!(value["error"]["step"], "register_agent");
    assert_eq!(value["error"]["known"]["userId"], "51");
    assert_eq!(
        *f.state.calls.lock().unwrap(),
        [
            "me",
            "me",
            "team_list",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "agents_owned"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_session_logs_in_with_csrf_verifies_me_and_stores_only_the_session() {
    let f = fixture().await;
    let value = parsed(&sign_in(&f, PASSWORD), 2);
    assert_eq!(value["stages"][1]["status"], "succeeded");
    assert_eq!(value["stages"][1]["reused"], false);
    assert_eq!(value["error"]["known"]["userId"], "51");
    assert_eq!(
        *f.state.calls.lock().unwrap(),
        [
            "csrf",
            "login",
            "me",
            "csrf",
            "me",
            "team_list",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "agents_owned"
        ]
    );
    let saved: Value = private_file::read(&user_auth::session_path(&f.path), 16_384)
        .unwrap()
        .unwrap();
    assert_eq!(saved["state"]["user"]["email"], "verified@example.test");
    assert!(saved.to_string().contains(COOKIE));
    assert!(!saved.to_string().contains(PASSWORD));
    assert!(!saved.to_string().contains(CSRF));
    assert!(!credentials::path(&f.path).exists());
    assert!(!devlink_broker::agent_profile::path(&f.path).exists());
    assert!(!std::fs::read_to_string(&f.path).unwrap().contains(PASSWORD));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_expiry_allows_explicit_login_without_retrying_the_expired_cookie() {
    let f = fixture().await;
    assert!(f.login().status.success());
    f.state.expired.store(true, Ordering::SeqCst);
    f.state.calls.lock().unwrap().clear();
    let value = parsed(&sign_in(&f, PASSWORD), 2);
    assert_eq!(value["stages"][1]["reused"], false);
    assert_eq!(
        *f.state.calls.lock().unwrap(),
        [
            "me",
            "csrf",
            "login",
            "me",
            "csrf",
            "me",
            "team_list",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "agents_owned"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattended_missing_or_expired_sessions_return_input_descriptors_without_prompting() {
    let f = fixture().await;
    let value = parsed(&f.run(&["setup", "--output", "json"], None), 4);
    assert_eq!(value["error"]["code"], "SETUP_AUTH_REQUIRED");
    assert_eq!(value["error"]["requiredInputs"][1]["secret"], true);
    assert!(f.state.calls.lock().unwrap().is_empty());
    assert!(!user_auth::session_path(&f.path).exists());
    assert!(f.login().status.success());
    f.state.expired.store(true, Ordering::SeqCst);
    f.state.calls.lock().unwrap().clear();
    let value = parsed(&f.run(&["setup", "--output", "json"], None), 4);
    assert_eq!(value["error"]["outcome"], "applied");
    assert_eq!(*f.state.calls.lock().unwrap(), ["me"]);
    assert!(
        !std::fs::read_to_string(user_auth::session_path(&f.path))
            .unwrap()
            .contains(COOKIE)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_failure_preserves_the_session_and_does_not_request_credentials() {
    let f = fixture().await;
    assert!(f.login().status.success());
    let before = std::fs::read(user_auth::session_path(&f.path)).unwrap();
    f.state.me_failure.store(true, Ordering::SeqCst);
    f.state.calls.lock().unwrap().clear();
    let value = parsed(&sign_in(&f, "unused-password"), 7);
    assert_eq!(value["error"]["code"], "SETUP_CENTRAL_UNAVAILABLE");
    assert_eq!(*f.state.calls.lock().unwrap(), ["me"]);
    assert_eq!(
        std::fs::read(user_auth::session_path(&f.path)).unwrap(),
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnected_server_does_not_expire_the_saved_session() {
    let f = fixture().await;
    assert!(f.login().status.success());
    let before = std::fs::read(user_auth::session_path(&f.path)).unwrap();
    f.server.abort();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let value = parsed(&sign_in(&f, "unused-password"), 7);
    assert_eq!(value["error"]["code"], "SETUP_CENTRAL_UNAVAILABLE");
    assert_eq!(
        std::fs::read(user_auth::session_path(&f.path)).unwrap(),
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_credentials_fail_once_without_printing_reflected_secrets() {
    let f = fixture().await;
    let output = sign_in(&f, "wrong-private-password");
    let value = parsed(&output, 4);
    assert_eq!(value["error"]["code"], "SETUP_AUTH_REJECTED");
    assert!(!text(&output).contains("wrong-private-password"));
    assert_eq!(*f.state.calls.lock().unwrap(), ["csrf", "login"]);
    assert!(!user_auth::session_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn csrf_or_post_login_me_failure_is_unavailable_instead_of_bad_credentials() {
    for csrf_fails in [true, false] {
        let f = fixture().await;
        f.state.csrf_failure.store(csrf_fails, Ordering::SeqCst);
        f.state.me_failure.store(!csrf_fails, Ordering::SeqCst);
        let value = parsed(&sign_in(&f, PASSWORD), 7);
        assert_eq!(value["error"]["code"], "SETUP_CENTRAL_UNAVAILABLE");
        assert!(!user_auth::session_path(&f.path).exists());
        assert_eq!(
            f.state
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| *call == "login")
                .count(),
            usize::from(!csrf_fails)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_or_forbidden_status_does_not_become_expiry_or_trigger_login() {
    for (mode, exit, code) in [
        (1, 4, "SETUP_AUTH_REJECTED"),
        (2, 7, "SETUP_PROTOCOL_ERROR"),
    ] {
        let f = fixture().await;
        assert!(f.login().status.success());
        let before = std::fs::read(user_auth::session_path(&f.path)).unwrap();
        f.state.me_mode.store(mode, Ordering::SeqCst);
        f.state.calls.lock().unwrap().clear();
        let value = parsed(&sign_in(&f, PASSWORD), exit);
        assert_eq!(value["error"]["code"], code);
        assert_eq!(*f.state.calls.lock().unwrap(), ["me"]);
        assert_eq!(
            std::fs::read(user_auth::session_path(&f.path)).unwrap(),
            before
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mismatched_identity_is_not_published_as_an_authenticated_setup_user() {
    let f = fixture().await;
    f.state.changed_user.store(true, Ordering::SeqCst);
    let value = parsed(&sign_in(&f, PASSWORD), 7);
    assert_eq!(value["error"]["code"], "SETUP_PROTOCOL_ERROR");
    assert!(value["error"]["known"].get("userId").is_none());
    assert!(!user_auth::session_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_or_invalid_local_session_requires_explicit_recovery_not_a_password_prompt() {
    for foreign in [true, false] {
        let f = fixture().await;
        assert!(f.login().status.success());
        let path = user_auth::session_path(&f.path);
        if foreign {
            let mut config = Config::load(&f.path).unwrap();
            config.server_url = "ws://127.0.0.1:9".into();
            config::atomic_json(&f.path, &config).unwrap();
        } else {
            private_file::write(
                &path,
                &json!({"schemaVersion":2,"private":"do-not-print"}),
                16_384,
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        f.state.calls.lock().unwrap().clear();
        let value = parsed(&sign_in(&f, PASSWORD), if foreign { 6 } else { 5 });
        assert_eq!(value["error"]["step"], "authenticate");
        assert!(f.state.calls.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
}

struct InjectedInput {
    calls: Arc<AtomicUsize>,
    fail: bool,
    path: Option<std::path::PathBuf>,
    switch_origin: Option<String>,
}
impl LoginInput for InjectedInput {
    fn available(&self) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn request(&mut self) -> Result<LoginCredentials, LoginInputError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(LoginInputError::Cancelled);
        }
        if let Some(path) = &self.path {
            if let Some(origin) = &self.switch_origin {
                let mut config = Config::load(path).unwrap();
                config.server_url = origin.clone();
                config::atomic_json(path, &config).unwrap();
            } else {
                user_auth::login(path, "input@example.test", PASSWORD)
                    .await
                    .unwrap();
            }
        }
        LoginCredentials::new("input@example.test".into(), "unused-password".into())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_adapter_waits_outside_the_flow_and_reuses_a_session_created_during_input() {
    let f = fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let input = InjectedInput {
        calls: calls.clone(),
        fail: false,
        path: Some(f.path.clone()),
        switch_origin: None,
    };
    let mut events = Vec::new();
    let result = setup::run(
        &SetupContext {
            config_path: f.path.clone(),
        },
        SetupStages::new(input)
            .with_project(devlink_broker::setup_project::ProjectOptions {
                path: Some(f._root.path().to_path_buf()),
                ..Default::default()
            })
            .with_mcp(account_server::tool_fixture::mcp_options(
                &f._root.path().join("mcp-runtime"),
            ))
            .with_tool(
                Some(devlink_broker::sessions::ToolType::Codex),
                Some(account_server::test_tool().to_path_buf()),
            ),
        |event| {
            events.push(event.clone());
            Ok(())
        },
    )
    .await;
    assert_eq!(result.exit_code, 2);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(result.stages[1].reused);
    assert_eq!(
        f.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| *call == "login")
            .count(),
        1
    );
    let waiting = events
        .iter()
        .find(|event| event.step_status == StageStatus::WaitingInput)
        .unwrap();
    assert_eq!(waiting.status, RunStatus::WaitingInput);
    assert_eq!(waiting.required_inputs.len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(
                |event| event.step == Step::Validate && event.step_status == StageStatus::Running
            )
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_input_preserves_the_confirmed_expiry_and_skips_later_stages() {
    let f = fixture().await;
    assert!(f.login().status.success());
    f.state.expired.store(true, Ordering::SeqCst);
    let calls = Arc::new(AtomicUsize::new(0));
    let input = InjectedInput {
        calls: calls.clone(),
        fail: true,
        path: None,
        switch_origin: None,
    };
    let result = setup::run(
        &SetupContext {
            config_path: f.path.clone(),
        },
        SetupStages::new(input),
        |_| Ok(()),
    )
    .await;
    assert_eq!(result.exit_code, 130);
    assert_eq!(result.status, RunStatus::Cancelled);
    assert_eq!(result.stages[1].status, StageStatus::Cancelled);
    assert_eq!(result.stages[1].outcome, Outcome::Applied);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        result.stages[2..]
            .iter()
            .all(|stage| stage.status == StageStatus::Skipped)
    );
    let Completion::Failure { error } = result.completion else {
        panic!("expected cancellation")
    };
    assert_eq!(error.outcome, Outcome::Applied);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_setup_login_reuses_a_current_session_instead_of_submitting_unused_credentials() {
    let f = fixture().await;
    assert!(f.login().status.success());
    f.state.calls.lock().unwrap().clear();
    let origin = user_auth::origin(&Config::load(&f.path).unwrap()).unwrap();
    let login = user_auth::login_for_setup(
        &f.path,
        "unused@example.test",
        "wrong-unused-password",
        origin.as_str(),
    )
    .await
    .unwrap();
    assert!(login.reused);
    assert_eq!(login.user.user_id, 51);
    assert_eq!(*f.state.calls.lock().unwrap(), ["me"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_the_server_during_input_does_not_send_credentials_to_the_new_origin() {
    let first = fixture().await;
    let second = fixture().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let input = InjectedInput {
        calls: calls.clone(),
        fail: false,
        path: Some(first.path.clone()),
        switch_origin: Some(Config::load(&second.path).unwrap().server_url),
    };
    let result = setup::run(
        &SetupContext {
            config_path: first.path.clone(),
        },
        SetupStages::new(input),
        |_| Ok(()),
    )
    .await;
    assert_eq!(result.exit_code, 6);
    let Completion::Failure { error } = result.completion else {
        panic!("expected conflict")
    };
    assert_eq!(error.code, setup::ErrorCode::AuthSessionConflict);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(first.state.calls.lock().unwrap().is_empty());
    assert!(second.state.calls.lock().unwrap().is_empty());
    assert!(!user_auth::session_path(&first.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_lock_contention_is_not_misclassified_as_expiry() {
    let f = fixture().await;
    let _guard = config::lock(&config::state_directory(&f.path).join("account.lock")).unwrap();
    let value = parsed(&sign_in(&f, PASSWORD), 6);
    assert_eq!(value["error"]["code"], "SETUP_AUTH_BUSY");
    assert!(f.state.calls.lock().unwrap().is_empty());
    assert!(!user_auth::session_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_broker_during_user_switch_is_reported_without_publishing_a_session() {
    let f = fixture().await;
    let profile = agent_profile::path(&f.path);
    private_file::write(&profile, &json!({
        "schemaVersion":1,"origin":user_auth::origin(&Config::load(&f.path).unwrap()).unwrap().as_str(),
        "userId":999,"teamId":71,"projectRoot":f.path.parent().unwrap(),
        "state":{"kind":"create_pending","name":"Pending","tool":"CODEX","project_name":"Project"}
    }), 16_384).unwrap();
    let before = std::fs::read(&profile).unwrap();
    let broker = config::state_directory(&f.path).join("broker.lock");
    let _guard = config::lock(&broker).unwrap();
    let value = parsed(&sign_in(&f, PASSWORD), 6);
    assert_eq!(value["error"]["code"], "SETUP_BROKER_BUSY");
    assert_eq!(value["error"]["outcome"], "not_applied");
    assert_eq!(value["error"]["location"]["file"], broker.to_str().unwrap());
    assert_eq!(std::fs::read(&profile).unwrap(), before);
    assert!(!user_auth::session_path(&f.path).exists());
    assert!(
        f.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "login")
    );
}

/// Changes local state while setup waits for credentials, after validate has passed.
struct MutatingInput {
    path: std::path::PathBuf,
    mutate: fn(&std::path::Path),
}
impl LoginInput for MutatingInput {
    fn available(&self) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn request(&mut self) -> Result<LoginCredentials, LoginInputError> {
        (self.mutate)(&self.path);
        LoginCredentials::new("input@example.test".into(), PASSWORD.into())
    }
}

async fn run_with_mutation(f: &Fixture, mutate: fn(&std::path::Path)) -> Box<setup::SetupError> {
    let input = MutatingInput {
        path: f.path.clone(),
        mutate,
    };
    let result = setup::run(
        &SetupContext {
            config_path: f.path.clone(),
        },
        SetupStages::new(input),
        |_| Ok(()),
    )
    .await;
    let Completion::Failure { error } = result.completion else {
        panic!("expected a setup failure")
    };
    assert_eq!(result.exit_code, error.code.exit_code());
    error
}

fn rewrite_config(path: &std::path::Path, edit: fn(&mut Config)) {
    let mut config = Config::load(path).unwrap();
    edit(&mut config);
    config::atomic_json(path, &config).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_changed_during_input_is_reported_as_config_instead_of_session_storage() {
    type Mutation = fn(&std::path::Path);
    let cases: [(Mutation, setup::ErrorCode, Option<&str>); 4] = [
        (
            |path| {
                rewrite_config(path, |c| {
                    c.server_url = "invalid-private-config-marker".into()
                })
            },
            setup::ErrorCode::ConfigInvalid,
            Some("serverUrl"),
        ),
        (
            |path| rewrite_config(path, |c| c.server_url = "ws://central.example.test".into()),
            setup::ErrorCode::ConfigInvalid,
            Some("serverUrl"),
        ),
        (
            |path| rewrite_config(path, |c| c.auth_timeout_ms = 5),
            setup::ErrorCode::ConfigInvalid,
            Some("authTimeoutMs"),
        ),
        (
            |path| std::fs::remove_file(path).unwrap(),
            setup::ErrorCode::ConfigStorageFailed,
            None,
        ),
    ];
    for (mutate, code, field) in cases {
        let f = fixture().await;
        let error = run_with_mutation(&f, mutate).await;
        assert_eq!(error.code, code);
        assert_eq!(error.step, Step::Authenticate);
        assert_eq!(error.outcome, Outcome::NotApplied);
        let location = error.location.unwrap();
        assert_eq!(location.file, f.path);
        assert_eq!(location.field, field);
        assert!(f.state.calls.lock().unwrap().is_empty());
        assert!(!user_auth::session_path(&f.path).exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsafe_broker_lock_during_user_switch_points_at_the_lock_not_the_session() {
    let f = fixture().await;
    let profile = agent_profile::path(&f.path);
    private_file::write(&profile, &json!({
        "schemaVersion":1,"origin":user_auth::origin(&Config::load(&f.path).unwrap()).unwrap().as_str(),
        "userId":999,"teamId":71,"projectRoot":f.path.parent().unwrap(),
        "state":{"kind":"create_pending","name":"Pending","tool":"CODEX","project_name":"Project"}
    }), 16_384).unwrap();
    let before = std::fs::read(&profile).unwrap();
    let error = run_with_mutation(&f, |path| {
        let lock = config::state_directory(path).join("broker.lock");
        let _ = std::fs::remove_file(&lock);
        std::fs::create_dir(&lock).unwrap();
    })
    .await;
    assert_eq!(error.code, setup::ErrorCode::UnsafeFile);
    assert_eq!(error.outcome, Outcome::NotApplied);
    assert_eq!(
        error.location.unwrap().file,
        config::state_directory(&f.path).join("broker.lock")
    );
    assert_eq!(std::fs::read(&profile).unwrap(), before);
    assert!(!user_auth::session_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_stdin_eof_cancels_but_an_empty_line_is_invalid_input() {
    let f = fixture().await;
    for (input, exit, code) in [
        ("", 130, "SETUP_CANCELLED"),
        ("\n", 2, "SETUP_INPUT_INVALID"),
    ] {
        let value = parsed(
            &f.run(
                &[
                    "setup",
                    "--email",
                    "input@example.test",
                    "--password-stdin",
                    "--output",
                    "json",
                ],
                Some(input),
            ),
            exit,
        );
        assert_eq!(value["error"]["code"], code);
        assert_eq!(value["error"]["outcome"], "not_applied");
    }
    assert!(f.state.calls.lock().unwrap().is_empty());
    assert!(!user_auth::session_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_remote_csrf_or_cookie_is_a_protocol_error_without_publishing_a_session() {
    for mode in [1, 2] {
        let f = fixture().await;
        f.state.csrf_mode.store(mode, Ordering::SeqCst);
        let value = parsed(&sign_in(&f, PASSWORD), 7);
        assert_eq!(value["error"]["code"], "SETUP_PROTOCOL_ERROR");
        assert_eq!(*f.state.calls.lock().unwrap(), ["csrf"]);
        assert!(!user_auth::session_path(&f.path).exists());
    }
    let f = fixture().await;
    assert!(f.login().status.success());
    let before = std::fs::read(user_auth::session_path(&f.path)).unwrap();
    f.state.me_mode.store(3, Ordering::SeqCst);
    f.state.calls.lock().unwrap().clear();
    let value = parsed(&sign_in(&f, PASSWORD), 7);
    assert_eq!(value["error"]["code"], "SETUP_PROTOCOL_ERROR");
    assert_eq!(*f.state.calls.lock().unwrap(), ["me"]);
    assert_eq!(
        std::fs::read(user_auth::session_path(&f.path)).unwrap(),
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn password_arguments_are_rejected_without_echoing_the_value() {
    let f = fixture().await;
    let output = f.run(&["setup", "--password", "must-never-print"], None);
    assert_eq!(output.status.code(), Some(2));
    assert!(!text(&output).contains("must-never-print"));
    assert_no_secrets(&output);
    assert!(f.state.calls.lock().unwrap().is_empty());
}
