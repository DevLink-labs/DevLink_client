use devlink_broker::{
    agent_profile,
    config::{self, Config},
    private_file, teams, user_auth,
};
use serde_json::{Value, json};
use std::process::{Command, Output, Stdio};

fn updates(config: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(config)
        .arg("updates")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

fn json_of(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn errors_are_one_json_object_with_a_stable_code() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    config::init(&path).unwrap();

    let unbound = updates(&path, &[]);
    assert_eq!(unbound.status.code(), Some(1));
    assert!(unbound.stderr.is_empty());
    let value = json_of(&unbound);
    assert_eq!(value["schemaVersion"], 1);
    assert_eq!(value["error"]["code"], "UPDATES_AGENT_UNBOUND");
    assert!(value["error"]["message"].is_string());

    let invalid = updates(&path, &["--cursor", "v3.0.0.0.0.bad"]);
    assert_eq!(invalid.status.code(), Some(2));
    assert_eq!(json_of(&invalid)["error"]["code"], "UPDATES_INPUT_INVALID");

    // Argument errors stay with clap: exit 2, no JSON on stdout.
    for args in [&["--latest", "--cursor", "x"][..], &["--limit", "51"][..]] {
        let output = updates(&path, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn bound_profile_returns_scope_and_a_resumable_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.json");
    config::init(&path).unwrap();
    config::set(&path, "serverUrl", "ws://127.0.0.1:9").unwrap();
    let origin = user_auth::origin(&Config::load(&path).unwrap())
        .unwrap()
        .to_string();
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).unwrap();
    let project = project.canonicalize().unwrap();
    private_file::write(
        &teams::selection_path(&path),
        &json!({"schemaVersion":1,"origin":origin,"userId":3,"teamId":1}),
        16384,
    )
    .unwrap();
    private_file::write(
        &agent_profile::path(&path),
        &json!({"schemaVersion":1,"origin":origin,"userId":3,"teamId":1,
            "projectRoot":project.to_str().unwrap(),"state":{"kind":"ready","agent":{"agentId":12,"teamId":1,
                "ownerUserId":3,"name":"fixture","roleDescription":null,"toolType":"CODEX",
                "projectName":"project","status":"OFFLINE","lastSeenAt":null,"createdAt":"synthetic"},
                "token":"synthetic-token"}}),
        16384,
    )
    .unwrap();

    let latest = updates(&path, &["--latest"]);
    assert!(latest.status.success());
    let value = json_of(&latest);
    assert_eq!(value["scope"]["agentId"], "12");
    assert_eq!(value["scope"]["toolType"], "codex");
    assert_eq!(value["updates"], json!([]));
    let cursor = value["nextCursor"].as_str().unwrap();

    let next = updates(&path, &["--cursor", cursor, "--limit", "5"]);
    assert!(next.status.success());
    let value = json_of(&next);
    assert_eq!(value["nextCursor"], cursor);
    assert_eq!(value["cursorReset"], false);
    assert_eq!(value["hasMore"], false);
}
