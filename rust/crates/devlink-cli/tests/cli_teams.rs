#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, INVITE, assert_no_secrets, text};
use devlink_broker::{credentials, private_file, teams, user_auth};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

fn data(output: &std::process::Output) -> Value {
    assert!(output.status.success(), "{}", text(output));
    assert_no_secrets(output);
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_confirms_owner_membership_and_selection_requires_an_explicit_command() {
    let f = Fixture::new().await;
    let denied = f.run(&["team", "list"], None);
    assert!(!denied.status.success() && text(&denied).contains("USER_LOGIN_REQUIRED"));
    assert!(f.state.calls.lock().unwrap().is_empty());
    assert!(f.login().status.success());
    let created = data(&f.run(&["team", "create", "--name", "New team"], None));
    assert_eq!(created["myRole"], "OWNER");
    assert_eq!(created["inviteCode"], INVITE);
    assert!(!teams::selection_path(&f.path).exists());
    assert_eq!(data(&f.run(&["team", "current"], None))["selected"], false);
    let id = created["teamId"].as_u64().unwrap().to_string();
    let members = data(&f.run(&["team", "members", "--id", &id], None));
    assert_eq!(members[0]["userId"], 51);
    assert_eq!(members[0]["role"], "OWNER");
    let selected = data(&f.run(&["team", "select", "--id", &id], None));
    assert_eq!(selected["agentBindingChanged"], false);
    assert_eq!(
        data(&f.run(&["team", "current"], None))["team"]["teamId"],
        created["teamId"]
    );
    let selection = teams::stored_selection(&f.path).unwrap().unwrap();
    assert_eq!(selection.user_id, 51);
    credentials::verify_private_permissions(&teams::selection_path(&f.path)).unwrap();
    assert!(
        !std::fs::read_to_string(teams::selection_path(&f.path))
            .unwrap()
            .contains(INVITE)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_maps_invalid_codes_and_idempotent_participation_without_echoing_the_code() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    let invalid = f.run(
        &["team", "join", "--code-stdin"],
        Some("invalid-secret-code\n"),
    );
    assert!(!invalid.status.success() && text(&invalid).contains("RESOURCE_NOT_FOUND"));
    assert!(!text(&invalid).contains("invalid-secret-code"));
    let first = f.run(
        &["team", "join", "--code-stdin"],
        Some(&format!("{INVITE}\n")),
    );
    assert_eq!(data(&first)["replayed"], false);
    assert!(!text(&first).contains(INVITE));
    let second = data(&f.run(
        &["team", "join", "--code-stdin"],
        Some(&format!("{INVITE}\n")),
    ));
    assert_eq!(second["replayed"], true);
    assert_eq!(second["myRole"], "MEMBER");
    assert_eq!(
        data(&f.run(&["team", "list"], None))
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        data(&f.run(&["team", "select", "--id", "72"], None))["team"]["teamId"],
        72
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forbidden_or_unconfirmed_membership_preserves_selection_and_all_agent_credentials() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    data(&f.run(&["team", "select", "--id", "71"], None));
    let before = std::fs::read(teams::selection_path(&f.path)).unwrap();
    let mcp = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    let agent = devlink_broker::config::state_directory(&f.path).join("agent-profile.json");
    private_file::write(
        &agent,
        &json!({"centralToken":"synthetic-agent-secret-preserve"}),
        16_384,
    )
    .unwrap();
    let agent_before = std::fs::read(&agent).unwrap();
    let forbidden = f.run(&["team", "select", "--id", "99"], None);
    assert!(!forbidden.status.success() && text(&forbidden).contains("ACCESS_DENIED"));
    assert_eq!(
        before,
        std::fs::read(teams::selection_path(&f.path)).unwrap()
    );
    f.state.membership_missing.store(true, Ordering::SeqCst);
    let missing = f.run(&["team", "select", "--id", "71"], None);
    assert!(!missing.status.success() && text(&missing).contains("not a member"));
    assert_eq!(
        before,
        std::fs::read(teams::selection_path(&f.path)).unwrap()
    );
    assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    assert_eq!(agent_before, std::fs::read(&agent).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn team_switch_only_changes_the_selector_and_current_revalidates_membership() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    data(&f.run(&["team", "select", "--id", "71"], None));
    let user_before = std::fs::read(user_auth::session_path(&f.path)).unwrap();
    let mcp = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    data(&f.run(
        &["team", "join", "--code-stdin"],
        Some(&format!("{INVITE}\n")),
    ));
    data(&f.run(&["team", "select", "--id", "72"], None));
    assert_eq!(
        teams::stored_selection(&f.path).unwrap().unwrap().team_id,
        72
    );
    assert_eq!(
        user_before,
        std::fs::read(user_auth::session_path(&f.path)).unwrap()
    );
    assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    f.state.membership_missing.store(true, Ordering::SeqCst);
    let current = f.run(&["team", "current"], None);
    assert!(!current.status.success());
    assert_no_secrets(&current);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_selection_for_another_user_and_expired_sessions_cannot_authorize_user_commands() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    data(&f.run(&["team", "select", "--id", "71"], None));
    let mut selection = teams::stored_selection(&f.path).unwrap().unwrap();
    selection.user_id = 999;
    private_file::write(&teams::selection_path(&f.path), &selection, 16_384).unwrap();
    let current = f.run(&["team", "current"], None);
    assert!(!current.status.success() && text(&current).contains("current central user"));
    f.state.expired.store(true, Ordering::SeqCst);
    let expired = f.run(&["team", "create", "--name", "Must not be created"], None);
    assert!(!expired.status.success() && text(&expired).contains("USER_SESSION_EXPIRED"));
    assert!(
        !f.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "team_create")
    );
    let before = f.state.calls.lock().unwrap().len();
    assert!(!f.run(&["team", "list"], None).status.success());
    assert_eq!(before, f.state.calls.lock().unwrap().len());
}
