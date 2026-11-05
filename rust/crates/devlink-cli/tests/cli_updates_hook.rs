use devlink_broker::{agent_profile, config, private_file};
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};

fn hook(config: &std::path::Path, cwd: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(config)
        .arg("updates-hook")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

#[test]
fn hook_always_exits_zero_and_stays_silent_without_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    for args in [
        ["--host", "codex", "--event", "session-start"],
        ["--host", "claude-code", "--event", "user-prompt-submit"],
    ] {
        // No config and no bound Agent: nothing to report.
        let output = hook(&path, dir.path(), &args);
        assert!(output.status.success());
        assert!(output.stdout.is_empty() && output.stderr.is_empty());
    }
    for args in [
        ["--host", "other", "--event", "session-start"],
        ["--host", "codex", "--event", "stop"],
    ] {
        let output = hook(&path, dir.path(), &args);
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("must be"));
    }
}

#[test]
fn unreadable_state_is_reported_only_at_session_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    config::init(&path).unwrap();
    private_file::write(&agent_profile::path(&path), &json!("invalid"), 16384).unwrap();

    let prompt = hook(
        &path,
        dir.path(),
        &["--host", "codex", "--event", "user-prompt-submit"],
    );
    assert!(prompt.status.success() && prompt.stdout.is_empty());

    let start = hook(
        &path,
        dir.path(),
        &["--host", "codex", "--event", "session-start"],
    );
    assert!(start.status.success());
    let output: Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(
        output["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    let context = output["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        context.contains("could not check local task updates"),
        "{context}"
    );
    assert!(!context.contains(dir.path().to_str().unwrap()), "{context}");
}
