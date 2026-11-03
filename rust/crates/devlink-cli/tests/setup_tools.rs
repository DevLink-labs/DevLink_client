#[path = "support/account_server.rs"]
mod account_server;
use account_server::tool_fixture;
use account_server::{Fixture, text};
use devlink_broker::{
    agent_profile, config, credentials, private_file,
    sessions::ToolType,
    setup_tools,
    tool_discovery::{DetectionStatus, ToolSearch},
};
use devlink_cli::setup::{
    self, Completion, NoLoginInput, SetupContext, SetupStages, Step, StepExecutor, StepResult,
    ToolInput, ToolInputError,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::Duration,
};

fn search(directory: &Path) -> ToolSearch {
    ToolSearch {
        directories: vec![directory.to_path_buf()],
        timeout: ToolSearch::default().timeout,
    }
}
async fn fixture() -> Fixture {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    assert!(f.login().status.success());
    f.state.calls.lock().unwrap().clear();
    f
}
async fn prepare(f: &Fixture, directory: &Path) -> SetupStages {
    let mut stages = SetupStages::new(NoLoginInput)
        .with_project(devlink_broker::setup_project::ProjectOptions {
            path: Some(f._root.path().to_path_buf()),
            ..Default::default()
        })
        .with_tool_search(search(directory));
    for step in [
        Step::Validate,
        Step::Authenticate,
        Step::SelectTeam,
        Step::BindProject,
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
    stages
}

#[tokio::test]
async fn discovery_reports_missing_available_failure_and_invalid_output_without_ai_arguments() {
    let root = tempfile::tempdir().unwrap();
    let reports = search(root.path()).detect(None, None).await;
    assert!(reports.iter().all(|r| r.status == DetectionStatus::Missing));
    let exe = tool_fixture::host(root.path(), "codex", "codex-cli 0.166.0");
    let claude = tool_fixture::host(root.path(), "claude", "2.1.310 (Claude Code)");
    let reports = search(root.path()).detect(None, None).await;
    assert!(
        reports
            .iter()
            .all(|r| r.status == DetectionStatus::Available)
    );
    assert!(
        reports
            .iter()
            .all(|r| r.selected.as_ref().unwrap().verified_host().is_ok())
    );
    assert_eq!(
        fs::read_to_string(exe.with_extension("calls")).unwrap(),
        "--version\n"
    );
    assert_eq!(
        fs::read_to_string(claude.with_extension("calls")).unwrap(),
        "--version\n"
    );
    for (mode, status) in [
        ("failure", DetectionStatus::VersionCheckFailed),
        ("invalid", DetectionStatus::InvalidVersion),
        ("overflow", DetectionStatus::VersionCheckFailed),
    ] {
        fs::write(exe.with_extension("probe"), mode).unwrap();
        let reports = search(root.path()).detect(None, None).await;
        assert_eq!(reports[0].status, status);
        assert!(
            !serde_json::to_string(&reports)
                .unwrap()
                .contains("private-version-error-marker")
        );
    }
}

#[tokio::test]
async fn probe_deadline_terminates_a_stalled_host() {
    let root = tempfile::tempdir().unwrap();
    let exe = tool_fixture::host(root.path(), "codex", "timeout");
    let mut deadline = search(root.path());
    // Only this deliberately stalled process uses a short deadline.
    deadline.timeout = Duration::from_millis(800);
    let reports = deadline.detect(None, Some((ToolType::Codex, &exe))).await;
    assert_eq!(reports[0].status, DetectionStatus::TimedOut);
}

#[cfg(windows)]
#[tokio::test]
#[ignore = "Requires real Node on PATH; executes synthetic version-only ESM entries, no AI tools"]
async fn real_node_loads_npm_esm_entries_for_both_hosts_and_reuses_saved_paths() {
    let node = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .filter(|p| p.is_absolute())
        .map(|p| p.join("node.exe"))
        .find(|p| p.is_file())
        .expect("Node must be installed for this opt-in test");
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("npm 한글 & spaces # %");
    fs::create_dir(&directory).unwrap();
    for (tool, name, package, entry, version) in [
        (
            ToolType::Codex,
            "codex",
            "@openai/codex",
            "bin/codex.js",
            "codex-cli 0.166.0",
        ),
        (
            ToolType::ClaudeCode,
            "claude",
            "@anthropic-ai/claude-code",
            "cli.js",
            "2.1.310 (Claude Code)",
        ),
    ] {
        let package = directory.join("node_modules").join(package);
        let script = package.join(entry);
        fs::create_dir_all(script.parent().unwrap()).unwrap();
        fs::write(package.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(&script, format!(
            "import assert from 'node:assert/strict'; assert.deepEqual(process.argv.slice(2), ['--version']); console.log({});",
            serde_json::to_string(version).unwrap()
        )).unwrap();
        let shim = directory.join(format!("{name}.cmd"));
        fs::write(&shim, "this batch text must never execute").unwrap();
        let mut paths = search(&directory);
        paths.directories.push(node.parent().unwrap().to_path_buf());
        let reports = paths.detect(None, None).await;
        let report = reports.iter().find(|r| r.tool_type == tool).unwrap();
        assert_eq!(report.status, DetectionStatus::Available, "{reports:?}");
        let selected = report.selected.as_ref().unwrap();
        assert_eq!(
            selected.version,
            if tool == ToolType::Codex {
                "0.166.0"
            } else {
                "2.1.310"
            }
        );
        assert_eq!(
            Path::new(&selected.executable.args[0])
                .canonicalize()
                .unwrap(),
            script.canonicalize().unwrap()
        );
        assert!(!selected.executable.args[0].starts_with(r"\\?\"));
        let explicit = paths.detect(None, Some((tool, &shim))).await;
        assert_eq!(
            explicit
                .iter()
                .find(|r| r.tool_type == tool)
                .unwrap()
                .status,
            DetectionStatus::Available
        );
        // A previous selection may contain canonical (verbatim) argv from an older build.
        let mut legacy = selected.clone();
        legacy.executable.args[0] = script.canonicalize().unwrap().to_str().unwrap().to_owned();
        let cached = search(root.path()).detect(Some(&legacy), None).await;
        assert_eq!(
            cached
                .iter()
                .find(|r| r.tool_type == tool)
                .unwrap()
                .selected
                .as_ref(),
            Some(selected)
        );
    }
}

#[tokio::test]
async fn explicit_korean_path_is_reusable_outside_path_and_unknown_versions_are_provider_decisions()
{
    let root = tempfile::tempdir().unwrap();
    let exe = tool_fixture::host(
        &root.path().join("한글 with spaces"),
        "codex",
        "codex-cli 9.9.9",
    );
    let reports = search(root.path())
        .detect(None, Some((ToolType::Codex, &exe)))
        .await;
    let selected = reports[0].selected.as_ref().unwrap();
    assert_eq!(selected.executable.command, exe.canonicalize().unwrap());
    assert!(!reports[0].mcp_automatic_supported);
    assert_eq!(
        selected.verified_host().err().unwrap().code,
        "SETUP_TOOL_UNAVAILABLE"
    );
    let again = search(root.path()).detect(Some(selected), None).await;
    assert_eq!(again[0].selected.as_ref().unwrap(), selected);
}

#[tokio::test]
async fn directories_and_nonexecutable_candidates_are_not_available_and_path_precedence_is_kept() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    fs::create_dir(&first).unwrap();
    fs::create_dir(first.join(if cfg!(windows) { "codex.exe" } else { "codex" })).unwrap();
    let later = root.path().join("later");
    let exe = tool_fixture::host(&later, "codex", "codex-cli 0.166.0");
    let mut paths = search(&first);
    paths.directories.push(later);
    let reports = paths.detect(None, None).await;
    assert_eq!(reports[0].status, DetectionStatus::NotExecutable);
    assert!(!exe.with_extension("calls").exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            search(exe.parent().unwrap()).detect(None, None).await[0].status,
            DetectionStatus::NotExecutable
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_tool_is_saved_privately_then_revalidated_and_reused_without_rewriting() {
    let f = fixture().await;
    let directory = f._root.path().join("tools");
    let exe = tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = prepare(&f, &directory).await;
    assert!(matches!(
        stages.execute(Step::SelectTool, &context).await,
        StepResult::Complete { reused: false, .. }
    ));
    let path = setup_tools::path(&f.path);
    credentials::verify_private_permissions(&path).unwrap();
    let before = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(
        stages.tool().unwrap().executable.command,
        exe.canonicalize().unwrap()
    );
    let mut again = prepare(&f, &directory).await;
    assert!(matches!(
        again.execute(Step::SelectTool, &context).await,
        StepResult::Complete { reused: true, .. }
    ));
    assert_eq!(before, fs::read(&path).unwrap());
    assert_eq!(modified, fs::metadata(&path).unwrap().modified().unwrap());
    assert_eq!(
        fs::read_to_string(exe.with_extension("calls")).unwrap(),
        "--version\n--version\n"
    );
    assert!(!agent_profile::path(&f.path).exists());
    assert!(!f._root.path().join(".mcp.json").exists());
    assert!(!f._root.path().join(".codex").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_tools_require_selection_and_saved_valid_choice_wins_when_both_exist() {
    let f = fixture().await;
    let directory = f._root.path().join("tools");
    tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
    tool_fixture::host(&directory, "claude", "2.1.310 (Claude Code)");
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = prepare(&f, &directory).await;
    let StepResult::InputRequired(error) = stages.execute(Step::SelectTool, &context).await else {
        panic!("Expected tool choice")
    };
    assert_eq!(error.required_inputs[0].field, "tool.type");
    assert_eq!(error.tools.len(), 2);
    assert!(!setup_tools::path(&f.path).exists());
    let mut chosen = stages.with_tool(Some(ToolType::ClaudeCode), None);
    assert!(matches!(
        chosen.execute(Step::SelectTool, &context).await,
        StepResult::Complete { .. }
    ));
    let mut again = prepare(&f, &directory).await;
    assert!(matches!(
        again.execute(Step::SelectTool, &context).await,
        StepResult::Complete { reused: true, .. }
    ));
    assert_eq!(again.tool().unwrap().tool_type, ToolType::ClaudeCode);
}

struct ChooseClaude;
impl ToolInput for ChooseClaude {
    fn available(&self) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn request(
        &mut self,
        _: &[devlink_broker::tool_discovery::ToolDetection],
    ) -> Result<ToolType, ToolInputError> {
        Ok(ToolType::ClaudeCode)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_selection_emits_detected_choices_and_resumes_only_the_tool_stage() {
    let f = fixture().await;
    let directory = f._root.path().join("tools");
    tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
    tool_fixture::host(&directory, "claude", "2.1.310 (Claude Code)");
    let mut events = Vec::new();
    let stages = SetupStages::default()
        .with_project(devlink_broker::setup_project::ProjectOptions {
            path: Some(f._root.path().to_path_buf()),
            ..Default::default()
        })
        .with_tool_search(search(&directory))
        .with_tool_input(ChooseClaude);
    let result = setup::run(
        &SetupContext {
            config_path: f.path.clone(),
        },
        stages,
        |event| {
            events.push(serde_json::to_value(event).unwrap());
            Ok(())
        },
    )
    .await;
    assert_eq!(result.exit_code, 2);
    let waiting = events
        .iter()
        .find(|e| e["stepStatus"] == "waiting_input")
        .unwrap();
    assert_eq!(waiting["step"], "select_tool");
    assert_eq!(waiting["tools"].as_array().unwrap().len(), 2);
    assert_eq!(waiting["requiredInputs"][0]["source"], "detected_tool_list");
    assert_eq!(
        events
            .iter()
            .filter(|e| e["step"] == "authenticate" && e["stepStatus"] == "running")
            .count(),
        1
    );
    let Completion::Failure { error } = result.completion else {
        panic!("Registration requires an explicit action")
    };
    assert_eq!(error.step, Step::RegisterAgent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_requested_tool_is_probed_alone_and_other_hosts_are_never_launched() {
    let f = fixture().await;
    let directory = f._root.path().join("tools");
    let codex = tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
    let claude = tool_fixture::host(&directory, "claude", "2.1.310 (Claude Code)");
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = prepare(&f, &directory)
        .await
        .with_tool(Some(ToolType::Codex), None);
    assert!(matches!(
        stages.execute(Step::SelectTool, &context).await,
        StepResult::Complete { .. }
    ));
    assert_eq!(stages.tool().unwrap().tool_type, ToolType::Codex);
    assert_eq!(
        fs::read_to_string(codex.with_extension("calls")).unwrap(),
        "--version\n"
    );
    assert!(!claude.with_extension("calls").exists());
    // The saved choice is probed first on a rerun; the other host is still untouched.
    let mut again = prepare(&f, &directory).await;
    assert!(matches!(
        again.execute(Step::SelectTool, &context).await,
        StepResult::Complete { reused: true, .. }
    ));
    assert_eq!(
        fs::read_to_string(codex.with_extension("calls")).unwrap(),
        "--version\n--version\n"
    );
    assert!(!claude.with_extension("calls").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_interactive_choice_reprobes_only_the_chosen_tool() {
    let f = fixture().await;
    let directory = f._root.path().join("tools");
    let codex = tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
    let claude = tool_fixture::host(&directory, "claude", "2.1.310 (Claude Code)");
    let stages = SetupStages::default()
        .with_project(devlink_broker::setup_project::ProjectOptions {
            path: Some(f._root.path().to_path_buf()),
            ..Default::default()
        })
        .with_tool_search(search(&directory))
        .with_tool_input(ChooseClaude);
    let result = setup::run(
        &SetupContext {
            config_path: f.path.clone(),
        },
        stages,
        |_| Ok(()),
    )
    .await;
    // Registration now follows tool selection and requires an explicit action.
    assert_eq!(result.exit_code, 2, "{:?}", result.status);
    // Both hosts are probed once to build the choice list; after the choice only
    // Claude Code is probed again, so a host removed during input is still caught.
    assert_eq!(
        fs::read_to_string(codex.with_extension("calls")).unwrap(),
        "--version\n"
    );
    assert_eq!(
        fs::read_to_string(claude.with_extension("calls")).unwrap(),
        "--version\n--version\n"
    );
    assert!(setup_tools::path(&f.path).exists());
    let Completion::Failure { error } = result.completion else {
        panic!("Registration requires an explicit action")
    };
    assert_eq!(error.step, Step::RegisterAgent);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_tools_returns_installation_links_and_next_action_without_saving() {
    let f = fixture().await;
    let directory = f._root.path().join("empty");
    fs::create_dir(&directory).unwrap();
    let mut stages = prepare(&f, &directory).await;
    let StepResult::Failed(error) = stages
        .execute(
            Step::SelectTool,
            &SetupContext {
                config_path: f.path.clone(),
            },
        )
        .await
    else {
        panic!("Expected unavailable")
    };
    assert_eq!(
        serde_json::to_value(&error).unwrap()["code"],
        "SETUP_TOOL_UNAVAILABLE"
    );
    assert_eq!(error.code.exit_code(), 5);
    assert_eq!(error.recovery_action(), "install_or_select_tool");
    assert!(
        error
            .tools
            .iter()
            .all(|r| r.status == DetectionStatus::Missing && r.install_url.starts_with("https://"))
    );
    assert!(error.message().contains("--tool-path"));
    assert!(!setup_tools::path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_explicit_path_works_and_invalid_tool_arguments_are_redacted() {
    let f = fixture().await;
    let exe = tool_fixture::host(
        &f._root.path().join("한글 tools"),
        "codex",
        "codex-cli 0.166.0",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(&f.path)
        .args(["setup", "--tool", "codex", "--tool-path"])
        .arg(exe)
        .args(["--output", "json"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    account_server::assert_no_secrets(&output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["stages"][4]["status"], "succeeded");
    assert_eq!(value["error"]["step"], "register_agent");
    for args in [
        vec!["setup", "--tool", "private-marker"],
        vec!["setup", "--tool-path", "private-marker"],
    ] {
        let output = f.run(&args, None);
        assert_eq!(output.status.code(), Some(2));
        assert!(!text(&output).contains("private-marker"));
    }
}

#[cfg(windows)]
#[tokio::test]
async fn npm_shims_resolve_to_node_and_package_entry_without_executing_batch_text() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("npm 한글 & spaces");
    let node = tool_fixture::host(&directory, "node", "unused");
    let shim = directory.join("codex.cmd");
    fs::write(&shim, "this batch file must never execute").unwrap();
    let script = directory.join("node_modules/@openai/codex/bin/codex.js");
    fs::create_dir_all(script.parent().unwrap()).unwrap();
    fs::write(&script, "synthetic entry").unwrap();
    fs::write(script.with_extension("probe"), "codex-cli 0.166.0").unwrap();
    let reports = search(&directory).detect(None, None).await;
    let selected = reports[0].selected.as_ref().unwrap();
    assert_eq!(selected.executable.command, node.canonicalize().unwrap());
    assert_eq!(
        Path::new(&selected.executable.args[0])
            .canonicalize()
            .unwrap(),
        script.canonicalize().unwrap()
    );
    assert!(!selected.executable.args[0].starts_with(r"\\?\"));
    let calls = fs::read_to_string(node.with_extension("calls")).unwrap();
    assert_eq!(
        calls,
        format!("{}|--version\n", selected.executable.args[0])
    );
}

#[cfg(windows)]
#[tokio::test]
async fn nonstandard_global_shims_remain_unsupported_without_running_batch_text() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("global-bin");
    let node = tool_fixture::host(&directory, "node", "unused");
    let shim = directory.join("codex.cmd");
    fs::write(&shim, "this batch text must never execute").unwrap();
    // A global manager can keep the package outside the shim's sibling node_modules.
    let package = root
        .path()
        .join("global-store/node_modules/@openai/codex/bin");
    fs::create_dir_all(&package).unwrap();
    fs::write(package.join("codex.js"), "synthetic entry").unwrap();
    for explicit in [None, Some((ToolType::Codex, shim.as_path()))] {
        let reports = search(&directory).detect(None, explicit).await;
        assert_eq!(reports[0].status, DetectionStatus::NotExecutable);
    }
    assert!(!node.with_extension("calls").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saved_executable_is_reused_outside_path_and_a_stale_unbound_choice_can_be_replaced() {
    let f = fixture().await;
    let directory = f._root.path().join("empty");
    fs::create_dir(&directory).unwrap();
    let exe = tool_fixture::host(
        &f._root.path().join("outside path"),
        "codex",
        "codex-cli 0.166.0",
    );
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut first = prepare(&f, &directory)
        .await
        .with_tool(Some(ToolType::Codex), Some(exe.clone()));
    assert!(matches!(
        first.execute(Step::SelectTool, &context).await,
        StepResult::Complete { .. }
    ));
    tool_fixture::host(&directory, "claude", "2.1.310 (Claude Code)");
    let mut again = prepare(&f, &directory).await;
    assert!(matches!(
        again.execute(Step::SelectTool, &context).await,
        StepResult::Complete { reused: true, .. }
    ));
    assert_eq!(again.tool().unwrap().tool_type, ToolType::Codex);
    fs::remove_file(&exe).unwrap();
    let mut stale = prepare(&f, &directory).await;
    assert!(matches!(
        stale.execute(Step::SelectTool, &context).await,
        StepResult::Complete { reused: false, .. }
    ));
    assert_eq!(stale.tool().unwrap().tool_type, ToolType::ClaudeCode);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_agent_tool_is_preferred_and_cannot_be_silently_switched_or_rotated() {
    let f = fixture().await;
    let directory = f._root.path().join("tools");
    let exe = tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
    tool_fixture::host(&directory, "claude", "2.1.310 (Claude Code)");
    private_file::write(&agent_profile::path(&f.path), &json!({
        "schemaVersion":1, "origin":devlink_broker::user_auth::origin(&config::Config::load(&f.path).unwrap()).unwrap().as_str(),
        "userId":51,"teamId":71,"projectRoot":f._root.path().canonicalize().unwrap(),
        "state":{"kind":"create_pending","name":"Existing","tool":"CODEX","project_name":"Existing project"}
    }), 16_384).unwrap();
    let before = fs::read(agent_profile::path(&f.path)).unwrap();
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = prepare(&f, &directory).await;
    assert!(matches!(
        stages.execute(Step::SelectTool, &context).await,
        StepResult::Complete { reused: true, .. }
    ));
    assert_eq!(stages.tool().unwrap().tool_type, ToolType::Codex);
    let mut switched = prepare(&f, &directory)
        .await
        .with_tool(Some(ToolType::ClaudeCode), None);
    let StepResult::Failed(error) = switched.execute(Step::SelectTool, &context).await else {
        panic!("Expected binding conflict")
    };
    assert_eq!(error.code, setup::ErrorCode::ToolBindingConflict);
    fs::remove_file(exe).unwrap();
    let mut missing = prepare(&f, &directory).await;
    let StepResult::Failed(error) = missing.execute(Step::SelectTool, &context).await else {
        panic!("Expected unavailable bound tool")
    };
    assert_eq!(error.code, setup::ErrorCode::ToolUnavailable);
    assert_eq!(before, fs::read(agent_profile::path(&f.path)).unwrap());
    assert_eq!(
        f.state
            .token_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_selection_and_locks_stop_before_writing_or_replaying_persisted_arguments() {
    let f = fixture().await;
    let directory = f._root.path().join("tools");
    let exe = tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = prepare(&f, &directory).await;
    for (file, code) in [
        ("control.lock", setup::ErrorCode::ConfigBusy),
        ("account.lock", setup::ErrorCode::AuthBusy),
    ] {
        let _guard = config::lock(&config::state_directory(&f.path).join(file)).unwrap();
        let StepResult::Failed(error) = stages.execute(Step::SelectTool, &context).await else {
            panic!("Expected contention")
        };
        assert_eq!(error.code, code);
        assert!(!setup_tools::path(&f.path).exists());
    }
    assert!(matches!(
        stages.execute(Step::SelectTool, &context).await,
        StepResult::Complete { .. }
    ));
    let path = setup_tools::path(&f.path);
    let mut stored: Value = private_file::read(&path, 16_384).unwrap().unwrap();
    stored["selected"]["executable"]["args"] = json!(["exec", "private-marker"]);
    private_file::write(&path, &stored, 16_384).unwrap();
    let before = fs::read(&path).unwrap();
    let calls = fs::read(exe.with_extension("calls")).unwrap();
    let mut again = prepare(&f, &directory).await;
    let StepResult::Failed(error) = again.execute(Step::SelectTool, &context).await else {
        panic!("Expected storage error")
    };
    assert_eq!(error.code, setup::ErrorCode::ToolStorageFailed);
    assert_eq!(before, fs::read(path).unwrap());
    assert_eq!(calls, fs::read(exe.with_extension("calls")).unwrap());
}

struct ChangedInput {
    path: std::path::PathBuf,
    mode: u8,
}
impl ToolInput for ChangedInput {
    fn available(&self) -> bool {
        true
    }
    async fn request(
        &mut self,
        tools: &[devlink_broker::tool_discovery::ToolDetection],
    ) -> Result<ToolType, ToolInputError> {
        match self.mode {
            0 => return Err(ToolInputError::Cancelled),
            1 => return Err(ToolInputError::ReadFailed),
            2 => {
                fs::remove_file(&tools[1].selected.as_ref().unwrap().executable.command).unwrap();
            }
            3 => {
                config::set(&self.path, "serverUrl", "wss://changed.example.test").unwrap();
            }
            4 => {
                let session = devlink_broker::user_auth::session_path(&self.path);
                let mut stored: Value = private_file::read(&session, 16_384).unwrap().unwrap();
                stored["state"]["user"]["userId"] = json!(999);
                private_file::write(&session, &stored, 16_384).unwrap();
            }
            _ => {
                private_file::write(&setup_tools::path(&self.path), &json!({
                    "schemaVersion":1,"binding":{
                        "origin":devlink_broker::user_auth::origin(&config::Config::load(&self.path).unwrap()).unwrap().as_str(),
                        "userId":51,"teamId":71,"projectRoot":self.path.parent().unwrap().canonicalize().unwrap()
                    }, "selected":tools[0].selected
                }), 16_384).unwrap();
            }
        }
        Ok(ToolType::ClaudeCode)
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_io_failure_removed_tool_and_changed_profile_are_not_retried_or_overwritten() {
    for (mode, code) in [
        (0, setup::ErrorCode::Cancelled),
        (1, setup::ErrorCode::InternalError),
        (2, setup::ErrorCode::ToolUnavailable),
        (3, setup::ErrorCode::AuthSessionConflict),
        (4, setup::ErrorCode::AuthSessionConflict),
        (5, setup::ErrorCode::ToolSelectionConflict),
    ] {
        let f = fixture().await;
        let directory = f._root.path().join("tools");
        tool_fixture::host(&directory, "codex", "codex-cli 0.166.0");
        tool_fixture::host(&directory, "claude", "2.1.310 (Claude Code)");
        let mut stages = prepare(&f, &directory).await.with_tool_input(ChangedInput {
            path: f.path.clone(),
            mode,
        });
        let context = SetupContext {
            config_path: f.path.clone(),
        };
        let StepResult::InputRequired(required) = stages.execute(Step::SelectTool, &context).await
        else {
            panic!("Expected selection")
        };
        let result = stages.resolve_input(&required).await;
        let error = match result {
            Err(error) => error,
            Ok(()) => {
                let StepResult::Failed(error) = stages.execute(Step::SelectTool, &context).await
                else {
                    panic!("Expected revalidation failure")
                };
                error
            }
        };
        assert_eq!(error.code, code);
        assert_eq!(error.known.user_id.as_deref(), Some("51"));
        assert!(!setup_tools::path(&f.path).exists() || mode == 5);
        if mode == 5 {
            let stored: Value = private_file::read(&setup_tools::path(&f.path), 16_384)
                .unwrap()
                .unwrap();
            assert_eq!(stored["selected"]["toolType"], "codex");
        }
    }
}
