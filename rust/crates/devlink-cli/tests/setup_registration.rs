#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, text};
use devlink_broker::{
    agent_profile::{
        self,
        registration::{
            self, FailureKind, Mutation, RegistrationContext, RegistrationState as R, Selection,
        },
    },
    config, credentials, private_file,
    sessions::ToolType,
    setup_project::ProjectOptions,
    user_auth,
};
use devlink_cli::setup::{
    self, AgentInput, AgentInputError, NoLoginInput, Outcome, SetupContext, SetupStages, Step,
    StepExecutor, StepResult,
};
use serde_json::{Value, json};
use std::{fs, sync::atomic::Ordering};

async fn fixture() -> (Fixture, SetupStages, RegistrationContext) {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    assert!(f.login().status.success());
    let mut stages = SetupStages::new(NoLoginInput)
        .with_mcp(account_server::tool_fixture::mcp_options(
            &f._root.path().join("mcp-runtime"),
        ))
        .with_project(ProjectOptions {
            path: Some(f._root.path().to_path_buf()),
            ..Default::default()
        })
        .with_tool(
            Some(ToolType::Codex),
            Some(account_server::test_tool().to_path_buf()),
        );
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    for step in [
        Step::Validate,
        Step::Authenticate,
        Step::SelectTeam,
        Step::BindProject,
        Step::SelectTool,
    ] {
        let result = stages.execute(step, &context).await;
        assert!(matches!(result, StepResult::Complete { .. }));
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

fn seed_pr37_agent(f: &Fixture, expected: &RegistrationContext, id: u64, owner: u64, tool: &str) {
    f.state.agents.lock().unwrap().insert(
        id,
        json!({
            "agentId":id,"teamId":expected.team_id,"ownerUserId":owner,
            "name":expected.project.metadata.agent_name,"roleDescription":"server profile",
            "toolType":tool,"projectName":"Different project display name",
            "status":"OFFLINE","lastSeenAt":null,"createdAt":"2026-11-03T00:00:00",
            "part":"OTHER","roles":[]
        }),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pr37_duplicate_requires_explicit_owned_id_and_rechecks_owner_and_tool() {
    let (f, stages, expected) = fixture().await;
    let id = i64::MAX as u64 - 1;
    seed_pr37_agent(&f, &expected, id, 51, "CODEX");
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = stages.with_agent(Some(Selection::Create));
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::AgentAlreadyExists);
    assert_eq!(error.code.exit_code(), 6);
    assert_eq!(error.outcome, Outcome::NotApplied);
    assert_eq!(error.recovery_action(), "rename_agent_or_select_owned_id");
    assert_eq!(error.known.agent_id, None);
    assert!(
        error
            .agent_choices
            .iter()
            .any(|agent| agent.id == id.to_string())
    );
    assert!(error.agent_list_error.is_none());
    assert_eq!(profile(&f)["state"]["kind"], "vacant");
    assert_eq!(counts(&f), (1, 0));

    // The list is advisory: ownership and tool are checked again when an ID is selected.
    f.state.agents.lock().unwrap().get_mut(&id).unwrap()["ownerUserId"] = json!(88);
    stages = stages.with_agent(Some(Selection::Use(id)));
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::AgentForbidden);
    assert_eq!(profile(&f)["state"]["kind"], "vacant");
    {
        let mut agents = f.state.agents.lock().unwrap();
        let agent = agents.get_mut(&id).unwrap();
        agent["ownerUserId"] = json!(51);
        agent["toolType"] = json!("CLAUDE_CODE");
    }
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::AgentBindingConflict);
    assert_eq!(profile(&f)["state"]["kind"], "vacant");
    f.state.agents.lock().unwrap().get_mut(&id).unwrap()["toolType"] = json!("CODEX");
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::TokenRequired);
    assert_eq!(
        error.known.agent_id.as_deref(),
        Some(id.to_string().as_str())
    );
    assert_eq!(profile(&f)["state"]["agent"]["agentId"], id);
    assert_eq!(
        profile(&f)["state"]["agent"]["projectName"],
        "Different project display name"
    );
    assert_eq!(counts(&f), (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pr37_foreign_duplicate_is_safe_in_cli_and_unrelated_409_is_not_relabelled() {
    let (f, _, expected) = fixture().await;
    seed_pr37_agent(&f, &expected, 501, 88, "CODEX");
    let run = || {
        f.run(
            &[
                "setup",
                "--output",
                "json",
                "--project",
                f._root.path().to_str().unwrap(),
                "--create-agent",
            ],
            None,
        )
    };
    let output = run();
    assert_eq!(output.status.code(), Some(6), "{}", text(&output));
    assert_no_secrets(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    let error = &result["error"];
    assert_eq!(error["code"], "SETUP_AGENT_ALREADY_EXISTS");
    assert_eq!(error["outcome"], "not_applied");
    assert_eq!(error["recovery"]["automatic"], false);
    assert!(error["known"].get("agentId").is_none());
    let choices = error["agentChoices"].as_array().unwrap();
    assert_eq!(choices.len(), 1); // Still never bind automatically, even with one owned choice.
    assert_eq!(choices[0]["id"], "301");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("server profile"));
    assert_eq!(counts(&f), (1, 0));
    assert_eq!(profile(&f)["state"]["kind"], "vacant");

    // A failed follow-up list must preserve the known rejection and not expose a partial list.
    f.state.agent_list_mode.store(1, Ordering::SeqCst);
    let output = run();
    assert_no_secrets(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "SETUP_AGENT_ALREADY_EXISTS");
    assert_eq!(
        result["error"]["agentListError"],
        "SETUP_CENTRAL_UNAVAILABLE"
    );
    assert!(result["error"].get("agentChoices").is_none());

    f.state.create_mode.store(4, Ordering::SeqCst);
    let output = run();
    assert_eq!(output.status.code(), Some(4), "{}", text(&output));
    assert_no_secrets(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["error"]["code"], "SETUP_AGENT_REJECTED");
    assert_eq!(counts(&f), (3, 0));

    // Renaming is an explicit new request, not a replay of the duplicate.
    f.state.create_mode.store(0, Ordering::SeqCst);
    let mut renamed = expected.clone();
    renamed.project.metadata.agent_name.push_str(" renamed");
    assert!(
        registration::ensure(&f.path, &renamed, Some(Selection::Create))
            .await
            .unwrap()
            .created
    );
    assert!(
        registration::ensure(&f.path, &renamed, Some(Selection::Create))
            .await
            .unwrap()
            .reused
    );
    assert_eq!(counts(&f), (4, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pr37_unknown_creation_preserves_pending_when_list_fails_then_recovers_by_id() {
    let (f, stages, _) = fixture().await;
    f.state.create_mode.store(1, Ordering::SeqCst);
    f.state.agent_list_mode.store(1, Ordering::SeqCst);
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = stages.with_agent(Some(Selection::Create));
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::RegistrationUnknown);
    assert_eq!(error.outcome, Outcome::Unknown);
    assert_eq!(
        error.agent_list_error,
        Some(setup::ErrorCode::CentralUnavailable)
    );
    let pending = fs::read(agent_profile::path(&f.path)).unwrap();
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::RegistrationUnknown);
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), pending);
    assert_eq!(counts(&f), (1, 0));
    f.state.create_mode.store(0, Ordering::SeqCst);
    f.state.agent_list_mode.store(0, Ordering::SeqCst);
    stages = stages.with_agent(None);
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::RegistrationUnknown);
    assert!(error.agent_choices.iter().any(|agent| agent.id == "201"));
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), pending);
    stages = stages.with_agent(Some(Selection::Use(201)));
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::TokenRequired);
    assert_eq!(error.known.agent_id.as_deref(), Some("201"));
    assert_eq!(counts(&f), (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pr37_owned_choices_validate_the_filter_and_reach_progress_and_explicit_input() {
    let (f, mut stages, expected) = fixture().await;
    seed_pr37_agent(&f, &expected, 501, 51, "CODEX");
    seed_pr37_agent(&f, &expected, 502, 51, "CLAUDE_CODE");
    let agents = registration::owned_agents(&f.path, &expected)
        .await
        .unwrap();
    assert_eq!(
        agents
            .iter()
            .map(|agent| agent.agent_id)
            .collect::<Vec<_>>(),
        vec![301, 501]
    );
    assert!(
        f.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "agents_owned")
    );
    assert!(!agent_profile::path(&f.path).exists());
    for mode in [2, 3, 4] {
        // ignored ownedByMe, duplicate IDs, another team
        f.state.agent_list_mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            registration::owned_agents(&f.path, &expected)
                .await
                .unwrap_err()
                .kind,
            FailureKind::Protocol
        );
    }
    f.state.agent_list_mode.store(1, Ordering::SeqCst);
    let StepResult::Failed(error) = stages
        .execute(
            Step::RegisterAgent,
            &SetupContext {
                config_path: f.path.clone(),
            },
        )
        .await
    else {
        panic!("A failed initial list must not become an empty selection")
    };
    assert_eq!(error.code, setup::ErrorCode::CentralUnavailable);
    assert!(!agent_profile::path(&f.path).exists());
    assert_eq!(counts(&f), (0, 0));
    f.state.agent_list_mode.store(0, Ordering::SeqCst);
    struct PickOwned;
    impl AgentInput for PickOwned {
        fn available(&self) -> bool {
            true
        }
        fn interactive(&self) -> bool {
            true
        }
        async fn request(
            &mut self,
            choices: &[setup::AgentChoice],
        ) -> Result<Selection, AgentInputError> {
            assert_eq!(
                choices
                    .iter()
                    .map(|agent| agent.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["301", "501"]
            );
            Ok(Selection::Use(501))
        }
    }
    let mut events = Vec::new();
    let result = setup::run(
        &SetupContext {
            config_path: f.path.clone(),
        },
        stages.with_agent_input(PickOwned),
        |event| {
            events.push(event.clone());
            Ok(())
        },
    )
    .await;
    let waiting = events
        .iter()
        .find(|event| {
            event.step == Step::RegisterAgent
                && event.step_status == setup::StageStatus::WaitingInput
        })
        .unwrap();
    assert_eq!(waiting.agent_choices.len(), 2);
    assert_eq!(waiting.agent_choices[1].id, "501");
    let setup::Completion::Failure { error } = result.completion else {
        panic!()
    };
    assert_eq!(error.code, setup::ErrorCode::TokenRequired);
    assert_eq!(error.known.agent_id.as_deref(), Some("501"));
    assert_eq!(profile(&f)["state"]["agent"]["agentId"], 501);
    assert_eq!(counts(&f), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_create_replaces_only_a_binding_whose_agent_central_deleted() {
    let (f, _, expected) = fixture().await;
    registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    assert_eq!(profile(&f)["state"]["agent"]["agentId"], 201);
    // 웹에서 Agent 삭제. 중앙 ID는 재사용되지 않으므로 다른 팀 Agent로 다음 ID를 202로 둔다
    {
        let mut agents = f.state.agents.lock().unwrap();
        agents.remove(&201);
        agents.insert(900, json!({"agentId":900,"teamId":999}));
    }
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    // 새 연결을 명시하지 않은 재사용·기존 ID 선택은 그대로 거부하고 로컬 연결을 보존
    for selection in [None, Some(Selection::Use(201))] {
        let rejected = registration::ensure(&f.path, &expected, selection)
            .await
            .unwrap_err();
        assert_eq!(rejected.kind, FailureKind::Rejected);
        assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    }
    assert_eq!(counts(&f), (1, 0));

    let replaced = registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    assert!(replaced.created && !replaced.reused);
    assert_eq!(replaced.state, R::TokenRequired);
    assert_eq!(replaced.agent_id, Some(202));
    assert_eq!(profile(&f)["state"]["kind"], "needs_token");
    assert_eq!(profile(&f)["state"]["agent"]["agentId"], 202);
    assert_eq!(counts(&f), (2, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn creation_is_explicit_durable_and_never_issues_a_token_or_sends_local_paths() {
    let (f, _, expected) = fixture().await;
    assert_eq!(
        registration::inspect(&f.path, &expected)
            .await
            .unwrap()
            .state,
        R::Vacant
    );
    assert!(!agent_profile::path(&f.path).exists());
    let created = registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    assert_eq!(created.state, R::TokenRequired);
    assert_eq!(created.agent_id, Some(201));
    assert!(created.created && !created.reused);
    assert_eq!(created.outcome, Mutation::Applied);
    assert_eq!(profile(&f)["state"]["kind"], "needs_token");
    assert_eq!(profile(&f)["state"]["agent"]["agentId"], 201);
    assert_eq!(counts(&f), (1, 0));
    for selection in [None, Some(Selection::Create), Some(Selection::Use(201))] {
        let again = registration::ensure(&f.path, &expected, selection)
            .await
            .unwrap();
        assert!(again.reused && !again.created);
        assert_eq!(again.agent_id, Some(201));
    }
    assert_eq!(counts(&f), (1, 0));
    // The server fixture rejects any create DTO keys beyond the four central metadata fields.
    assert!(
        !serde_json::to_string(&created)
            .unwrap()
            .contains(account_server::AGENT_SECRET)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_creation_response_is_never_replayed_and_explicit_owned_id_recovers_it() {
    let (f, _, expected) = fixture().await;
    f.state.create_mode.store(1, Ordering::SeqCst); // central stores the Agent, then sends 500
    let unknown = registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    assert_eq!(unknown.state, R::CreationUnknown);
    assert_eq!(unknown.outcome, Mutation::Unknown);
    assert_eq!(unknown.agent_id, None);
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    for selection in [None, Some(Selection::Create)] {
        assert_eq!(
            registration::ensure(&f.path, &expected, selection)
                .await
                .unwrap()
                .state,
            R::CreationUnknown
        );
        assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    }
    let output = f.run(
        &[
            "setup",
            "--output",
            "json",
            "--project",
            f._root.path().to_str().unwrap(),
            "--create-agent",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert_no_secrets(&output);
    let error: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(error["error"]["code"], "SETUP_REGISTRATION_UNKNOWN");
    assert_eq!(error["error"]["outcome"], "unknown");
    assert_eq!(error["error"]["requiredInputs"][0]["field"], "agent.id");
    assert_eq!(
        error["error"]["requiredInputs"][0]["source"],
        "owned_agent_list"
    );
    assert_eq!(error["error"]["recovery"]["automatic"], false);
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    let recovered = registration::ensure(&f.path, &expected, Some(Selection::Use(201)))
        .await
        .unwrap();
    assert_eq!(recovered.state, R::TokenRequired);
    assert_eq!(recovered.agent_id, Some(201));
    assert_eq!(counts(&f), (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_creation_leaves_pending_and_rerun_does_not_replay_the_post() {
    let (f, _, expected) = fixture().await;
    f.state.pause_create.store(true, Ordering::SeqCst);
    let mut pending = false;
    tokio::select! {
        result = registration::ensure_tracked(&f.path, &expected, Some(Selection::Create), &mut pending) =>
            panic!("expected creation to still be running: {result:?}"),
        entered = tokio::time::timeout(std::time::Duration::from_secs(30), f.state.create_entered.notified()) =>
            entered.unwrap(),
    }
    assert!(pending);
    f.state.create_resume.notify_one();
    f.state.pause_create.store(false, Ordering::SeqCst);

    let profile: Value =
        serde_json::from_slice(&fs::read(agent_profile::path(&f.path)).unwrap()).unwrap();
    assert_eq!(profile["state"]["kind"], "create_pending");
    let resumed = registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    assert_eq!(resumed.state, R::CreationUnknown);
    let recovered = registration::ensure(&f.path, &expected, Some(Selection::Use(201)))
        .await
        .unwrap();
    assert_eq!(recovered.agent_id, Some(201));
    assert_eq!(counts(&f), (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn csrf_failure_does_not_create_pending_and_definite_rejection_allows_explicit_retry() {
    let (f, _, expected) = fixture().await;
    f.state.csrf_failure.store(true, Ordering::SeqCst);
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap_err()
            .kind,
        FailureKind::Unavailable
    );
    assert!(!agent_profile::path(&f.path).exists());
    assert_eq!(counts(&f), (0, 0));
    f.state.csrf_failure.store(false, Ordering::SeqCst);
    f.state.create_mode.store(2, Ordering::SeqCst);
    let rejected = registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap_err();
    assert_eq!(rejected.kind, FailureKind::Rejected);
    assert_eq!(rejected.outcome, Mutation::NotApplied);
    assert_eq!(profile(&f)["state"]["kind"], "vacant");
    f.state.create_mode.store(0, Ordering::SeqCst);
    registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    assert_eq!(counts(&f), (2, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_agents_invalid_ids_and_local_binding_changes_are_rejected_without_mutation() {
    let (f, _, mut expected) = fixture().await;
    for id in [0, i64::MAX as u64 + 1, 302, 303, 999] {
        assert!(
            registration::ensure(&f.path, &expected, Some(Selection::Use(id)))
                .await
                .is_err()
        );
        assert!(!agent_profile::path(&f.path).exists());
    }
    expected.user_id = 52;
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap_err()
            .kind,
        FailureKind::SessionConflict
    );
    expected.user_id = 51;
    expected.team_id = 72;
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap_err()
            .kind,
        FailureKind::SessionConflict
    );
    assert_eq!(counts(&f), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_agent_reuse_preserves_token_and_checks_live_central_metadata() {
    let (f, _, mut expected) = fixture().await;
    let metadata = &expected.project.metadata;
    agent_profile::create(
        &f.path,
        agent_profile::CreateOptions {
            name: metadata.agent_name.clone(),
            role_description: metadata.role_description.clone(),
            part: metadata.part,
            roles: metadata.roles.clone(),
            tool: agent_profile::Tool::Codex,
            project_name: metadata.project_name.clone(),
            project: expected.project.root.clone(),
        },
    )
    .await
    .unwrap();
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    for selection in [None, Some(Selection::Create), Some(Selection::Use(201))] {
        let reused = registration::ensure(&f.path, &expected, selection)
            .await
            .unwrap();
        assert_eq!(reused.state, R::Ready);
        assert!(reused.reused);
        assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    }
    assert_eq!(counts(&f), (1, 1));
    expected.project.metadata.agent_name.push_str(" changed");
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap_err()
            .kind,
        FailureKind::BindingConflict
    );
    expected.project.metadata.agent_name.pop();
    f.state.agents.lock().unwrap().get_mut(&201).unwrap()["ownerUserId"] = json!(999);
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Use(201)))
            .await
            .unwrap_err()
            .kind,
        FailureKind::Forbidden
    );
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(counts(&f), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_and_competing_account_locks_fail_without_creating_an_agent() {
    let (f, _, expected) = fixture().await;
    let guard = config::lock(&config::state_directory(&f.path).join("account.lock")).unwrap();
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap_err()
            .kind,
        FailureKind::Busy
    );
    drop(guard);
    private_file::write(
        &agent_profile::path(&f.path),
        &json!({"invalid":"private marker"}),
        16_384,
    )
    .unwrap();
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Use(301)))
            .await
            .unwrap_err()
            .kind,
        FailureKind::Storage
    );
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(counts(&f), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_reports_required_input_known_ids_and_token_boundary_without_starting_broker() {
    let (f, stages, expected) = fixture().await;
    registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = stages;
    let result = stages.execute(Step::RegisterAgent, &context).await;
    let StepResult::Failed(error) = result else {
        panic!("registration should require token preparation")
    };
    assert_eq!(error.code, setup::ErrorCode::TokenRequired);
    assert_eq!(error.known.agent_id.as_deref(), Some("201"));
    assert_eq!(error.outcome, Outcome::NotApplied);
    assert!(
        !config::state_directory(&f.path)
            .join("runtime.json")
            .exists()
    );
    assert_eq!(counts(&f), (1, 0));
    let output = f.run(
        &[
            "setup",
            "--output",
            "json",
            "--project",
            f._root.path().to_str().unwrap(),
            "--create-agent",
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(3), "{}", text(&output));
    assert_no_secrets(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["known"]["agentId"], "201");
    assert_eq!(value["stages"][6]["status"], "skipped");
    assert_eq!(counts(&f), (1, 0));
}

struct InteractiveRecovery;
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_ignoring_routing_metadata_leaves_creation_unknown_without_token_or_retry() {
    unconfirmed_metadata_leaves_creation_unknown(5, true).await;
}
// GitHub #127: the default request (OTHER, no tags) equals what a missing field reads as
// locally, so a reply without the fields must leave that creation unknown as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_omitting_routing_metadata_leaves_a_default_creation_unknown() {
    unconfirmed_metadata_leaves_creation_unknown(5, false).await;
}
// The central reports both fields, but with its defaults instead of the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_storing_defaults_leaves_a_requested_creation_unknown() {
    unconfirmed_metadata_leaves_creation_unknown(6, true).await;
}
async fn unconfirmed_metadata_leaves_creation_unknown(create_mode: u8, requested: bool) {
    let (f, _, mut expected) = fixture().await;
    if requested {
        expected.project.metadata.part = devlink_broker::agent_metadata::AgentPart::Backend;
        expected.project.metadata.roles = vec!["API".into()];
    }
    f.state.create_mode.store(create_mode, Ordering::SeqCst);
    for _ in 0..2 {
        let result = registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap();
        assert_eq!(result.state, R::CreationUnknown);
        assert_eq!(result.outcome, Mutation::Unknown);
        assert_eq!(profile(&f)["state"]["kind"], "create_pending");
    }
    assert_eq!(counts(&f), (1, 0));
}

impl AgentInput for InteractiveRecovery {
    fn available(&self) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn request(&mut self, _: &[setup::AgentChoice]) -> Result<Selection, AgentInputError> {
        Err(AgentInputError::Unavailable)
    }
    async fn rename(&mut self) -> Result<String, AgentInputError> {
        Ok("Distinct Agent".into())
    }
    async fn confirm_issue(&mut self, id: u64) -> Result<(), AgentInputError> {
        assert_eq!(id, 201);
        Ok(())
    }
}

struct RoutingInput(std::sync::Arc<std::sync::atomic::AtomicUsize>);
impl AgentInput for RoutingInput {
    fn available(&self) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn request(&mut self, _: &[setup::AgentChoice]) -> Result<Selection, AgentInputError> {
        Ok(Selection::Create)
    }
    async fn routing_metadata(
        &mut self,
        part: Option<devlink_broker::agent_metadata::AgentPart>,
        roles: Option<Vec<String>>,
    ) -> Result<(devlink_broker::agent_metadata::AgentPart, Vec<String>), AgentInputError> {
        assert!(part.is_none() && roles.is_none());
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok((
            devlink_broker::agent_metadata::AgentPart::Backend,
            vec![" API ".into(), "API".into()],
        ))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_creation_collects_routing_metadata_once_and_reuses_it() {
    let (f, stages, _) = fixture().await;
    let prompts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut stages = stages
        .with_agent_input(RoutingInput(prompts.clone()))
        .with_agent(Some(Selection::Create));
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let StepResult::InputRequired(required) = stages.execute(Step::RegisterAgent, &context).await
    else {
        panic!("expected routing metadata input");
    };
    assert_eq!(
        required
            .required_inputs
            .iter()
            .map(|i| i.field)
            .collect::<Vec<_>>(),
        vec!["agent.part", "agent.roles"]
    );
    assert!(stages.can_resolve_input() && stages.input_is_interactive());
    assert_eq!(counts(&f), (0, 0));
    stages.resolve_input(&required).await.unwrap();
    for _ in 0..2 {
        assert!(matches!(
            stages.execute(Step::RegisterAgent, &context).await,
            StepResult::Complete { .. }
        ));
        assert_eq!(profile(&f)["state"]["agent"]["part"], "BACKEND");
        assert_eq!(profile(&f)["state"]["agent"]["roles"], json!(["API"]));
    }
    assert_eq!(prompts.load(Ordering::SeqCst), 1);
    assert_eq!(counts(&f), (1, 1));
    let saved = devlink_broker::setup_project::prepare(
        &f.path,
        &ProjectOptions {
            path: Some(f._root.path().to_path_buf()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        saved.metadata.part,
        devlink_broker::agent_metadata::AgentPart::Backend
    );
    assert_eq!(saved.metadata.roles, vec!["API"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_registration_retries_a_confirmed_name_conflict_with_a_new_name() {
    let (f, stages, expected) = fixture().await;
    seed_pr37_agent(&f, &expected, 302, 88, "CODEX");
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = stages
        .with_agent_input(InteractiveRecovery)
        .with_agent(Some(Selection::Create));
    let StepResult::InputRequired(metadata) = stages.execute(Step::RegisterAgent, &context).await
    else {
        panic!("expected routing metadata input");
    };
    stages.resolve_input(&metadata).await.unwrap();
    let StepResult::InputRequired(required) = stages.execute(Step::RegisterAgent, &context).await
    else {
        panic!("expected a prompt to rename the Agent");
    };
    assert_eq!(required.code, setup::ErrorCode::AgentAlreadyExists);
    assert_eq!(counts(&f), (1, 0));
    stages.resolve_input(&required).await.unwrap();
    assert!(matches!(
        stages.execute(Step::RegisterAgent, &context).await,
        StepResult::Complete { .. }
    ));
    assert_eq!(counts(&f), (2, 1));
    assert_eq!(
        stages.project().unwrap().metadata.agent_name,
        "Distinct Agent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_token_prompt_issues_only_after_confirming_the_existing_agent_id() {
    let (f, stages, expected) = fixture().await;
    registration::ensure(&f.path, &expected, Some(Selection::Create))
        .await
        .unwrap();
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = stages
        .with_agent_input(InteractiveRecovery)
        .with_agent(Some(Selection::Use(201)));
    let StepResult::InputRequired(required) = stages.execute(Step::RegisterAgent, &context).await
    else {
        panic!("expected a token issuance prompt");
    };
    assert_eq!(required.code, setup::ErrorCode::TokenRequired);
    assert_eq!(required.known.agent_id.as_deref(), Some("201"));
    assert_eq!(counts(&f), (1, 0));
    stages.resolve_input(&required).await.unwrap();
    assert!(matches!(
        stages.execute(Step::RegisterAgent, &context).await,
        StepResult::Complete { .. }
    ));
    assert_eq!(counts(&f), (1, 1));
}

struct ExplicitCreate;
impl AgentInput for ExplicitCreate {
    fn available(&self) -> bool {
        true
    }
    async fn request(&mut self, _: &[setup::AgentChoice]) -> Result<Selection, AgentInputError> {
        Ok(Selection::Create)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn injected_agent_selection_resumes_only_registration_and_machine_mode_returns_descriptor() {
    let (f, mut stages, _) = fixture().await;
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let StepResult::InputRequired(required) = stages.execute(Step::RegisterAgent, &context).await
    else {
        panic!()
    };
    assert_eq!(required.required_inputs[0].field, "agent.action");
    assert_eq!(counts(&f), (0, 0));
    let mut stages = stages.with_agent_input(ExplicitCreate);
    stages.resolve_input(&required).await.unwrap();
    assert!(matches!(
        stages.execute(Step::RegisterAgent, &context).await,
        StepResult::Complete { .. }
    ));
    assert_eq!(counts(&f), (1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_missing_action_and_argument_conflicts_are_structured_and_never_mutate() {
    let (f, _, _) = fixture().await;
    let output = f.run(
        &[
            "setup",
            "--output",
            "json",
            "--project",
            f._root.path().to_str().unwrap(),
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert_no_secrets(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["error"]["requiredInputs"][0]["field"],
        "agent.action"
    );
    assert!(!agent_profile::path(&f.path).exists());
    for args in [
        vec!["setup", "--create-agent", "--agent-id", "301"],
        vec!["setup", "--agent-id", "0"],
        vec![
            "setup",
            "--agent-id",
            "301",
            "--agent-name",
            "private-invalid-marker",
        ],
    ] {
        let output = f.run(&args, None);
        assert_eq!(output.status.code(), Some(2));
        assert!(!text(&output).contains("private-invalid-marker"));
    }
    assert_eq!(counts(&f), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_holds_account_and_control_locks_across_network_and_refuses_live_broker_mutation()
 {
    let (f, _, expected) = fixture().await;
    // A running/starting broker is identified by its OS lock, not by a PID.
    let broker = config::lock(&config::state_directory(&f.path).join("broker.lock")).unwrap();
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap_err()
            .kind,
        FailureKind::BrokerBusy
    );
    assert_eq!(counts(&f), (0, 0));
    drop(broker);
    f.state.pause_members.store(true, Ordering::SeqCst);
    f.state.pause_create.store(true, Ordering::SeqCst);
    let path = f.path.clone();
    let other = expected.clone();
    let request =
        tokio::spawn(
            async move { registration::ensure(&path, &other, Some(Selection::Create)).await },
        );
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        f.state.members_entered.notified(),
    )
    .await
    .unwrap();
    let control_path = config::state_directory(&f.path).join("control.lock");
    assert!(config::try_lock(&control_path).unwrap().is_none());
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Create))
            .await
            .unwrap_err()
            .kind,
        FailureKind::Busy
    );
    f.state.members_resume.notify_one();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        f.state.create_entered.notified(),
    )
    .await
    .unwrap();
    assert!(
        config::try_lock(&config::state_directory(&f.path).join("broker.lock"))
            .unwrap()
            .is_none()
    );
    f.state.create_resume.notify_one();
    assert_eq!(request.await.unwrap().unwrap().state, R::TokenRequired);
    assert_eq!(counts(&f), (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_input_cancellation_preserves_local_state_and_does_not_send_creation() {
    struct Cancel;
    impl AgentInput for Cancel {
        fn available(&self) -> bool {
            true
        }
        async fn request(
            &mut self,
            _: &[setup::AgentChoice],
        ) -> Result<Selection, AgentInputError> {
            Err(AgentInputError::Cancelled)
        }
    }
    let (f, stages, _) = fixture().await;
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = stages.with_agent_input(Cancel);
    let StepResult::InputRequired(required) = stages.execute(Step::RegisterAgent, &context).await
    else {
        panic!()
    };
    let error = stages.resolve_input(&required).await.unwrap_err();
    assert_eq!(error.code, setup::ErrorCode::Cancelled);
    assert_eq!(error.known.user_id.as_deref(), Some("51"));
    assert!(!agent_profile::path(&f.path).exists());
    assert_eq!(counts(&f), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_owned_selection_preserves_java_long_ids_as_public_strings() {
    let (f, _, expected) = fixture().await;
    let id = i64::MAX as u64;
    let mut agent = json!({"agentId":id,"teamId":71,"ownerUserId":51,"name":"Remote Agent",
        "roleDescription":null,"toolType":"CODEX","projectName":"Remote project",
        "status":"OFFLINE","lastSeenAt":null,"createdAt":"2026-11-03T00:00:00",
        "part":"OTHER","roles":[]});
    f.state.agents.lock().unwrap().insert(id, agent.clone());
    let selected = registration::ensure(&f.path, &expected, Some(Selection::Use(id)))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&selected).unwrap()["agentId"],
        "9223372036854775807"
    );
    assert_eq!(selected.state, R::TokenRequired);
    assert_eq!(profile(&f)["state"]["agent"]["name"], "Remote Agent");
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    agent["toolType"] = json!("CLAUDE_CODE");
    f.state.agents.lock().unwrap().insert(id, agent);
    assert!(
        registration::ensure(&f.path, &expected, Some(Selection::Use(id)))
            .await
            .is_err()
    );
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    assert_eq!(counts(&f), (0, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_initial_issue_is_requested_once_and_existing_agent_requires_permission() {
    for (flag, expected_id) in [("--create-agent", "201"), ("--agent-id", "301")] {
        let (f, _, _) = fixture().await;
        if flag == "--create-agent" {
            f.state.token_mode.store(1, Ordering::SeqCst);
        }
        let mut args = vec![
            "setup",
            "--output",
            "jsonl",
            "--project",
            f._root.path().to_str().unwrap(),
            flag,
        ];
        if flag == "--agent-id" {
            args.push("301");
        }
        let output = f.run(&args, None);
        assert_eq!(output.status.code(), Some(3), "{}", text(&output));
        assert_no_secrets(&output);
        let events: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.iter().filter(|e| e["type"] == "result").count(), 1);
        let result = events.last().unwrap();
        assert_eq!(result["error"]["code"], "SETUP_TOKEN_REQUIRED");
        assert_eq!(result["error"]["known"]["agentId"], expected_id);
        assert_eq!(
            result["error"]["outcome"],
            if flag == "--create-agent" {
                "unknown"
            } else {
                "applied"
            }
        );
        assert_eq!(result["stages"][6]["status"], "skipped");
        assert_eq!(
            counts(&f),
            (
                usize::from(flag == "--create-agent"),
                usize::from(flag == "--create-agent")
            )
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_foreign_selection_has_the_same_contract_error_for_dto_and_http_denial() {
    let (f, _, expected) = fixture().await;
    for id in [302, 303] {
        // foreign owner in a 200 DTO; foreign team rejected with HTTP 403
        let rejected = registration::ensure(&f.path, &expected, Some(Selection::Use(id)))
            .await
            .unwrap_err();
        assert_eq!(rejected.kind, FailureKind::Forbidden);
        let output = f.run(
            &[
                "setup",
                "--output",
                "json",
                "--project",
                f._root.path().to_str().unwrap(),
                "--agent-id",
                &id.to_string(),
            ],
            None,
        );
        assert_eq!(output.status.code(), Some(4), "{}", text(&output));
        assert_no_secrets(&output);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["error"]["code"], "SETUP_AGENT_FORBIDDEN");
        assert_eq!(result["error"]["recovery"]["action"], "select_owned_agent");
        assert_eq!(result["error"]["requiredInputs"][0]["field"], "agent.id");
        assert_eq!(result["error"]["retryable"], true);
        assert_eq!(result["error"]["outcome"], "not_applied");
        assert!(!agent_profile::path(&f.path).exists());
        assert_eq!(counts(&f), (0, 0));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_explicit_same_id_refreshes_central_metadata_and_preserves_ready_credentials() {
    let (f, stages, expected) = fixture().await;
    let metadata = &expected.project.metadata;
    agent_profile::create(
        &f.path,
        agent_profile::CreateOptions {
            name: metadata.agent_name.clone(),
            role_description: metadata.role_description.clone(),
            part: metadata.part,
            roles: metadata.roles.clone(),
            tool: agent_profile::Tool::Codex,
            project_name: metadata.project_name.clone(),
            project: expected.project.root.clone(),
        },
    )
    .await
    .unwrap();
    let credentials_before = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    let before = profile(&f);
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    {
        let mut agents = f.state.agents.lock().unwrap();
        let central = agents.get_mut(&201).unwrap();
        central["name"] = json!("웹에서 변경한 Agent");
        central["projectName"] = json!("새 프로젝트 표시명");
        central["roleDescription"] = json!("갱신된 역할");
        central["part"] = json!("INFRA");
        central["roles"] = json!(["배포"]);
    }
    // A read/repeated create cannot silently rewrite the saved metadata.
    for selection in [None, Some(Selection::Create)] {
        assert_eq!(
            registration::ensure(&f.path, &expected, selection)
                .await
                .unwrap_err()
                .kind,
            FailureKind::BindingConflict
        );
        assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    }
    let broker_guard = config::lock(&config::state_directory(&f.path).join("broker.lock")).unwrap();
    assert_eq!(
        registration::ensure(&f.path, &expected, Some(Selection::Use(201)))
            .await
            .unwrap_err()
            .kind,
        FailureKind::BrokerBusy
    );
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    drop(broker_guard);
    let mut stages = stages.with_agent(Some(Selection::Use(201)));
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    assert!(matches!(
        stages.execute(Step::RegisterAgent, &context).await,
        StepResult::Complete {
            outcome: Outcome::Applied,
            reused: true
        }
    ));
    let after = profile(&f);
    assert_eq!(after["state"]["kind"], "ready");
    assert_eq!(after["state"]["token"], before["state"]["token"]);
    assert_eq!(after["projectRoot"], before["projectRoot"]);
    assert_eq!(after["state"]["agent"]["name"], "웹에서 변경한 Agent");
    assert_eq!(after["state"]["agent"]["projectName"], "새 프로젝트 표시명");
    assert_eq!(after["state"]["agent"]["part"], "INFRA");
    assert_eq!(after["state"]["agent"]["roles"], json!(["배포"]));
    assert_eq!(
        stages.project().unwrap().metadata.agent_name,
        "웹에서 변경한 Agent"
    );
    assert_eq!(
        stages
            .project()
            .unwrap()
            .metadata
            .role_description
            .as_deref(),
        Some("갱신된 역할")
    );
    assert_eq!(credentials::load(&f.path).unwrap(), credentials_before);
    let stable = fs::read(agent_profile::path(&f.path)).unwrap();
    let mut stages = stages.with_agent(None);
    assert!(matches!(
        stages.execute(Step::RegisterAgent, &context).await,
        StepResult::Complete {
            outcome: Outcome::NotApplied,
            reused: true
        }
    ));
    assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), stable);
    assert_eq!(counts(&f), (1, 1));
}

/// GitHub #127: reuse is confirmed only against metadata central itself reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reuse_is_not_confirmed_by_a_reply_without_part_and_roles() {
    let (f, _, expected) = fixture().await;
    let metadata = &expected.project.metadata;
    agent_profile::create(
        &f.path,
        agent_profile::CreateOptions {
            name: metadata.agent_name.clone(),
            role_description: metadata.role_description.clone(),
            part: metadata.part,
            roles: metadata.roles.clone(),
            tool: agent_profile::Tool::Codex,
            project_name: metadata.project_name.clone(),
            project: expected.project.root.clone(),
        },
    )
    .await
    .unwrap();
    let original = fs::read(agent_profile::path(&f.path)).unwrap();
    // The saved metadata and the request are both the defaults here, so a reply read with
    // defaults would look like a match.
    f.state.update_mode.store(5, Ordering::SeqCst);
    for selection in [None, Some(Selection::Create), Some(Selection::Use(201))] {
        let error = registration::ensure(&f.path, &expected, selection)
            .await
            .unwrap_err();
        assert_eq!(error.kind, FailureKind::Protocol, "{selection:?}");
        assert_eq!(fs::read(agent_profile::path(&f.path)).unwrap(), original);
    }
    assert_eq!(counts(&f), (1, 1));
}
