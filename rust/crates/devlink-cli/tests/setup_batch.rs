#[path = "support/account_server.rs"]
mod account_server;

use account_server::{Fixture, PASSWORD, assert_no_secrets, text, tool_fixture};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::Ordering,
};

fn batch(config: &Path, input: &Path, format: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_devlink"));
    cmd.arg("--config")
        .arg(config)
        .args(["setup", "--non-interactive", "--input"])
        .arg(input)
        .args(["--output", format])
        .env_remove("DEVLINK_TOKEN")
        .env_remove("DEVLINK_BROKER_FILE")
        .stdin(Stdio::null());
    cmd
}

#[test]
fn missing_batch_fields_are_reported_together_without_initializing_the_profile() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("profile/config.json");
    let input = root.path().join("input.json");
    fs::write(&input, r#"{"schemaVersion":1,"user":{"mode":"login"},"agent":{"action":"create"},"mcp":{"mode":"apply"}}"#).unwrap();
    let output = batch(&config, &input, "jsonl").output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(output.stderr.is_empty());
    let lines: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["stepStatus"], "running");
    assert_eq!(lines[1]["stepStatus"], "failed");
    assert_eq!(lines[2]["error"]["code"], "SETUP_INPUT_REQUIRED");
    assert_eq!(lines[2]["error"]["step"], "validate");
    let fields: Vec<_> = lines[2]["error"]["requiredInputs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|field| field["field"].as_str().unwrap())
        .collect();
    assert_eq!(
        fields,
        [
            "user.email",
            "password",
            "team.id",
            "project.path",
            "project.name",
            "tool.type",
            "agent.name",
            "mcp.targetPath"
        ]
    );
    assert!(!config.parent().unwrap().exists());
}

#[test]
fn malformed_or_ambiguous_batch_input_is_redacted_and_never_writes_a_profile() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("profile/config.json");
    let input = root.path().join("input.json");
    let cases = [
        r#"{"schemaVersion":1,"schemaVersion":1}"#,
        r#"{"schemaVersion":1,"user":{"mode":"session","mode":"session"}}"#,
        r#"{"schemaVersion":1,"user":{"password":"secret-marker"}}"#,
        r#"{"schemaVersion":1,"user":{"mode":"session","email":null}}"#,
        r#"{"schemaVersion":1,"user":[]}"#,
        r#"[]"#,
        r#"{"schemaVersion":1,"team":{"id":"01"}}"#,
        r#"{"schemaVersion":1,"team":{"id":1}}"#,
        r#"{"schemaVersion":1,"team":{"id":"9223372036854775808"}}"#,
        r#"{"schemaVersion":2}"#,
        r#"{"schemaVersion":1,"timeoutMs":29999}"#,
        r#"{"schemaVersion":1,"serverUrl":"ws://central.example.test"}"#,
        r#"{"schemaVersion":1,"agent":{"action":"create","issueToken":false}}"#,
        r#"{"schemaVersion":1,"mcp":{"mode":"manual","targetPath":"relative"}}"#,
    ];
    for contents in cases {
        fs::write(&input, contents).unwrap();
        let output = batch(&config, &input, "json").output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", text(&output));
        assert_eq!(
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
            "SETUP_INPUT_INVALID",
            "{contents}"
        );
        assert!(!text(&output).contains("secret-marker"));
    }
    fs::write(&input, vec![b' '; 65_537]).unwrap();
    let output = batch(&config, &input, "json").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["error"]["code"],
        "SETUP_INPUT_INVALID"
    );
    fs::write(&input, "{}").unwrap();
    let output = batch(&config, &input, "json")
        .args(["--email", "secret-marker"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!text(&output).contains("secret-marker"));
    let output = batch(&config, Path::new("-"), "json").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(!config.parent().unwrap().exists());
}

#[test]
fn a_single_utf8_signature_from_windows_powershell_is_accepted() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("profile/config.json");
    let input = root.path().join("input.json");
    let body = br#"{"schemaVersion":1,"user":{"mode":"session"}}"#;
    for (bom, code) in [
        (&b"\xEF\xBB\xBF"[..], "SETUP_INPUT_REQUIRED"),
        (&b"\xEF\xBB\xBF\xEF\xBB\xBF"[..], "SETUP_INPUT_INVALID"),
    ] {
        fs::write(&input, [bom, &body[..]].concat()).unwrap();
        let output = batch(&config, &input, "json").output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", text(&output));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["error"]["code"], code);
    }
    assert!(!config.parent().unwrap().exists());
}

#[test]
fn a_busy_profile_is_reported_before_reading_the_batch_password() {
    use std::time::{Duration, Instant};
    let root = tempfile::tempdir().unwrap();
    devlink_broker::credentials::set_private_permissions(root.path()).unwrap();
    let config = root.path().join("profile/config.json");
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    let bin = root.path().join("test-bin");
    tool_fixture::host(&bin, "codex", "codex-cli 0.166.0");
    let (node, entry) = tool_fixture::mcp_runtime(&bin);
    let search_path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let input = root.path().join("input.json");
    fs::write(
        &input,
        serde_json::to_vec(&json!({
            "schemaVersion":1, "serverUrl":"ws://127.0.0.1:9",
            "user":{"mode":"login","email":"input@example.test"},
            "team":{"id":"71"}, "project":{"path":project},
            "tool":{"type":"codex"}, "agent":{"action":"use","id":"201"},
            "mcp":{"mode":"manual"}, "timeoutMs":300000
        }))
        .unwrap(),
    )
    .unwrap();
    let run = || {
        let mut cmd = batch(&config, &input, "json");
        cmd.arg("--password-stdin")
            .arg("--node-path")
            .arg(&node)
            .arg("--mcp-entry")
            .arg(&entry)
            .env("PATH", &search_path);
        cmd
    };
    let lock = devlink_broker::config::state_directory(&config).join("setup.lock");
    let held = devlink_broker::config::lock(&lock).unwrap();

    // An open, silent stdin must not delay the busy result until timeoutMs.
    let mut waiting = run()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdin = waiting.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while waiting.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            waiting.kill().unwrap();
            waiting.wait().unwrap();
            panic!("a busy profile waited for the batch password");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let open = waiting.wait_with_output().unwrap();
    drop(stdin);
    // A closed stdin must report the same conflict, not a missing password.
    let closed = run().output().unwrap();
    for output in [&open, &closed] {
        assert_eq!(output.status.code(), Some(6), "{}", text(output));
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["error"]["code"], "SETUP_CONFIG_BUSY");
        assert_eq!(result["error"]["step"], "validate");
        assert_eq!(result["error"]["location"]["file"], lock.to_str().unwrap());
        assert!(
            result["error"]["requiredInputs"]
                .as_array()
                .is_none_or(Vec::is_empty)
        );
    }
    assert!(!config.exists());

    // Without a competing run the password is still required before the
    // profile is initialized.
    drop(held);
    let missing = run().output().unwrap();
    assert_eq!(missing.status.code(), Some(2), "{}", text(&missing));
    let result: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(result["error"]["code"], "SETUP_INPUT_REQUIRED");
    assert_eq!(result["error"]["requiredInputs"][0]["field"], "password");
    assert!(!config.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattended_login_creates_once_then_session_mode_reuses_the_agent() {
    verify_unattended_login_and_reuse(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an installed Node >=24 and the built MCP entry"]
async fn unattended_setup_with_real_node_and_mcp_runtime() {
    let node = PathBuf::from(std::env::var_os("DEVLINK_TEST_NODE").expect("set DEVLINK_TEST_NODE"));
    let entry = PathBuf::from(
        std::env::var_os("DEVLINK_TEST_MCP_ENTRY").expect("set DEVLINK_TEST_MCP_ENTRY"),
    );
    assert!(node.is_absolute() && node.is_file());
    assert!(entry.is_absolute() && entry.is_file());
    verify_unattended_login_and_reuse(Some((node, entry))).await;
}

async fn verify_unattended_login_and_reuse(runtime: Option<(PathBuf, PathBuf)>) {
    struct StopBroker<'a>(&'a Fixture);
    impl Drop for StopBroker<'_> {
        fn drop(&mut self) {
            let _ = self.0.run(&["stop"], None);
        }
    }

    let app = Fixture::new().await;
    let _stop = StopBroker(&app);
    let project = app._root.path();
    devlink_broker::credentials::set_private_permissions(project).unwrap();
    // Explicit login must call login even when a valid session is present.
    assert!(app.login().status.success());
    let bin = project.join("test-bin");
    let saved_host = tool_fixture::host(&bin, "codex", "codex-cli 0.166.0");
    let synthetic_runtime = runtime.is_none();
    let (node, entry) = runtime.unwrap_or_else(|| tool_fixture::mcp_runtime(&bin));
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let search_path = std::env::join_paths(paths).unwrap();
    let input = project.join("batch.json");
    let config = devlink_broker::config::Config::load(&app.path).unwrap();
    let mut plan = json!({
        "schemaVersion":1, "serverUrl":config.server_url,
        "user":{"mode":"login","email":"input@example.test"},
        "team":{"id":"71"}, "project":{"path":project,"name":"Batch project"},
        "tool":{"type":"codex"}, "agent":{"action":"create","name":"Batch agent",
            "part":"BACKEND","roles":[" API ","API","인증"],"roleDescription":"기존 역할 설명"},
        "mcp":{"mode":"manual"}, "timeoutMs":300000
    });
    let run = |plan: &Value, format: &str| {
        fs::write(&input, serde_json::to_vec(plan).unwrap()).unwrap();
        let mut cmd = batch(&app.path, &input, format);
        cmd.arg("--node-path")
            .arg(&node)
            .arg("--mcp-entry")
            .arg(&entry)
            .env("PATH", &search_path);
        cmd
    };
    let mut child = run(&plan, "jsonl")
        .arg("--password-stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{PASSWORD}\r\n").as_bytes())
        .unwrap();
    let login = child.wait_with_output().unwrap();
    assert_eq!(login.status.code(), Some(3), "{}", text(&login));
    let stored = app.state.agents.lock().unwrap().get(&201).cloned().unwrap();
    assert_eq!(stored["part"], "BACKEND");
    assert_eq!(stored["roles"], json!(["API", "인증"]));
    assert_eq!(stored["roleDescription"], "기존 역할 설명");
    let logins = || {
        app.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == "login")
            .count()
    };

    // The Host is now saved, so its full MCP check waits for sign-in. An entry
    // that is already known locally to be broken must still stop before login.
    if synthetic_runtime {
        let before = logins();
        let mode = node.with_extension("mcp-mode");
        fs::write(&mode, "invalid").unwrap();
        let mut child = run(&plan, "json")
            .arg("--password-stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Validation may fail before stdin is read; a closed pipe is expected then.
        let _ = child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("{PASSWORD}\n").as_bytes());
        let broken = child.wait_with_output().unwrap();
        fs::remove_file(&mode).unwrap();
        assert_eq!(broken.status.code(), Some(5), "{}", text(&broken));
        assert_no_secrets(&broken);
        let error: Value = serde_json::from_slice(&broken.stdout).unwrap();
        assert_eq!(error["error"]["code"], "SETUP_TOOL_UNAVAILABLE");
        assert_eq!(error["error"]["step"], "validate");
        assert_eq!(logins(), before);
    }

    // The saved installation will now be outside PATH, with a different valid
    // installation available under the same command name.
    let alternate_bin = project.join("alternate-bin");
    let alternate = tool_fixture::host(&alternate_bin, "codex", "codex-cli 0.166.0");
    let alternate_path = std::env::join_paths(std::iter::once(alternate_bin).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let selection_file = devlink_broker::setup_tools::path(&app.path);
    let saved_selection = fs::read(&selection_file).unwrap();
    let saved_calls = fs::read_to_string(saved_host.with_extension("calls")).unwrap();

    plan["user"] = json!({"mode":"session"});
    plan["project"] = json!({"path":project});
    plan["agent"] = json!({"action":"use","id":"201"});
    plan["mcp"] = json!({"mode":"apply","targetPath":project.join(".codex/config.toml")});
    let reuse = run(&plan, "json")
        .env("PATH", &alternate_path)
        .output()
        .unwrap();
    let reused_selection = fs::read(&selection_file).unwrap();
    let reused_calls = fs::read_to_string(saved_host.with_extension("calls")).unwrap();
    fs::remove_file(&saved_host).unwrap();
    let unavailable = run(&plan, "json")
        .env("PATH", &alternate_path)
        .output()
        .unwrap();
    assert_no_secrets(&login);
    assert!(login.stderr.is_empty());
    let events: Vec<Value> = String::from_utf8(login.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        events
            .iter()
            .all(|event| event["status"] != "waiting_input")
    );
    assert_eq!(
        events.last().unwrap()["error"]["code"],
        "SETUP_MANUAL_ACTION_REQUIRED"
    );
    assert_eq!(events.last().unwrap()["error"]["known"]["agentId"], "201");

    assert_eq!(reuse.status.code(), Some(0), "{}", text(&reuse));
    assert_no_secrets(&reuse);
    let data: Value = serde_json::from_slice(&reuse.stdout).unwrap();
    assert_eq!(data["data"]["agentId"], "201");
    assert_eq!(data["data"]["connection"]["setupReady"], true);
    assert_eq!(data["stages"][1]["reused"], true);
    assert_eq!(data["stages"][4]["reused"], true);
    assert_eq!(reused_selection, saved_selection);
    assert!(reused_calls.len() > saved_calls.len());
    assert_eq!(unavailable.status.code(), Some(5), "{}", text(&unavailable));
    assert_no_secrets(&unavailable);
    let error: Value = serde_json::from_slice(&unavailable.stdout).unwrap();
    assert_eq!(error["error"]["code"], "SETUP_TOOL_UNAVAILABLE");
    assert_eq!(error["error"]["step"], "select_tool");
    assert_eq!(
        error["error"]["recovery"]["action"],
        "restore_or_reselect_saved_host"
    );
    assert_eq!(error["stages"][5]["status"], "skipped");
    assert_eq!(fs::read(&selection_file).unwrap(), saved_selection);
    assert!(!alternate.with_extension("calls").exists());
    assert_eq!(app.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(app.state.token_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        app.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == "login")
            .count(),
        2
    );
}

/// The desktop app drives setup with batch input, which rechecks a saved Host without
/// rediscovery. A desktop-app update that moves its bundled Host must not strand that
/// selection. The Claude desktop layout is faked through LOCALAPPDATA; the Codex app's
/// package registration cannot be, and shares the same recheck path.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_saved_desktop_app_host_follows_an_app_update_in_unattended_setup() {
    struct StopBroker<'a>(&'a Fixture);
    impl Drop for StopBroker<'_> {
        fn drop(&mut self) {
            let _ = self.0.run(&["stop"], None);
        }
    }
    let home = PathBuf::from(std::env::var_os("USERPROFILE").unwrap());
    if [".local/bin/claude.exe", ".cargo/bin/claude.exe"]
        .iter()
        .any(|p| home.join(p).is_file())
    {
        eprintln!("skipped: a native Claude Code install outranks the synthetic app bundle");
        return;
    }

    let app = Fixture::new().await;
    let _stop = StopBroker(&app);
    let project = app._root.path();
    devlink_broker::credentials::set_private_permissions(project).unwrap();
    assert!(app.login().status.success());
    let bin = project.join("test-bin");
    let (node, entry) = tool_fixture::mcp_runtime(&bin);
    let local = project.join("local-app-data");
    let tools = local.join(r"Claude-Data\claude_test\claude-code");
    tool_fixture::host(
        &tools.join(r"2.1.317\b1"),
        "claude",
        "2.1.317 (Claude Code)",
    );
    let input = project.join("batch.json");
    let config = devlink_broker::config::Config::load(&app.path).unwrap();
    let mut plan = json!({
        "schemaVersion":1, "serverUrl":config.server_url,
        "user":{"mode":"session"}, "team":{"id":"71"},
        "project":{"path":project,"name":"Bundle project"},
        "tool":{"type":"claude-code"}, "agent":{"action":"create","name":"Bundle agent"},
        "mcp":{"mode":"manual"}, "timeoutMs":300000
    });
    let run = |plan: &Value| {
        fs::write(&input, serde_json::to_vec(plan).unwrap()).unwrap();
        let mut cmd = batch(&app.path, &input, "json");
        cmd.arg("--node-path")
            .arg(&node)
            .arg("--mcp-entry")
            .arg(&entry)
            // Only the synthetic runtime is on PATH; the Host comes from the app bundle.
            .env("PATH", &bin)
            .env("LOCALAPPDATA", &local)
            .env("APPDATA", project.join("roaming"));
        cmd.output().unwrap()
    };
    let selection_file = devlink_broker::setup_tools::path(&app.path);
    let saved_command = || {
        let stored: Value = serde_json::from_slice(&fs::read(&selection_file).unwrap()).unwrap();
        stored["selected"]["executable"]["command"]
            .as_str()
            .unwrap()
            .to_owned()
    };

    let first = run(&plan);
    assert_eq!(first.status.code(), Some(3), "{}", text(&first));
    let error: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(error["error"]["code"], "SETUP_MANUAL_ACTION_REQUIRED");
    assert!(saved_command().contains(r"claude-code\2.1.317\b1\claude.exe"));

    // The app updated itself and removed the previous version folder.
    tool_fixture::host(
        &tools.join(r"2.1.321\b2"),
        "claude",
        "2.1.321 (Claude Code)",
    );
    fs::remove_dir_all(tools.join("2.1.317")).unwrap();
    plan["project"] = json!({"path":project});
    plan["agent"] = json!({"action":"use","id":"201","issueToken":false});
    let resumed = run(&plan);
    assert_eq!(resumed.status.code(), Some(3), "{}", text(&resumed));
    let error: Value = serde_json::from_slice(&resumed.stdout).unwrap();
    assert_eq!(error["error"]["code"], "SETUP_MANUAL_ACTION_REQUIRED");
    assert!(saved_command().contains(r"claude-code\2.1.321\b2\claude.exe"));
    assert_eq!(app.state.create_calls.load(Ordering::SeqCst), 1);

    // With the app uninstalled the saved Host is still reported, not replaced.
    let before = fs::read(&selection_file).unwrap();
    fs::remove_dir_all(&tools).unwrap();
    let gone = run(&plan);
    assert_eq!(gone.status.code(), Some(5), "{}", text(&gone));
    let error: Value = serde_json::from_slice(&gone.stdout).unwrap();
    assert_eq!(error["error"]["code"], "SETUP_TOOL_UNAVAILABLE");
    assert_eq!(
        error["error"]["recovery"]["action"],
        "restore_or_reselect_saved_host"
    );
    assert_eq!(fs::read(&selection_file).unwrap(), before);
}
