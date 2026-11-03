#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, text};
use devlink_broker::{
    agent_profile::{
        self,
        registration::{self, FailureKind, Mutation, RegistrationContext, Selection},
        tokens,
    },
    config, credentials, private_file,
    sessions::ToolType,
    setup_project::ProjectOptions,
    user_auth,
};
use devlink_cli::setup::{
    ErrorCode, NoLoginInput, Outcome, SetupContext, SetupStages, Step, StepExecutor, StepResult,
};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::atomic::Ordering};

async fn fixture() -> (Fixture, SetupStages, RegistrationContext) {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    assert!(f.login().status.success());
    let mut stages = SetupStages::new(NoLoginInput)
        .with_project(ProjectOptions {
            path: Some(f._root.path().to_path_buf()),
            ..Default::default()
        })
        .with_tool(
            Some(ToolType::Codex),
            Some(account_server::test_tool().to_path_buf()),
        );
    for step in [
        Step::Validate,
        Step::Authenticate,
        Step::SelectTeam,
        Step::BindProject,
        Step::SelectTool,
    ] {
        assert!(matches!(
            stages
                .execute(
                    step,
                    &SetupContext {
                        config_path: f.path.clone()
                    }
                )
                .await,
            StepResult::Complete { .. }
        ));
    }
    let expected = RegistrationContext {
        origin: user_auth::origin(&config::Config::load(&f.path).unwrap())
            .unwrap()
            .to_string(),
        user_id: 51,
        team_id: 71,
        project: stages.project().unwrap().clone(),
        tool: ToolType::Codex,
    };
    (f, stages, expected)
}
fn profile(f: &Fixture) -> Value {
    private_file::read(&agent_profile::path(&f.path), 16_384)
        .unwrap()
        .unwrap()
}
fn counts(f: &Fixture) -> (usize, usize) {
    (
        f.state.create_calls.load(Ordering::SeqCst),
        f.state.token_calls.load(Ordering::SeqCst),
    )
}
async fn ready(f: &Fixture, expected: &RegistrationContext) {
    let metadata = &expected.project.metadata;
    agent_profile::create(
        &f.path,
        agent_profile::CreateOptions {
            name: metadata.agent_name.clone(),
            role_description: metadata.role_description.clone(),
            tool: agent_profile::Tool::Codex,
            project_name: metadata.project_name.clone(),
            project: expected.project.root.clone(),
        },
    )
    .await
    .unwrap();
}
async fn unissued(f: &Fixture, expected: &RegistrationContext) {
    assert_eq!(
        registration::ensure(&f.path, expected, Some(Selection::Create))
            .await
            .unwrap()
            .agent_id,
        Some(201)
    );
}
fn failed(result: StepResult) -> devlink_cli::setup::SetupError {
    match result {
        StepResult::Failed(error) => error,
        _ => panic!("expected a setup failure"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_tokens_are_reused_byte_for_byte_even_with_issuance_permission() {
    let (f, _, expected) = fixture().await;
    ready(&f, &expected).await;
    let mcp = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    let modified = fs::metadata(agent_profile::path(&f.path))
        .unwrap()
        .modified()
        .unwrap();
    for allow in [false, true] {
        let reused = tokens::ensure(&f.path, &expected, 201, allow)
            .await
            .unwrap();
        assert!(reused.verified && reused.reused && !reused.issued);
        assert_eq!(reused.outcome, Mutation::NotApplied);
        assert_eq!(reused.agent_id, "201");
        assert!(
            !serde_json::to_string(&reused)
                .unwrap()
                .contains(account_server::AGENT_SECRET)
        );
        assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
        assert_eq!(
            fs::metadata(agent_profile::path(&f.path))
                .unwrap()
                .modified()
                .unwrap(),
            modified
        );
    }
    assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    assert_eq!(counts(&f), (1, 1));
    assert!(f.state.agent_context_calls.load(Ordering::SeqCst) >= 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_token_requires_permission_then_is_verified_before_ready_without_new_agent() {
    let (f, _, expected) = fixture().await;
    unissued(&f, &expected).await;
    let needed = tokens::ensure(&f.path, &expected, 201, false)
        .await
        .unwrap();
    assert!(!needed.verified && !needed.issued);
    assert_eq!(counts(&f), (1, 0));
    let issued = tokens::ensure(&f.path, &expected, 201, true).await.unwrap();
    assert!(issued.verified && issued.issued && !issued.reused);
    assert_eq!(issued.outcome, Mutation::Applied);
    assert_eq!(profile(&f)["state"]["kind"], "ready");
    assert_eq!(counts(&f), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_token_stops_in_registration_and_running_broker_blocks_explicit_issuance() {
    let (f, stages, expected) = fixture().await;
    ready(&f, &expected).await;
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    f.state.tokens.lock().unwrap().clear();
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = stages;
    let error = failed(stages.execute(Step::RegisterAgent, &context).await);
    assert_eq!(error.code, ErrorCode::TokenRequired);
    assert_eq!(error.known.agent_id.as_deref(), Some("201"));
    assert_eq!(error.required_inputs[0].field, "agent.issueToken");
    assert_eq!(error.outcome, Outcome::NotApplied);
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    let broker = config::lock(&config::state_directory(&f.path).join("broker.lock")).unwrap();
    let mut stages = stages.with_issue_token(true);
    assert_eq!(
        failed(stages.execute(Step::RegisterAgent, &context).await).code,
        ErrorCode::BrokerBusy
    );
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(counts(&f), (1, 1));
    drop(broker);
    let mut stages = stages.with_issue_token(true);
    assert!(matches!(
        stages.execute(Step::RegisterAgent, &context).await,
        StepResult::Complete {
            outcome: Outcome::Applied,
            ..
        }
    ));
    assert_ne!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(counts(&f), (1, 2));
    let mut stages = stages.with_issue_token(true);
    assert!(matches!(
        stages.execute(Step::RegisterAgent, &context).await,
        StepResult::Complete {
            outcome: Outcome::NotApplied,
            ..
        }
    ));
    assert_eq!(counts(&f), (1, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verification_outage_malformed_redirect_and_wrong_subject_never_authorize_issuance() {
    let (f, _, expected) = fixture().await;
    ready(&f, &expected).await;
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    for (mode, kind) in [
        (1, FailureKind::Unavailable),
        (2, FailureKind::Protocol),
        (3, FailureKind::Protocol),
    ] {
        f.state.agent_context_mode.store(mode, Ordering::SeqCst);
        let error = tokens::ensure(&f.path, &expected, 201, true)
            .await
            .unwrap_err();
        assert_eq!(error.kind, kind);
        assert_eq!(error.agent_id, Some(201));
        assert_eq!(error.outcome, Mutation::NotApplied);
        assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    }
    f.state.agent_context_mode.store(0, Ordering::SeqCst);
    f.state.wrong_context.store(true, Ordering::SeqCst);
    assert_eq!(
        tokens::ensure(&f.path, &expected, 201, true)
            .await
            .unwrap_err()
            .kind,
        FailureKind::Protocol
    );
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(counts(&f), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_issuance_preserves_known_id_and_consumes_the_run_grant_without_replay() {
    let (f, stages, expected) = fixture().await;
    unissued(&f, &expected).await;
    f.state.token_mode.store(1, Ordering::SeqCst);
    let mut stages = stages.with_issue_token(true);
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let error = failed(stages.execute(Step::RegisterAgent, &context).await);
    assert_eq!(error.code, ErrorCode::TokenRequired);
    assert_eq!(error.outcome, Outcome::Unknown);
    assert_eq!(error.known.agent_id.as_deref(), Some("201"));
    assert_eq!(profile(&f)["state"]["kind"], "needs_token");
    assert!(profile(&f)["state"].get("token").is_none());
    assert_eq!(counts(&f), (1, 1));
    assert_eq!(
        failed(stages.execute(Step::RegisterAgent, &context).await).code,
        ErrorCode::TokenRequired
    );
    assert_eq!(counts(&f), (1, 1));
    f.state.token_mode.store(0, Ordering::SeqCst);
    assert!(
        tokens::ensure(&f.path, &expected, 201, true)
            .await
            .unwrap()
            .verified
    );
    assert_eq!(counts(&f), (1, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_issued_identity_and_unverifiable_token_never_publish_ready() {
    for mode in [2, 4, 9] {
        let (f, _, expected) = fixture().await;
        unissued(&f, &expected).await;
        if mode == 9 {
            f.state.wrong_context.store(true, Ordering::SeqCst);
        } else {
            f.state.token_mode.store(mode, Ordering::SeqCst);
        }
        let unknown = tokens::ensure(&f.path, &expected, 201, true).await.unwrap();
        assert!(!unknown.verified && !unknown.issued);
        assert_eq!(unknown.outcome, Mutation::Unknown);
        assert_eq!(profile(&f)["state"]["kind"], "needs_token");
        assert!(profile(&f)["state"].get("token").is_none());
        assert!(
            !serde_json::to_string(&unknown)
                .unwrap()
                .contains(account_server::AGENT_SECRET)
        );
        assert_eq!(counts(&f), (1, 1));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn private_publication_failure_after_issuance_remains_unknown_without_restoring_old_token() {
    let (f, _, expected) = fixture().await;
    unissued(&f, &expected).await;
    private_file::write(
        &credentials::path(&f.path),
        &json!({"invalid":"private-marker"}),
        16_384,
    )
    .unwrap();
    let unknown = tokens::ensure(&f.path, &expected, 201, true).await.unwrap();
    assert!(!unknown.verified);
    assert_eq!(unknown.outcome, Mutation::Unknown);
    assert_eq!(profile(&f)["state"]["kind"], "needs_token");
    assert_eq!(counts(&f), (1, 1));
    assert!(
        !tokens::ensure(&f.path, &expected, 201, false)
            .await
            .unwrap()
            .verified
    );
    assert_eq!(counts(&f), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issue_holds_locks_and_durably_removes_old_token_before_the_post_response() {
    let (f, _, expected) = fixture().await;
    ready(&f, &expected).await;
    f.state.tokens.lock().unwrap().clear();
    f.state.pause_token.store(true, Ordering::SeqCst);
    let path = f.path.clone();
    let expected = expected.clone();
    let issue = tokio::spawn(async move { tokens::ensure(&path, &expected, 201, true).await });
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        f.state.token_entered.notified(),
    )
    .await
    .unwrap();
    assert_eq!(profile(&f)["state"]["kind"], "needs_token");
    assert!(profile(&f)["state"].get("token").is_none());
    for name in ["account.lock", "control.lock", "broker.lock"] {
        assert!(
            config::try_lock(&config::state_directory(&f.path).join(name))
                .unwrap()
                .is_none()
        );
    }
    f.state.token_resume.notify_one();
    assert!(issue.await.unwrap().unwrap().verified);
    assert_eq!(counts(&f), (1, 2));
}

struct Stop(PathBuf);
impl Drop for Stop {
    fn drop(&mut self) {
        let _ = account_server::run_at(&self.0, &["stop"], None);
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_initial_create_issues_once_and_reuses_a_running_broker_and_verified_token() {
    let (f, _, _) = fixture().await;
    let _stop = Stop(f.path.clone());
    for flag in [Some("--create-agent"), None, Some("--issue-token")] {
        let mut args = vec![
            "setup",
            "--output",
            "json",
            "--project",
            f._root.path().to_str().unwrap(),
        ];
        if let Some(flag) = flag {
            args.push(flag);
        }
        let output = f.run(&args, None);
        assert_eq!(output.status.code(), Some(3), "{}", text(&output));
        assert_no_secrets(&output);
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["step"], "configure_mcp");
        assert_eq!(value["error"]["known"]["agentId"], "201");
        assert_eq!(value["stages"][5]["status"], "succeeded");
        assert_eq!(value["stages"][6]["status"], "succeeded");
        assert_eq!(profile(&f)["state"]["kind"], "ready");
        assert_eq!(counts(&f), (1, 1));
    }
    assert!(f.run(&["stop"], None).status.success());
    assert!(!devlink_broker::local::running(&f.path).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_issue_permission_cannot_be_combined_with_create_and_is_not_secret_input() {
    let (f, _, _) = fixture().await;
    let output = f.run(&["setup", "--create-agent", "--issue-token"], None);
    assert_eq!(output.status.code(), Some(2));
    assert_no_secrets(&output);
    assert_eq!(counts(&f), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issuance_permission_requires_valid_csrf_and_respects_confirmed_denial() {
    let (f, _, expected) = fixture().await;
    unissued(&f, &expected).await;
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    f.state.csrf_failure.store(true, Ordering::SeqCst);
    let error = tokens::ensure(&f.path, &expected, 201, true)
        .await
        .unwrap_err();
    assert_eq!(error.kind, FailureKind::Unavailable);
    assert_eq!(error.outcome, Mutation::NotApplied);
    assert_eq!(error.agent_id, Some(201));
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(f.state.token_requests.load(Ordering::SeqCst), 0);
    f.state.csrf_failure.store(false, Ordering::SeqCst);
    f.state.token_mode.store(3, Ordering::SeqCst);
    let error = tokens::ensure(&f.path, &expected, 201, true)
        .await
        .unwrap_err();
    assert_eq!(error.kind, FailureKind::Forbidden);
    assert_eq!(error.outcome, Mutation::Applied);
    assert_eq!(error.agent_id, Some(201));
    assert_eq!(profile(&f)["state"]["kind"], "needs_token");
    assert_eq!(f.state.token_requests.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&f), (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn issuance_permission_rechecks_frozen_user_team_project_and_tool() {
    let (f, _, expected) = fixture().await;
    unissued(&f, &expected).await;
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    let mut wrong = expected.clone();
    wrong.user_id = 52;
    assert_eq!(
        tokens::ensure(&f.path, &wrong, 201, true)
            .await
            .unwrap_err()
            .kind,
        FailureKind::SessionConflict
    );
    wrong = expected.clone();
    wrong.team_id = 72;
    assert_eq!(
        tokens::ensure(&f.path, &wrong, 201, true)
            .await
            .unwrap_err()
            .kind,
        FailureKind::SessionConflict
    );
    wrong = expected.clone();
    wrong.tool = ToolType::ClaudeCode;
    assert_eq!(
        tokens::ensure(&f.path, &wrong, 201, true)
            .await
            .unwrap_err()
            .kind,
        FailureKind::BindingConflict
    );
    wrong = expected.clone();
    wrong.project.root = f._root.path().join("missing project");
    assert_eq!(
        tokens::ensure(&f.path, &wrong, 201, true)
            .await
            .unwrap_err()
            .kind,
        FailureKind::BindingConflict
    );
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(counts(&f), (1, 0));
    assert_eq!(f.state.token_requests.load(Ordering::SeqCst), 0);
}
