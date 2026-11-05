#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, text};
use devlink_broker::{
    agent_profile::{
        self,
        registration::{self, Selection},
        tokens,
    },
    credentials, local,
    mcp_config::{self, ApplyStatus, Change},
    sessions::ToolType,
    setup_mcp::{self, Options},
    setup_project::ProjectOptions,
    user_auth,
};
use devlink_cli::setup::{
    ErrorCode, McpInput, McpInputError, NoLoginInput, Outcome, SetupContext, SetupStages, Step,
    StepExecutor, StepResult,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Prepared {
    app: Fixture,
    stages: Option<SetupStages>,
    context: setup_mcp::Context,
    options: Options,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        let _ = self.app.run(&["stop"], None);
    }
}
impl Prepared {
    async fn new(tool: ToolType, version: &str) -> Self {
        Self::seeded(tool, version, None).await
    }
    /// `existing` is written to the project's Host target before validate runs.
    async fn seeded(tool: ToolType, version: &str, existing: Option<&[u8]>) -> Self {
        let app = Fixture::new().await;
        credentials::set_private_permissions(app.path.parent().unwrap()).unwrap();
        credentials::set_private_permissions(&app.path).unwrap();
        assert!(app.login().status.success());
        let root = app._root.path();
        if let Some(existing) = existing {
            let target = mcp_config::project_target(tool, root);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, existing).unwrap();
        }
        let host = account_server::tool_fixture::host(
            &root.join("hosts"),
            if tool == ToolType::Codex {
                "codex"
            } else {
                "claude"
            },
            &if tool == ToolType::Codex {
                format!("codex-cli {version}")
            } else {
                format!("{version} (Claude Code)")
            },
        );
        let (node, entry) =
            account_server::tool_fixture::mcp_runtime(&root.join("런타임 with spaces"));
        let mut stages = SetupStages::new(NoLoginInput)
            .with_mcp(Options {
                node: Some(node.clone()),
                entry: Some(entry.clone()),
                ..Default::default()
            })
            .with_project(ProjectOptions {
                path: Some(root.to_path_buf()),
                ..Default::default()
            })
            .with_tool(Some(tool), Some(host))
            .with_broker_executable(PathBuf::from(env!("CARGO_BIN_EXE_devlink")));
        let execution = SetupContext {
            config_path: app.path.clone(),
        };
        for step in [
            Step::Validate,
            Step::Authenticate,
            Step::SelectTeam,
            Step::BindProject,
            Step::SelectTool,
        ] {
            match stages.execute(step, &execution).await {
                StepResult::Complete { .. } => {}
                StepResult::Failed(error) | StepResult::InputRequired(error) => {
                    panic!("{step:?}: {}", serde_json::to_string(&error).unwrap());
                }
            }
        }
        let expected = registration::RegistrationContext {
            origin: user_auth::origin(&stages.configuration().unwrap().config)
                .unwrap()
                .to_string(),
            user_id: 51,
            team_id: 71,
            project: stages.project().unwrap().clone(),
            tool,
        };
        registration::ensure(&app.path, &expected, Some(Selection::Create))
            .await
            .unwrap();
        assert!(
            tokens::ensure(&app.path, &expected, 201, true)
                .await
                .unwrap()
                .verified
        );
        assert!(matches!(
            stages.execute(Step::RegisterAgent, &execution).await,
            StepResult::Complete { .. }
        ));
        let result = stages.execute(Step::StartBroker, &execution).await;
        if let StepResult::Failed(e) = &result {
            panic!("{}", serde_json::to_string(e).unwrap());
        }
        assert!(matches!(result, StepResult::Complete { .. }));
        let broker = stages.broker().unwrap();
        let context = setup_mcp::Context {
            config_path: app.path.clone(),
            origin: expected.origin,
            user_id: 51,
            team_id: 71,
            agent_id: 201,
            project: expected.project.root,
            tool: stages.tool().unwrap().clone(),
            broker_pid: broker.health.pid,
            broker_instance: broker.health.instance_id.clone(),
        };
        Self {
            app,
            stages: Some(stages),
            context,
            options: Options {
                node: Some(node),
                entry: Some(entry),
                ..Default::default()
            },
        }
    }
    async fn plan(&self) -> setup_mcp::Preparation {
        setup_mcp::prepare(
            self.context.clone(),
            self.options.clone(),
            PathBuf::from(env!("CARGO_BIN_EXE_devlink")),
        )
        .await
        .unwrap()
    }
    fn target(&self) -> PathBuf {
        mcp_config::project_target(self.context.tool.tool_type, &self.context.project)
    }
    fn profile(&self) -> Vec<u8> {
        fs::read(agent_profile::path(&self.app.path)).unwrap()
    }
    fn calls(&self) -> (usize, usize) {
        (
            self.app.state.create_calls.load(Ordering::SeqCst),
            self.app.state.token_calls.load(Ordering::SeqCst),
        )
    }
}
async fn create_and_reuse(tool: ToolType, version: &str) {
    let f = Prepared::new(tool, version).await;
    let original = f.profile();
    let descriptor = credentials::load(&f.app.path).unwrap();
    let prepared = f.plan().await;
    assert_eq!(prepared.plan().unwrap().change, Change::Create);
    let plan_text = serde_json::to_string(prepared.plan().unwrap()).unwrap();
    assert!(!plan_text.contains(&descriptor.local_token));
    assert!(!plan_text.contains(account_server::AGENT_SECRET));
    assert!(!f.target().exists());
    let applied = prepared.apply().await.unwrap();
    assert_eq!(applied.config.status, ApplyStatus::Applied);
    assert!(applied.config.restart_required && applied.runtime_verified);
    assert_eq!(applied.host_session, "not_checked");
    let bytes = fs::read(f.target()).unwrap();
    let contents = String::from_utf8(bytes.clone()).unwrap();
    assert!(contents.contains("DEVLINK_BROKER_FILE"));
    assert!(!contents.contains(&descriptor.local_token));
    assert!(!contents.contains(account_server::AGENT_SECRET));
    let modified = fs::metadata(f.target()).unwrap().modified().unwrap();
    let prepared = f.plan().await;
    assert_eq!(prepared.plan().unwrap().change, Change::Unchanged);
    let reused = prepared.apply().await.unwrap();
    assert_eq!(reused.config.status, ApplyStatus::Unchanged);
    assert!(reused.config.backup_path.is_none());
    assert!(!reused.config.restart_required);
    assert_eq!(fs::read(f.target()).unwrap(), bytes);
    assert_eq!(
        fs::metadata(f.target()).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(f.profile(), original);
    assert_eq!(credentials::load(&f.app.path).unwrap(), descriptor);
    assert_eq!(f.calls(), (1, 1));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_project_configuration_is_applied_and_reused_without_token_values() {
    create_and_reuse(ToolType::Codex, "0.166.0").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claude_project_configuration_is_applied_and_reused_without_token_values() {
    create_and_reuse(ToolType::ClaudeCode, "2.1.310").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn merge_preserves_other_mcp_settings_and_returns_a_private_exact_backup() {
    let f = Prepared::new(ToolType::ClaudeCode, "2.1.310").await;
    let original = br#"{"huge":9223372036854775807,"mcpServers":{"other":{"command":"other","env":{"OTHER_SECRET":"unrelated-private-marker"}}}}"#;
    fs::write(f.target(), original).unwrap();
    let prepared = f.plan().await;
    assert_eq!(prepared.plan().unwrap().change, Change::Merge);
    assert!(
        !serde_json::to_string(prepared.plan().unwrap())
            .unwrap()
            .contains("unrelated-private-marker")
    );
    let result = prepared.apply().await.unwrap();
    let backup = result.config.backup_path.unwrap();
    credentials::verify_private_permissions(&backup).unwrap();
    assert_eq!(fs::read(backup).unwrap(), original);
    let bytes = fs::read_to_string(f.target()).unwrap();
    assert!(bytes.contains("9223372036854775807"));
    let value: Value = serde_json::from_str(&bytes).unwrap();
    assert_eq!(value["mcpServers"]["other"]["command"], "other");
    assert!(value["mcpServers"]["devlink"].is_object());
    assert_eq!(f.calls(), (1, 1));
}

struct Confirm {
    approve: bool,
    edit: Option<PathBuf>,
    calls: Arc<AtomicUsize>,
}
impl McpInput for Confirm {
    fn available(&self) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn request(&mut self, plan: &mcp_config::ConfigPlan) -> Result<bool, McpInputError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(plan.diff.is_some());
        if let Some(target) = &self.edit {
            fs::write(target, "{\"outside\":true}").unwrap();
        }
        Ok(self.approve)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_keeps_the_same_plan_and_refuses_concurrent_config_edits() {
    let mut f = Prepared::new(ToolType::ClaudeCode, "2.1.310").await;
    let snapshot = f.profile();
    let calls = Arc::new(AtomicUsize::new(0));
    let stages = f.stages.take().unwrap();
    let mut stages = stages.with_mcp(f.options.clone()).with_mcp_input(Confirm {
        approve: true,
        edit: Some(f.target()),
        calls: calls.clone(),
    });
    let context = SetupContext {
        config_path: f.app.path.clone(),
    };
    let StepResult::InputRequired(required) = stages.execute(Step::ConfigureMcp, &context).await
    else {
        panic!("expected review")
    };
    assert_eq!(required.required_inputs[0].field, "mcp.apply");
    assert!(required.mcp_plan.is_some());
    assert!(!f.target().exists());
    stages.resolve_input(&required).await.unwrap();
    let StepResult::Failed(error) = stages.execute(Step::ConfigureMcp, &context).await else {
        panic!("expected conflict")
    };
    assert_eq!(error.code, ErrorCode::McpConfigConflict);
    assert_eq!(error.known.agent_id.as_deref(), Some("201"));
    assert_eq!(
        fs::read_to_string(f.target()).unwrap(),
        "{\"outside\":true}"
    );
    assert_eq!(f.profile(), snapshot);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.calls(), (1, 1));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declined_review_never_changes_the_host_file_or_prepared_agent() {
    let mut f = Prepared::new(ToolType::Codex, "0.166.0").await;
    let snapshot = f.profile();
    let mut stages = f
        .stages
        .take()
        .unwrap()
        .with_mcp(f.options.clone())
        .with_mcp_input(Confirm {
            approve: false,
            edit: None,
            calls: Arc::default(),
        });
    let context = SetupContext {
        config_path: f.app.path.clone(),
    };
    let StepResult::InputRequired(required) = stages.execute(Step::ConfigureMcp, &context).await
    else {
        panic!()
    };
    let error = stages.resolve_input(&required).await.unwrap_err();
    assert_eq!(error.code, ErrorCode::Cancelled);
    assert_eq!(error.code.exit_code(), 130);
    assert_eq!(error.outcome, Outcome::NotApplied);
    assert!(error.mcp_plan.is_some());
    assert!(!f.target().exists());
    assert_eq!(f.profile(), snapshot);
    assert!(local::running(&f.app.path).unwrap());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_host_returns_only_manual_guidance_without_writing_or_claiming_readiness() {
    let mut f = Prepared::new(ToolType::Codex, "9.9.9").await;
    let descriptor = credentials::load(&f.app.path).unwrap();
    let mut stages = f.stages.take().unwrap().with_mcp(f.options.clone());
    let StepResult::Failed(error) = stages
        .execute(
            Step::ConfigureMcp,
            &SetupContext {
                config_path: f.app.path.clone(),
            },
        )
        .await
    else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::ManualActionRequired);
    let manual = error.manual_config.unwrap();
    assert!(manual.snippet.contains("[mcp_servers]"));
    assert!(manual.snippet.contains("devlink ="));
    assert_eq!(manual.suggested_target_path, f.target());
    assert!(!manual.snippet.contains(&descriptor.local_token));
    assert!(!manual.snippet.contains(account_server::AGENT_SECRET));
    assert!(!f.target().exists());
    assert_eq!(f.calls(), (1, 1));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_runtime_is_rejected_after_review_and_cannot_reuse_a_prior_probe() {
    let f = Prepared::new(ToolType::Codex, "0.166.0").await;
    let plan = f.plan().await;
    fs::write(f.options.entry.as_ref().unwrap(), "// edited during review").unwrap();
    assert_eq!(
        plan.apply().await.unwrap_err().code,
        "SETUP_PROFILE_CONFLICT"
    );
    assert!(!f.target().exists());
    assert_eq!(f.calls(), (1, 1));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_node_wrong_service_and_raw_runtime_errors_never_become_applied_configuration() {
    let f = Prepared::new(ToolType::ClaudeCode, "2.1.310").await;
    for mode in ["old-node", "wrong-server", "invalid", "timeout"] {
        fs::write(
            f.options.node.as_ref().unwrap().with_extension("mcp-mode"),
            mode,
        )
        .unwrap();
        let result = setup_mcp::prepare(
            f.context.clone(),
            f.options.clone(),
            PathBuf::from(env!("CARGO_BIN_EXE_devlink")),
        )
        .await;
        let Err(error) = result else {
            panic!("invalid runtime accepted")
        };
        assert_eq!(error.code, "SETUP_TOOL_UNAVAILABLE");
        assert!(
            !serde_json::to_string(&error)
                .unwrap()
                .contains("private-runtime-error-marker")
        );
        assert!(!f.target().exists());
    }
    assert_eq!(f.calls(), (1, 1));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_requires_review_applies_explicitly_and_then_reuses_configuration_before_verify_step() {
    let f = Prepared::new(ToolType::ClaudeCode, "2.1.310").await;
    let base = [
        "setup",
        "--output",
        "json",
        "--project",
        f.app._root.path().to_str().unwrap(),
        "--tool",
        "claude-code",
        "--tool-path",
        f.context.tool.executable.command.to_str().unwrap(),
        "--node-path",
        f.options.node.as_ref().unwrap().to_str().unwrap(),
        "--mcp-entry",
        f.options.entry.as_ref().unwrap().to_str().unwrap(),
    ];
    let original = f.profile();
    let output = f.app.run(&base, None);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert_no_secrets(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["step"], "configure_mcp");
    assert!(value["error"]["mcpPlan"]["diff"].is_object());
    assert!(!f.target().exists());
    let mut apply = base.to_vec();
    apply.push("--apply-mcp");
    let output = f.app.run(&apply, None);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert_no_secrets(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["stages"][8]["status"], "succeeded");
    assert_eq!(value["data"]["mcpConfig"]["status"], "applied");
    assert_eq!(value["data"]["connection"]["setupReady"], true);
    assert_eq!(value["data"]["connection"]["hostSession"], "unbound");
    assert_eq!(value["data"]["connection"]["collaborationReady"], false);
    let modified = fs::metadata(f.target()).unwrap().modified().unwrap();
    let output = f.app.run(&base, None);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["data"]["mcpConfig"]["status"], "unchanged");
    assert_eq!(
        fs::metadata(f.target()).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(f.profile(), original);
    assert_eq!(f.calls(), (1, 1));

    let status = [
        "setup",
        "status",
        "--output",
        "jsonl",
        "--node-path",
        f.options.node.as_ref().unwrap().to_str().unwrap(),
        "--mcp-entry",
        f.options.entry.as_ref().unwrap().to_str().unwrap(),
    ];
    let output = f.app.run(&status, None);
    assert!(output.status.success(), "{}", text(&output));
    assert_no_secrets(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["sequence"], 1);
    assert_eq!(result["stages"], json!([]));
    assert_eq!(result["data"]["agentId"], "201");
    assert_eq!(result["data"]["connection"]["setupReady"], true);
    assert_eq!(
        fs::metadata(f.target()).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(f.profile(), original);

    fs::write(
        f.target(),
        br#"{"mcpServers":{"devlink":{"command":"changed"}}}"#,
    )
    .unwrap();
    let output = f.app.run(&status, None);
    assert!(output.status.success(), "{}", text(&output));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["data"]["connection"]["mcpConfig"], "conflict");
    assert_eq!(result["data"]["connection"]["setupReady"], false);

    f.app.state.wrong_context.store(true, Ordering::SeqCst);
    let output = f.app.run(&status, None);
    assert!(output.status.success(), "{}", text(&output));
    assert_no_secrets(&output);
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["data"]["connection"]["identity"], "mismatch");
    assert_eq!(result["data"]["connection"]["setupReady"], false);
    assert!(result["data"].get("agentId").is_none());
    assert_eq!(f.calls(), (1, 1));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_mcp_options_are_rejected_before_configuration_or_central_calls() {
    let app = Fixture::new().await;
    for args in [
        vec!["setup", "--mcp-mode", "manual", "--apply-mcp"],
        vec!["setup", "--mcp-entry", "relative-secret-entry.js"],
        vec![
            "setup",
            "--mcp-mode",
            "manual",
            "--mcp-target",
            "C:\\unused\\config.toml",
        ],
    ] {
        let output = app.run(&args, None);
        assert_eq!(output.status.code(), Some(2), "{}", text(&output));
        assert!(app.state.calls.lock().unwrap().is_empty());
        assert!(!agent_profile::path(&app.path).exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_rejects_existing_host_conflict_before_agent_or_token_creation() {
    let app = Fixture::new().await;
    credentials::set_private_permissions(app._root.path()).unwrap();
    assert!(app.login().status.success());
    app.state.calls.lock().unwrap().clear();
    let root = app._root.path();
    let host =
        account_server::tool_fixture::host(&root.join("host"), "claude", "2.1.310 (Claude Code)");
    let (node, entry) = account_server::tool_fixture::mcp_runtime(&root.join("runtime"));
    let target = root.join(".mcp.json");
    let original =
        br#"{"mcpServers":{"devlink":{"command":"other-runtime"},"other":{"command":"keep-me"}}}"#;
    fs::write(&target, original).unwrap();
    let output = app.run(
        &[
            "setup",
            "--output",
            "json",
            "--project",
            root.to_str().unwrap(),
            "--tool",
            "claude-code",
            "--tool-path",
            host.to_str().unwrap(),
            "--node-path",
            node.to_str().unwrap(),
            "--mcp-entry",
            entry.to_str().unwrap(),
            "--create-agent",
            "--apply-mcp",
        ],
        None,
    );
    // Stop an owned child even when the regression fails against the old build.
    let _ = app.run(&["stop"], None);
    assert_eq!(
        app.state.create_calls.load(Ordering::SeqCst),
        0,
        "{}",
        text(&output)
    );
    assert_eq!(app.state.token_calls.load(Ordering::SeqCst), 0);
    assert!(app.state.calls.lock().unwrap().is_empty());
    assert_eq!(output.status.code(), Some(6), "{}", text(&output));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["step"], "validate");
    assert_eq!(value["error"]["code"], "SETUP_MCP_CONFIG_CONFLICT");
    assert_eq!(value["error"]["recovery"]["action"], "review_mcp_diff");
    let conflict = &value["error"]["mcpError"]["conflict"];
    assert_eq!(
        conflict["targetPath"],
        serde_json::json!(target.canonicalize().unwrap())
    );
    let proposed = &conflict["proposed"];
    assert_eq!(proposed["type"], "stdio");
    assert_eq!(
        PathBuf::from(proposed["command"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        node.canonicalize().unwrap()
    );
    assert_eq!(
        PathBuf::from(proposed["args"][0].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        entry.canonicalize().unwrap()
    );
    let env = proposed["env"].as_object().unwrap();
    assert_eq!(env.len(), 1);
    assert!(env["DEVLINK_BROKER_FILE"].is_string());
    assert!(!text(&output).contains("other-runtime"));
    assert!(!text(&output).contains("keep-me"));
    assert_eq!(fs::read(target).unwrap(), original);
    assert!(!agent_profile::path(&app.path).exists());
    assert!(!credentials::path(&app.path).exists());
    assert_no_secrets(&output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_rejects_missing_node_before_agent_or_token_creation() {
    let app = Fixture::new().await;
    credentials::set_private_permissions(app._root.path()).unwrap();
    assert!(app.login().status.success());
    app.state.calls.lock().unwrap().clear();
    let root = app._root.path();
    let host =
        account_server::tool_fixture::host(&root.join("host"), "claude", "2.1.310 (Claude Code)");
    let (_, entry) = account_server::tool_fixture::mcp_runtime(&root.join("runtime"));
    let node = root.join(if cfg!(windows) {
        "missing/node.exe"
    } else {
        "missing/node"
    });
    let output = app.run(
        &[
            "setup",
            "--output",
            "json",
            "--project",
            root.to_str().unwrap(),
            "--tool",
            "claude-code",
            "--tool-path",
            host.to_str().unwrap(),
            "--node-path",
            node.to_str().unwrap(),
            "--mcp-entry",
            entry.to_str().unwrap(),
            "--create-agent",
            "--apply-mcp",
        ],
        None,
    );
    let _ = app.run(&["stop"], None);
    assert_eq!(
        app.state.create_calls.load(Ordering::SeqCst),
        0,
        "{}",
        text(&output)
    );
    assert_eq!(app.state.token_calls.load(Ordering::SeqCst), 0);
    assert!(app.state.calls.lock().unwrap().is_empty());
    assert_eq!(output.status.code(), Some(5), "{}", text(&output));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["step"], "validate");
    assert_eq!(value["error"]["code"], "SETUP_TOOL_UNAVAILABLE");
    assert!(!agent_profile::path(&app.path).exists());
    assert!(!credentials::path(&app.path).exists());
    assert!(!root.join(".mcp.json").exists());
    assert_no_secrets(&output);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_checks_a_later_tool_selection_and_rechecks_changes_before_registration() {
    let app = Fixture::new().await;
    credentials::set_private_permissions(app._root.path()).unwrap();
    assert!(app.login().status.success());
    let root = app._root.path();
    let hosts = root.join("hosts");
    account_server::tool_fixture::host(&hosts, "claude", "2.1.310 (Claude Code)");
    let options = account_server::tool_fixture::mcp_options(&root.join("runtime"));
    let mut stages = SetupStages::default()
        .with_project(ProjectOptions {
            path: Some(root.to_path_buf()),
            ..Default::default()
        })
        .with_mcp(options.clone())
        .with_tool_search(devlink_broker::tool_discovery::ToolSearch {
            directories: vec![hosts],
            timeout: std::time::Duration::from_secs(15),
        })
        .with_agent(Some(Selection::Create));
    let context = SetupContext {
        config_path: app.path.clone(),
    };
    for step in [
        Step::Validate,
        Step::Authenticate,
        Step::SelectTeam,
        Step::BindProject,
        Step::SelectTool,
    ] {
        match stages.execute(step, &context).await {
            StepResult::Complete { .. } => {}
            StepResult::Failed(error) | StepResult::InputRequired(error) => {
                panic!("{step:?}: {}", serde_json::to_string(&error).unwrap());
            }
        }
    }
    let target = root.join(".mcp.json");
    fs::write(
        &target,
        br#"{"mcpServers":{"devlink":{"command":"changed"}}}"#,
    )
    .unwrap();
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::McpConfigConflict);
    assert_eq!(error.outcome, Outcome::NotApplied);
    assert_eq!(error.known.user_id.as_deref(), Some("51"));
    assert_eq!(error.known.team_id.as_deref(), Some("71"));
    let conflict = error.mcp_error.unwrap().conflict.unwrap();
    assert_eq!(conflict.target_path, target.canonicalize().unwrap());
    assert_eq!(conflict.proposed["type"], "stdio");
    fs::remove_file(&target).unwrap();
    fs::remove_file(options.entry.unwrap()).unwrap();
    let StepResult::Failed(error) = stages.execute(Step::RegisterAgent, &context).await else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::ToolUnavailable);
    assert_eq!(app.state.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.state.token_calls.load(Ordering::SeqCst), 0);
    assert!(!agent_profile::path(&app.path).exists());
    assert!(!credentials::path(&app.path).exists());
    assert!(!local::Runtime::path(&app.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_host_reaches_manual_guidance_despite_a_conflicting_existing_entry() {
    let original =
        br#"{"mcpServers":{"devlink":{"command":"other-runtime"},"other":{"command":"keep-me"}}}"#;
    // The seeded fixture asserts validate, select_tool and register_agent complete:
    // a merge conflict blocks only a version the provider would apply automatically.
    let mut f = Prepared::seeded(ToolType::ClaudeCode, "9.9.9", Some(original)).await;
    let mut stages = f.stages.take().unwrap().with_mcp(f.options.clone());
    let StepResult::Failed(error) = stages
        .execute(
            Step::ConfigureMcp,
            &SetupContext {
                config_path: f.app.path.clone(),
            },
        )
        .await
    else {
        panic!()
    };
    assert_eq!(error.code, ErrorCode::ManualActionRequired);
    let manual = error.manual_config.unwrap();
    assert_eq!(manual.suggested_target_path, f.target());
    assert!(manual.snippet.contains("mcpServers"));
    assert!(!manual.snippet.contains("keep-me"));
    assert_eq!(fs::read(f.target()).unwrap(), original);
    assert_eq!(f.calls(), (1, 1));
}

/// A standard npm layout keeps node.exe beside the shim; it need not be on PATH.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_uses_the_node_beside_an_npm_shim_without_node_on_path() {
    let app = Fixture::new().await;
    credentials::set_private_permissions(app._root.path()).unwrap();
    assert!(app.login().status.success());
    let root = app._root.path();
    let npm = root.join("npm");
    let (node, entry) = account_server::tool_fixture::mcp_runtime(&npm);
    let shim = npm.join("claude.cmd");
    fs::write(&shim, "this batch text must never execute").unwrap();
    let script = npm.join("node_modules/@anthropic-ai/claude-code/cli.js");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(&script, "synthetic entry").unwrap();
    fs::write(script.with_extension("probe"), "2.1.310 (Claude Code)").unwrap();
    // PATH keeps PowerShell for the broker's ACL setup but offers no Node at all.
    let path = std::env::join_paths(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|p| !p.join("node.exe").is_file()),
    )
    .unwrap();
    let unrelated = root.join("unrelated-runtime");
    let (path_node, _) = account_server::tool_fixture::mcp_runtime(&unrelated);
    fs::write(path_node.with_extension("mcp-mode"), "invalid").unwrap();
    let competing_path =
        std::env::join_paths(std::iter::once(unrelated).chain(std::env::split_paths(&path)))
            .unwrap();
    let run = |first_setup: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devlink"));
        command
            .arg("--config")
            .arg(&app.path)
            .args(["setup", "--output", "json", "--project"])
            .arg(root)
            .args(["--tool", "claude-code", "--mcp-entry"])
            .arg(&entry)
            .env("PATH", if first_setup { &path } else { &competing_path })
            .env_remove("DEVLINK_TOKEN")
            .env_remove("DEVLINK_BROKER_FILE")
            .env_remove("NODE_OPTIONS")
            .current_dir(root)
            .stdin(Stdio::null());
        if first_setup {
            command
                .arg("--tool-path")
                .arg(&shim)
                .args(["--create-agent", "--apply-mcp"]);
        }
        command.output().unwrap()
    };
    let first_setup = run(true);
    let reuse = run(false);
    let requests_before = app.state.calls.lock().unwrap().len();
    let foreign = root.join("foreign-project");
    fs::create_dir(&foreign).unwrap();
    let input = root.join("outside-project-batch.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "schemaVersion": 1, "user": {"mode": "session"},
            "team": {"id": "71"}, "project": {"path": root},
            "tool": {"type": "claude-code"}, "agent": {"action": "use", "id": "201"},
            "mcp": {"mode": "apply", "targetPath": foreign.join(".mcp.json")}
        }))
        .unwrap(),
    )
    .unwrap();
    let outside = Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(&app.path)
        .args(["setup", "--non-interactive", "--input"])
        .arg(&input)
        .args(["--output", "json", "--mcp-entry"])
        .arg(&entry)
        .env("PATH", &path)
        .env_remove("DEVLINK_TOKEN")
        .env_remove("DEVLINK_BROKER_FILE")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let requests_after = app.state.calls.lock().unwrap().len();
    let _ = app.run(&["stop"], None);
    for (output, status) in [(&first_setup, "applied"), (&reuse, "unchanged")] {
        assert_eq!(output.status.code(), Some(0), "{}", text(output));
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            value["data"]["connection"]["setupReady"],
            true,
            "{}",
            text(output)
        );
        assert_eq!(value["data"]["mcpConfig"]["status"], status);
        assert_no_secrets(output);
    }
    let written = fs::read_to_string(root.join(".mcp.json")).unwrap();
    assert!(written.contains(node.file_name().unwrap().to_str().unwrap()));
    assert!(!shim.with_extension("calls").exists());
    assert!(!path_node.with_extension("mcp-calls").exists());
    assert_eq!(outside.status.code(), Some(2), "{}", text(&outside));
    let result: Value = serde_json::from_slice(&outside.stdout).unwrap();
    assert_eq!(result["error"]["code"], "SETUP_INPUT_INVALID");
    assert_eq!(result["error"]["step"], "validate");
    assert_eq!(requests_after, requests_before);
    assert_eq!(app.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(app.state.token_calls.load(Ordering::SeqCst), 1);
}
