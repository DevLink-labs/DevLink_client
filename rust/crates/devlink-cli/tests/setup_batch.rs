#[path = "support/account_server.rs"]
mod account_server;

use account_server::{Fixture, PASSWORD, assert_no_secrets, text, tool_fixture};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::Path,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattended_login_creates_once_then_session_mode_reuses_the_agent() {
    let app = Fixture::new().await;
    let project = app._root.path();
    devlink_broker::credentials::set_private_permissions(project).unwrap();
    // Explicit login must call login even when a valid session is present.
    assert!(app.login().status.success());
    let bin = project.join("test-bin");
    let saved_host = tool_fixture::host(&bin, "codex", "codex-cli 0.166.0");
    let (node, entry) = tool_fixture::mcp_runtime(&bin);
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
        "tool":{"type":"codex"}, "agent":{"action":"create","name":"Batch agent"},
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
    let _ = app.run(&["stop"], None);

    assert_eq!(login.status.code(), Some(3), "{}", text(&login));
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
