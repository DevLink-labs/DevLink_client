#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, text};
use devlink_broker::{
    agent_profile, config, credentials, private_file,
    setup_project::{self, FailureKind, ProjectOptions},
    user_auth,
};
use devlink_cli::setup::{
    ErrorCode, NoLoginInput, Outcome, SetupContext, SetupStages, Step, StepExecutor, StepResult,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    sync::atomic::Ordering,
};

async fn fixture() -> Fixture {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    assert!(f.login().status.success());
    f.state.calls.lock().unwrap().clear();
    f
}

fn directory(f: &Fixture, name: &str) -> std::path::PathBuf {
    let path = f._root.path().join(name);
    fs::create_dir(&path).unwrap();
    path
}

fn run(f: &Fixture, cwd: &Path, args: &[&str], exit: i32) -> Value {
    let runtime = account_server::tool_fixture::default_mcp_options();
    let output = Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(&f.path)
        .args(["setup", "--output", "json"])
        .args(args)
        .args(["--tool", "codex", "--tool-path"])
        .arg(account_server::test_tool())
        .arg("--node-path")
        .arg(runtime.node.unwrap())
        .arg("--mcp-entry")
        .arg(runtime.entry.unwrap())
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(exit), "{}", text(&output));
    assert_no_secrets(&output);
    assert!(output.stderr.is_empty(), "{}", text(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

fn profile(f: &Fixture, root: &Path, kind: &str) {
    let agent = json!({"agentId":201,"teamId":71,"ownerUserId":51,"name":"Saved Agent",
        "roleDescription":"Saved role","toolType":"CODEX","projectName":"Saved project",
        "status":"OFFLINE","lastSeenAt":null,"createdAt":"2026-11-03T00:00:00"});
    let state = match kind {
        "ready" => json!({"kind":kind,"agent":agent,"token":account_server::AGENT_SECRET}),
        "needs_token" => json!({"kind":kind,"agent":agent}),
        "create_pending" => {
            json!({"kind":kind,"name":"Saved Agent","tool":"CODEX","project_name":"Saved project"})
        }
        _ => json!({"kind":"vacant"}),
    };
    private_file::write(&agent_profile::path(&f.path), &json!({
        "schemaVersion":1,"origin":user_auth::origin(&config::Config::load(&f.path).unwrap()).unwrap().as_str(),
        "userId":51,"teamId":71,"projectRoot":root.canonicalize().unwrap(),"state":state,
    }), 16_384).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_defaults_to_cwd_and_accepts_explicit_relative_korean_paths_with_spaces() {
    let f = fixture().await;
    let cwd = directory(&f, "현재 작업");
    let target = directory(&f, "한글 project");
    for args in [
        vec![],
        vec!["--project", "../한글 project/../한글 project/."],
    ] {
        let value = run(&f, &cwd, &args, 2);
        assert_eq!(value["stages"][3]["status"], "succeeded");
        assert_eq!(value["stages"][3]["outcome"], "not_applied");
        assert_eq!(value["error"]["step"], "register_agent");
    }
    let value = run(&f, &cwd, &["--project-path", target.to_str().unwrap()], 2);
    assert_eq!(value["stages"][3]["status"], "succeeded");
    assert!(!agent_profile::path(&f.path).exists());
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stage_retains_canonical_root_and_separate_registration_metadata() {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    let project = directory(&f, "프로젝트 with spaces");
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = SetupStages::default().with_project(ProjectOptions {
        path: Some(project.join(".")),
        ..Default::default()
    });
    assert!(matches!(
        stages.execute(Step::Validate, &context).await,
        StepResult::Complete { .. }
    ));
    assert!(stages.project().is_none());
    assert!(matches!(
        stages.execute(Step::BindProject, &context).await,
        StepResult::Complete {
            outcome: Outcome::NotApplied,
            reused: false
        }
    ));
    let prepared = stages.project().unwrap();
    assert_eq!(prepared.root, project.canonicalize().unwrap());
    assert!(prepared.root.is_absolute());
    assert_eq!(
        serde_json::to_value(&prepared.metadata).unwrap(),
        json!({
            "projectName":"프로젝트 with spaces", "name":"프로젝트 with spaces", "roleDescription":null,
        })
    );
    assert!(f.state.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_and_file_paths_fail_before_config_creation_or_any_central_request() {
    let f = Fixture::new().await;
    let file = f._root.path().join("file.txt");
    fs::write(&file, "preserved").unwrap();
    let before = fs::read(&f.path).unwrap();
    for invalid in [f._root.path().join("missing"), file.clone()] {
        let value = run(
            &f,
            f._root.path(),
            &["--project", invalid.to_str().unwrap()],
            2,
        );
        assert_eq!(value["error"]["code"], "SETUP_PROJECT_INVALID");
        assert_eq!(value["error"]["step"], "validate");
        assert_eq!(value["error"]["location"]["field"], "project.path");
        assert_eq!(fs::read(&f.path).unwrap(), before);
    }
    let new_config = f._root.path().join("new profile/config.json");
    let out = Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(&new_config)
        .args([
            "setup",
            "--project",
            "no-such-directory",
            "--output",
            "json",
        ])
        .current_dir(f._root.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["error"]["code"], "SETUP_PROJECT_INVALID");
    assert!(!new_config.parent().unwrap().exists());
    assert!(f.state.calls.lock().unwrap().is_empty());
    assert_eq!(fs::read_to_string(file).unwrap(), "preserved");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_project_is_rejected_in_every_profile_state_without_overwriting_credentials() {
    let f = fixture().await;
    let first = directory(&f, "original 프로젝트");
    let second = directory(&f, "different project");
    let mcp = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    for kind in ["ready", "needs_token", "create_pending", "vacant"] {
        profile(&f, &first, kind);
        let path = agent_profile::path(&f.path);
        let before = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let value = run(&f, &second, &[], 6);
        assert_eq!(value["error"]["code"], "SETUP_PROJECT_BINDING_CONFLICT");
        assert_eq!(value["error"]["step"], "bind_project");
        assert_eq!(value["error"]["known"]["teamId"], "71");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
        assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    }
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_project_reuses_saved_names_and_keeps_exact_profile_bytes_and_time() {
    let f = fixture().await;
    let project = directory(&f, "local folder");
    for kind in ["ready", "needs_token", "create_pending", "vacant"] {
        profile(&f, &project, kind);
        let path = agent_profile::path(&f.path);
        let before = fs::read(&path).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let options = ProjectOptions {
            path: Some(project.join("../local folder/.")),
            ..Default::default()
        };
        let prepared = setup_project::prepare(&f.path, &options).unwrap();
        assert!(prepared.reused);
        assert_eq!(
            prepared.metadata.agent_name,
            if kind == "vacant" {
                "local folder"
            } else {
                "Saved Agent"
            }
        );
        // Synthetic local profiles exercise project preparation only; live registration
        // and exact profile preservation are covered in setup_registration.
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_metadata_overrides_defaults_and_invalid_utf16_lengths_are_rejected() {
    let f = fixture().await;
    let project = directory(&f, "folder");
    let options = ProjectOptions {
        path: Some(project.clone()),
        project_name: Some("표시 프로젝트".into()),
        agent_name: Some("개발 Agent".into()),
        role_description: Some("구현과 검증".into()),
    };
    let prepared = setup_project::prepare(&f.path, &options).unwrap();
    assert_eq!(
        serde_json::to_value(prepared.metadata).unwrap(),
        json!({"name":"개발 Agent","projectName":"표시 프로젝트","roleDescription":"구현과 검증"})
    );
    let value = run(
        &f,
        &project,
        &[
            "--project-name",
            "표시 프로젝트",
            "--agent-name",
            "개발 Agent",
            "--role-description",
            "구현과 검증",
        ],
        2,
    );
    assert_eq!(value["stages"][3]["status"], "succeeded");
    for (flag, invalid, field) in [
        ("--agent-name", " ".into(), "agent.name"),
        ("--agent-name", "😀".repeat(51), "agent.name"),
        ("--project-name", "😀".repeat(61), "project.name"),
        ("--project-name", "\t".into(), "project.name"),
        (
            "--role-description",
            "😀".repeat(128),
            "agent.roleDescription",
        ),
    ] {
        let value = run(&f, &project, &[flag, &invalid], 2);
        assert_eq!(value["error"]["code"], "SETUP_INPUT_INVALID");
        // Names are checked in bind_project, after sign-in and team selection,
        // because a saved profile may still provide them.
        assert_eq!(value["error"]["step"], "bind_project");
        assert_eq!(value["error"]["requiredInputs"][0]["field"], field);
    }
}

#[tokio::test]
async fn a_filesystem_root_passes_validate_and_bind_project_uses_saved_names() {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    // A root such as C:\ or / has no folder name to derive default names from.
    let root = f._root.path().ancestors().last().unwrap().to_path_buf();
    assert!(root.file_name().is_none());
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = SetupStages::default().with_project(ProjectOptions {
        path: Some(root.clone()),
        ..Default::default()
    });
    assert!(matches!(
        stages.execute(Step::Validate, &context).await,
        StepResult::Complete { .. }
    ));
    let StepResult::Failed(error) = stages.execute(Step::BindProject, &context).await else {
        panic!("empty default names were accepted")
    };
    assert_eq!(error.code, ErrorCode::InputInvalid);
    assert_eq!(error.step, Step::BindProject);
    assert_eq!(error.required_inputs[0].field, "project.name");
    assert!(stages.project().is_none());
    profile(&f, &root, "ready");
    assert!(matches!(
        stages.execute(Step::BindProject, &context).await,
        StepResult::Complete { reused: true, .. }
    ));
    let prepared = stages.project().unwrap();
    assert_eq!(prepared.metadata.agent_name, "Saved Agent");
    assert_eq!(prepared.metadata.project_name, "Saved project");
}

#[tokio::test]
async fn long_folder_defaults_fit_central_limits_and_do_not_include_parent_paths() {
    let f = Fixture::new().await;
    // Keep the emoji at the UTF-16 boundary within the filesystem's byte limit.
    let folder = format!("가{}😀{}", "a".repeat(98), "나".repeat(25));
    let project = directory(&f, &folder);
    let prepared = setup_project::resolve(&ProjectOptions {
        path: Some(project),
        ..Default::default()
    })
    .unwrap();
    // Leave one unit unused rather than split the two-unit emoji at the boundary.
    assert_eq!(prepared.metadata.agent_name.encode_utf16().count(), 99);
    assert_eq!(prepared.metadata.project_name.encode_utf16().count(), 120);
    let body = serde_json::to_value(prepared.metadata).unwrap();
    assert_eq!(body.as_object().unwrap().len(), 3);
    assert!(body.get("projectRoot").is_none());
    assert!(body.get("path").is_none());
    let boundary = setup_project::resolve(&ProjectOptions {
        path: Some(f._root.path().to_path_buf()),
        agent_name: Some("😀".repeat(50)),
        project_name: Some("😀".repeat(60)),
        role_description: Some("가".repeat(255)),
    })
    .unwrap();
    assert_eq!(boundary.metadata.agent_name.encode_utf16().count(), 100);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_profile_is_preserved_and_a_missing_saved_directory_is_a_conflict() {
    let f = fixture().await;
    let project = directory(&f, "project");
    let path = agent_profile::path(&f.path);
    private_file::write(&path, &json!({"schemaVersion":99}), 16_384).unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(
        setup_project::prepare(
            &f.path,
            &ProjectOptions {
                path: Some(project.clone()),
                ..Default::default()
            }
        )
        .unwrap_err()
        .kind,
        FailureKind::Storage
    );
    let value = run(&f, &project, &[], 5);
    // Team selection already rejects invalid Agent snapshots before bind_project.
    assert_eq!(value["error"]["code"], "SETUP_TEAM_STORAGE_FAILED");
    assert_eq!(fs::read(&path).unwrap(), before);
    let gone = directory(&f, "gone");
    profile(&f, &gone, "ready");
    fs::remove_dir(&gone).unwrap();
    let before = fs::read(&path).unwrap();
    let value = run(&f, &project, &[], 6);
    assert_eq!(value["error"]["code"], "SETUP_PROJECT_BINDING_CONFLICT");
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn binding_checks_control_lock_and_revalidates_directory_after_preflight() {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    let project = directory(&f, "temporary project");
    let options = ProjectOptions {
        path: Some(project.clone()),
        ..Default::default()
    };
    let lock = config::lock(&config::state_directory(&f.path).join("control.lock")).unwrap();
    assert_eq!(
        setup_project::prepare(&f.path, &options).unwrap_err().kind,
        FailureKind::Busy
    );
    drop(lock);
    let context = SetupContext {
        config_path: f.path.clone(),
    };
    let mut stages = SetupStages::new(NoLoginInput).with_project(options);
    assert!(matches!(
        stages.execute(Step::Validate, &context).await,
        StepResult::Complete { .. }
    ));
    fs::remove_dir(project).unwrap();
    assert!(matches!(
        stages.execute(Step::BindProject, &context).await,
        StepResult::Failed(_)
    ));
    assert!(stages.project().is_none());
    assert!(!agent_profile::path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_metadata_registers_without_sending_local_path_fields() {
    let f = fixture().await;
    assert!(
        f.run(&["team", "select", "--id", "71"], None)
            .status
            .success()
    );
    let project = directory(&f, "public project");
    let prepared = setup_project::prepare(
        &f.path,
        &ProjectOptions {
            path: Some(project.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    // The synthetic central handler asserts the exact request allowlist, including no path.
    let result = agent_profile::create(
        &f.path,
        agent_profile::CreateOptions {
            name: prepared.metadata.agent_name,
            project_name: prepared.metadata.project_name,
            role_description: prepared.metadata.role_description,
            tool: agent_profile::Tool::Codex,
            project: prepared.root,
        },
    )
    .await
    .unwrap();
    assert_eq!(result["agent"]["name"], "public project");
    assert_eq!(result["agent"]["projectName"], "public project");
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    let local: Value = private_file::read(&agent_profile::path(&f.path), 16_384)
        .unwrap()
        .unwrap();
    assert_eq!(
        local["projectRoot"],
        project.canonicalize().unwrap().to_str().unwrap()
    );
}
