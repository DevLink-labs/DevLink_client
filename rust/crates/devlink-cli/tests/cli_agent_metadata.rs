#[path = "support/account_server.rs"]
mod account_server;

use account_server::{Fixture, assert_no_secrets, text};
use serde_json::{Value, json};
use std::{
    fs,
    process::{Command, Stdio},
    sync::atomic::Ordering,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_list_preserve_part_roles_and_legacy_description() {
    for part in [
        None,
        Some("FRONTEND"),
        Some("BACKEND"),
        Some("INFRA"),
        Some("OTHER"),
    ] {
        let f = Fixture::new().await;
        assert!(f.login().status.success());
        assert!(
            f.run(&["team", "select", "--id", "71"], None)
                .status
                .success()
        );
        let mut args = vec![
            "agent",
            "create",
            "--name",
            "Metadata agent",
            "--tool",
            "codex",
            "--project-name",
            "Metadata project",
            "--project",
            f._root.path().to_str().unwrap(),
            "--role-description",
            "기존 역할 설명",
        ];
        if let Some(part) = part {
            args.extend(["--part", part, "--roles", " API ,API", "--roles", "인증"]);
        }
        let output = f.run(&args, None);
        assert!(output.status.success(), "{}", text(&output));
        assert_no_secrets(&output);
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        let roles = if part.is_some() {
            json!(["API", "인증"])
        } else {
            json!([])
        };
        assert_eq!(result["agent"]["part"], part.unwrap_or("OTHER"));
        assert_eq!(result["agent"]["roles"], roles);
        assert_eq!(result["agent"]["roleDescription"], "기존 역할 설명");
        let list = f.run(&["agent", "list"], None);
        assert!(list.status.success(), "{}", text(&list));
        let agents: Value = serde_json::from_slice(&list.stdout).unwrap();
        let agent = agents
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["agentId"] == 201)
            .unwrap();
        assert_eq!(agent["part"], result["agent"]["part"]);
        assert_eq!(agent["roles"], roles);
        assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_creation_metadata_never_contacts_central() {
    let f = Fixture::new().await;
    let too_long = "😀".repeat(33);
    let too_many = ["tag"; 11].join(",");
    for extra in [
        vec!["--part", "backend"],
        vec!["--roles", " "],
        vec!["--roles", too_long.as_str()],
        vec!["--roles", too_many.as_str()],
    ] {
        let mut args = vec![
            "agent",
            "create",
            "--name",
            "Metadata agent",
            "--tool",
            "codex",
            "--project-name",
            "Project",
            "--project",
            f._root.path().to_str().unwrap(),
        ];
        args.extend(extra);
        assert!(!f.run(&args, None).status.success());
    }
    for extra in [
        vec!["--part", "backend"],
        vec!["--roles", " "],
        vec!["--roles", too_long.as_str()],
        vec!["--roles", too_many.as_str()],
    ] {
        let mut args = vec!["setup", "--output", "jsonl"];
        args.extend(extra);
        let output = f.run(&args, None);
        assert_eq!(output.status.code(), Some(2), "{}", text(&output));
        if args.contains(&"--part") {
            assert!(text(&output).contains("devlink setup --help"));
            assert!(!text(&output).contains("backend"));
            continue;
        }
        let lines: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines.last().unwrap()["error"]["code"],
            "SETUP_INPUT_INVALID"
        );
    }
    assert!(f.state.calls.lock().unwrap().is_empty());
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_ignoring_part_does_not_issue_a_token_or_repeat_creation() {
    creation_is_not_confirmed(5, &["--part", "BACKEND", "--roles", "API"]).await;
}
// GitHub #127: OTHER and no tags are what a missing field reads as locally, so a reply
// without the fields must not confirm that request either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_omitting_part_and_roles_does_not_confirm_a_default_request() {
    creation_is_not_confirmed(5, &[]).await;
}
// The central reports both fields, but with its defaults instead of the request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_storing_defaults_does_not_confirm_a_requested_part_or_roles() {
    creation_is_not_confirmed(6, &["--part", "BACKEND", "--roles", "API"]).await;
}
async fn creation_is_not_confirmed(create_mode: u8, metadata: &[&str]) {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    assert!(
        f.run(&["team", "select", "--id", "71"], None)
            .status
            .success()
    );
    f.state.create_mode.store(create_mode, Ordering::SeqCst);
    let mut args = vec![
        "agent",
        "create",
        "--name",
        "Metadata agent",
        "--tool",
        "codex",
        "--project-name",
        "Project",
        "--project",
        f._root.path().to_str().unwrap(),
    ];
    args.extend_from_slice(metadata);
    // The first run stops at the unconfirmed reply; the rerun finds the pending creation
    // and refuses to create again.
    for reason in ["metadata not confirmed", "AGENT_CREATION_UNKNOWN"] {
        let output = f.run(&args, None);
        assert!(!output.status.success());
        assert!(text(&output).contains(reason), "{}", text(&output));
        assert_no_secrets(&output);
    }
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.state.token_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_batch_metadata_and_use_overrides_are_rejected_before_central() {
    let f = Fixture::new().await;
    let input = f._root.path().join("metadata-input.json");
    let invalid_agents = [
        json!({"action":"create","name":"Agent","part":"backend"}),
        json!({"action":"create","name":"Agent","part":null}),
        json!({"action":"create","name":"Agent","roles":null}),
        json!({"action":"create","name":"Agent","roles":"API"}),
        json!({"action":"create","name":"Agent","roles":[null]}),
        json!({"action":"create","name":"Agent","roles":[" "]}),
        json!({"action":"create","name":"Agent","roles":["😀".repeat(33)]}),
        json!({"action":"create","name":"Agent","roles":vec!["API";11]}),
        json!({"action":"use","id":"301","part":"OTHER"}),
        json!({"action":"use","id":"301","roles":[]}),
    ];
    for agent in invalid_agents {
        let mut plan = json!({"schemaVersion":1,"user":{"mode":"session"},"team":{"id":"71"},
            "project":{"path":f._root.path()},"tool":{"type":"codex"},"agent":agent,"mcp":{"mode":"manual"}});
        if agent["action"] == "create" {
            plan["project"]["name"] = json!("Project");
        }
        fs::write(&input, serde_json::to_vec(&plan).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_devlink"))
            .arg("--config")
            .arg(&f.path)
            .args([
                "setup",
                "--non-interactive",
                "--input",
                input.to_str().unwrap(),
                "--output",
                "jsonl",
            ])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{}", text(&output));
        assert_no_secrets(&output);
        let records: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            records.last().unwrap()["error"]["code"],
            "SETUP_INPUT_INVALID"
        );
        assert_eq!(records.last().unwrap()["error"]["step"], "validate");
    }
    assert!(f.state.calls.lock().unwrap().is_empty());
    assert_eq!(f.state.create_calls.load(Ordering::SeqCst), 0);
}

/// GitHub #127: a central reply without part/roles is refused. The OTHER / no-tags defaults
/// it would otherwise turn into are never cached in the profile or listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replies_without_part_and_roles_are_not_cached_or_listed_as_defaults() {
    const REFUSED: &str = "central did not report the Agent part and role tags";
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    assert!(
        f.run(&["team", "select", "--id", "71"], None)
            .status
            .success()
    );
    let root = f._root.path().to_str().unwrap();
    let created = f.run(
        &[
            "agent",
            "create",
            "--name",
            "Metadata agent",
            "--tool",
            "codex",
            "--project-name",
            "Metadata project",
            "--project",
            root,
            "--part",
            "BACKEND",
            "--roles",
            "login",
        ],
        None,
    );
    assert!(created.status.success(), "{}", text(&created));
    let profile_path = devlink_broker::agent_profile::path(&f.path);
    let cached = fs::read(&profile_path).unwrap();
    let refused = |args: &[&str]| {
        let output = f.run(args, None);
        assert!(!output.status.success(), "{args:?}: {}", text(&output));
        assert!(
            text(&output).contains(REFUSED),
            "{args:?}: {}",
            text(&output)
        );
        assert_no_secrets(&output);
    };

    // GET /agents/{id} leaves them out. Without the check `agent use` would accept the reply
    // and `agent token` would cache OTHER / no tags over BACKEND / login.
    f.state.update_mode.store(5, Ordering::SeqCst);
    refused(&["agent", "use", "--id", "201", "--project", root]);
    refused(&["agent", "token"]);
    assert_eq!(fs::read(&profile_path).unwrap(), cached);
    f.state.update_mode.store(0, Ordering::SeqCst);

    // The team list leaves them out: no row is shown as OTHER with no tags.
    f.state.agent_list_mode.store(5, Ordering::SeqCst);
    refused(&["agent", "list"]);
    f.state.agent_list_mode.store(0, Ordering::SeqCst);

    // /agents/me leaves them out after a token was issued: the profile stays unverified with
    // the metadata the owner lookup reported.
    f.state.agent_context_mode.store(4, Ordering::SeqCst);
    refused(&["agent", "token"]);
    let after: Value = serde_json::from_slice(&fs::read(&profile_path).unwrap()).unwrap();
    assert_eq!(after["state"]["kind"], "needs_token");
    assert_eq!(after["state"]["agent"]["part"], "BACKEND");
    assert_eq!(after["state"]["agent"]["roles"], json!(["login"]));
}
