use serde_json::Value;
use std::{
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

fn cli(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devlink"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Keep stdin open to prove setup never waits for input in this foundation.
    let deadline = Instant::now() + Duration::from_secs(60);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("CLI waited for stdin or failed to exit");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn setup_help_documents_usage_output_and_global_config() {
    let out = cli(&["setup", "--help"]);
    assert!(out.status.success());
    let help = String::from_utf8(out.stdout).unwrap();
    for expected in [
        "setup [OPTIONS]",
        "--config <FILE>",
        "--output",
        "human",
        "jsonl",
        "subsequent work",
    ] {
        assert!(help.contains(expected), "missing {expected}: {help}");
    }
    assert!(out.stderr.is_empty());
}

#[test]
fn setup_initializes_the_selected_profile_then_reuses_it_without_an_init_command() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("프로필 with spaces/config.json");
    for (index, args) in [
        vec![
            "--config",
            path.to_str().unwrap(),
            "setup",
            "--output",
            "json",
        ],
        vec![
            "setup",
            "--output",
            "json",
            "--config",
            path.to_str().unwrap(),
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let out = cli(&args);
        assert_eq!(out.status.code(), Some(3));
        assert!(out.stderr.is_empty());
        let result: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(result["exitCode"], 3);
        assert_eq!(result["error"]["code"], "SETUP_STEP_UNAVAILABLE");
        assert_eq!(result["error"]["step"], "authenticate");
        assert_eq!(result["stages"].as_array().unwrap().len(), 9);
        assert_eq!(result["stages"][0]["status"], "succeeded");
        assert_eq!(
            result["stages"][0]["outcome"],
            if index == 0 { "applied" } else { "not_applied" }
        );
        assert_eq!(result["stages"][0]["reused"], index != 0);
        devlink_broker::config::Config::load(&path).unwrap();
    }
}

#[test]
fn jsonl_streams_progress_then_exactly_one_final_result() {
    let root = tempfile::tempdir().unwrap();
    devlink_broker::credentials::set_private_permissions(root.path()).unwrap();
    let path = root.path().join("config.json");
    let out = cli(&[
        "--config",
        path.to_str().unwrap(),
        "setup",
        "--output",
        "jsonl",
    ]);
    assert_eq!(out.status.code(), Some(3));
    assert!(out.stderr.is_empty());
    let lines: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0]["type"], "progress");
    assert_eq!(lines[0]["stepStatus"], "running");
    assert_eq!(lines[1]["stepStatus"], "succeeded");
    assert_eq!(lines[2]["step"], "authenticate");
    assert_eq!(lines[3]["stepStatus"], "failed");
    assert_eq!(lines[4]["type"], "result");
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["runId"], lines[0]["runId"]);
        assert_eq!(line["sequence"], index + 1);
    }
}

#[test]
fn invalid_setup_arguments_are_redacted_and_exit_two() {
    let secret = "argument-secret-must-not-appear";
    for args in [
        vec!["setup", "--output", secret],
        vec!["--config", "profile.json", "setup", "--output", secret],
        vec!["--config=profile.json", "setup", "--output", secret],
        vec!["setup", "--config", "profile.json", "--output", secret],
        vec!["setup", secret],
    ] {
        let out = cli(&args);
        assert_eq!(out.status.code(), Some(2));
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!output.contains(secret));
        assert!(output.contains("devlink setup --help"));
    }
}

#[test]
fn a_config_value_named_setup_does_not_change_legacy_argument_errors() {
    for args in [
        vec!["--config", "setup", "agent", "create"],
        vec!["--config=setup", "agent", "create"],
    ] {
        let out = cli(&args);
        assert_eq!(out.status.code(), Some(2));
        let error = String::from_utf8(out.stderr).unwrap();
        assert!(error.contains("--name <NAME>"));
        assert!(error.contains("--project <PROJECT>"));
        assert!(!error.contains("Invalid setup arguments"));
    }
}

#[test]
fn existing_init_config_and_mcp_arguments_keep_their_behavior() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    let prefix = ["--config", path.to_str().unwrap()];
    let init = cli(&[prefix[0], prefix[1], "init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    assert!(
        cli(&[
            prefix[0],
            prefix[1],
            "config",
            "set",
            "agent",
            "claude-code"
        ])
        .status
        .success()
    );
    let show = cli(&[prefix[0], prefix[1], "config", "show"]);
    assert!(show.status.success());
    let config: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(config["agent"], "claude-code");
    assert!(cli(&[prefix[0], prefix[1], "mcp", "path"]).status.success());
    for command in ["auth", "team", "agent", "start", "stop", "status", "inbox"] {
        assert!(
            cli(&[command, "--help"]).status.success(),
            "{command} help changed"
        );
    }
}
