#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, text};
use devlink_broker::{
    agent_profile,
    config::{self, Config},
    credentials, private_file, teams, user_auth,
};
use devlink_cli::setup::{
    self, Completion, NoLoginInput, Outcome, SetupContext, SetupStages, StageStatus, Step,
    TeamChoice, TeamInput, TeamInputError,
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

async fn fixture() -> Fixture {
    let f = Fixture::new().await;
    credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    credentials::set_private_permissions(&f.path).unwrap();
    assert!(f.login().status.success());
    f.state.calls.lock().unwrap().clear();
    f
}
fn parsed(f: &Fixture, args: &[&str], exit: i32) -> Value {
    let output = f.run(args, None);
    assert_eq!(output.status.code(), Some(exit), "{}", text(&output));
    assert_no_secrets(&output);
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).unwrap()
}
fn selected(f: &Fixture, user: u64, team: u64) {
    private_file::write(
        &teams::selection_path(&f.path),
        &teams::Selection {
            schema_version: 1,
            origin: user_auth::origin(&Config::load(&f.path).unwrap())
                .unwrap()
                .to_string(),
            user_id: user,
            team_id: team,
        },
        16_384,
    )
    .unwrap();
}
fn multiple(f: &Fixture) {
    f.state.joined.store(true, Ordering::SeqCst);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_team_is_verified_saved_privately_and_then_reused_without_rewriting() {
    let f = fixture().await;
    let value = parsed(&f, &["setup", "--output", "json"], 2);
    assert_eq!(value["stages"][2]["status"], "succeeded");
    assert_eq!(value["stages"][2]["reused"], false);
    assert_eq!(value["error"]["step"], "register_agent");
    assert_eq!(value["error"]["known"]["teamId"], "71");
    assert_eq!(
        *f.state.calls.lock().unwrap(),
        [
            "me",
            "me",
            "team_list",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "me",
            "team_detail",
            "team_members",
            "agents_owned"
        ]
    );
    let path = teams::selection_path(&f.path);
    credentials::verify_private_permissions(&path).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    multiple(&f);
    let value = parsed(&f, &["setup", "--output", "json"], 2);
    assert_eq!(value["stages"][2]["reused"], true);
    assert_eq!(value["stages"][2]["outcome"], "not_applied");
    assert_eq!(bytes, std::fs::read(&path).unwrap());
    assert_eq!(
        modified,
        std::fs::metadata(&path).unwrap().modified().unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn multiple_teams_return_current_choices_and_required_input_without_waiting() {
    let f = fixture().await;
    multiple(&f);
    let value = parsed(&f, &["setup", "--output", "json"], 2);
    assert_eq!(value["error"]["code"], "SETUP_TEAM_REQUIRED");
    assert_eq!(value["error"]["requiredInputs"][0]["field"], "team.id");
    assert_eq!(
        value["error"]["requiredInputs"][0]["source"],
        "verified_team_list"
    );
    assert_eq!(value["error"]["teamChoices"][0]["id"], "71");
    assert_eq!(value["error"]["teamChoices"][1]["id"], "72");
    assert!(!teams::selection_path(&f.path).exists());
    assert_eq!(*f.state.calls.lock().unwrap(), ["me", "me", "team_list"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_joined_team_overrides_saved_selection_without_changing_other_credentials() {
    let f = fixture().await;
    selected(&f, 51, 71);
    multiple(&f);
    let mcp = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    let value = parsed(&f, &["setup", "--team-id", "72", "--output", "json"], 2);
    assert_eq!(value["error"]["known"]["teamId"], "72");
    assert_eq!(
        teams::stored_selection(&f.path).unwrap().unwrap().team_id,
        72
    );
    assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    assert!(!agent_profile::path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_users_foreign_origins_and_departed_teams_are_not_reused() {
    let f = fixture().await;
    multiple(&f);
    for (user, team, foreign) in [(999, 71, false), (51, 999, false), (51, 71, true)] {
        selected(&f, user, team);
        if foreign {
            let mut s = teams::stored_selection(&f.path).unwrap().unwrap();
            s.origin = "https://foreign.example.test/".into();
            private_file::write(&teams::selection_path(&f.path), &s, 16_384).unwrap();
        }
        let before = std::fs::read(teams::selection_path(&f.path)).unwrap();
        let value = parsed(&f, &["setup", "--output", "json"], 2);
        assert_eq!(value["error"]["code"], "SETUP_TEAM_REQUIRED");
        assert_eq!(
            before,
            std::fs::read(teams::selection_path(&f.path)).unwrap()
        );
    }
    f.state.joined.store(false, Ordering::SeqCst);
    let value = parsed(&f, &["setup", "--output", "json"], 2);
    assert_eq!(value["stages"][2]["reused"], false);
    let s = teams::stored_selection(&f.path).unwrap().unwrap();
    assert_eq!((s.user_id, s.team_id), (51, 71));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_teams_offer_existing_create_join_commands_without_creating_or_deleting_anything() {
    let f = fixture().await;
    selected(&f, 51, 71);
    f.state.team_list_mode.store(1, Ordering::SeqCst);
    let before = std::fs::read(teams::selection_path(&f.path)).unwrap();
    let value = parsed(&f, &["setup", "--output", "json"], 3);
    assert_eq!(value["error"]["code"], "SETUP_TEAM_MEMBERSHIP_REQUIRED");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("devlink team create")
    );
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("devlink team join")
    );
    assert_eq!(
        before,
        std::fs::read(teams::selection_path(&f.path)).unwrap()
    );
    assert_eq!(*f.state.calls.lock().unwrap(), ["me", "me", "team_list"]);
    let explicit = parsed(&f, &["setup", "--team-id", "71", "--output", "json"], 3);
    assert_eq!(explicit["error"]["code"], "SETUP_TEAM_MEMBERSHIP_REQUIRED");
    assert_eq!(
        before,
        std::fs::read(teams::selection_path(&f.path)).unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unjoined_explicit_id_or_unconfirmed_membership_cannot_replace_saved_selection() {
    let f = fixture().await;
    selected(&f, 51, 71);
    let before = std::fs::read(teams::selection_path(&f.path)).unwrap();
    for id in ["99", "999"] {
        let value = parsed(&f, &["setup", "--team-id", id, "--output", "json"], 4);
        assert_eq!(value["error"]["code"], "SETUP_TEAM_FORBIDDEN");
    }
    f.state.membership_missing.store(true, Ordering::SeqCst);
    let value = parsed(&f, &["setup", "--output", "json"], 4);
    assert_eq!(value["error"]["code"], "SETUP_TEAM_FORBIDDEN");
    assert_eq!(value["stages"][3]["status"], "skipped");
    assert_eq!(
        before,
        std::fs::read(teams::selection_path(&f.path)).unwrap()
    );
}

fn binding(f: &Fixture, user: u64, team: u64, state: Value) {
    private_file::write(&agent_profile::path(&f.path), &json!({
        "schemaVersion":1,"origin":user_auth::origin(&Config::load(&f.path).unwrap()).unwrap().as_str(),
        "userId":user,"teamId":team,"projectRoot":f.path.parent().unwrap(),"state":state,
    }), 16_384).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_binding_conflicts_preserve_selector_agent_and_mcp_including_pending_creation() {
    let f = fixture().await;
    multiple(&f);
    selected(&f, 51, 71);
    let selector = std::fs::read(teams::selection_path(&f.path)).unwrap();
    let mcp = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    for (user, team, state) in [
        (
            51,
            71,
            json!({"kind":"create_pending","name":"Pending","tool":"CODEX","project_name":"Project"}),
        ),
        (
            999,
            72,
            json!({"kind":"create_pending","name":"Pending","tool":"CODEX","project_name":"Project"}),
        ),
        (
            51,
            71,
            json!({"kind":"ready","token":"synthetic-private-agent-token","agent":{
                "agentId":301,"teamId":71,"ownerUserId":51,"name":"Existing","roleDescription":null,
                "toolType":"CODEX","projectName":"Project","status":"OFFLINE","lastSeenAt":null,"createdAt":"2026-11-02"
            }}),
        ),
    ] {
        binding(&f, user, team, state);
        let agent = std::fs::read(agent_profile::path(&f.path)).unwrap();
        let value = parsed(&f, &["setup", "--team-id", "72", "--output", "json"], 6);
        assert_eq!(value["error"]["code"], "SETUP_TEAM_BINDING_CONFLICT");
        assert_eq!(
            selector,
            std::fs::read(teams::selection_path(&f.path)).unwrap()
        );
        assert_eq!(agent, std::fs::read(agent_profile::path(&f.path)).unwrap());
        assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    }
    binding(
        &f,
        51,
        72,
        json!({"kind":"create_pending","name":"Pending","tool":"CODEX","project_name":"Project"}),
    );
    assert_eq!(
        parsed(
            &f,
            &[
                "setup",
                "--team-id",
                "72",
                "--project",
                f.path.parent().unwrap().to_str().unwrap(),
                "--output",
                "json",
            ],
            3
        )["stages"][2]["status"],
        "succeeded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_failure_and_duplicate_team_ids_preserve_the_previous_selection() {
    let f = fixture().await;
    selected(&f, 51, 71);
    let before = std::fs::read(teams::selection_path(&f.path)).unwrap();
    for (mode, code, exit) in [
        (2, "SETUP_CENTRAL_UNAVAILABLE", 7),
        (3, "SETUP_PROTOCOL_ERROR", 7),
        (8, "SETUP_CENTRAL_UNAVAILABLE", 7),
        (4, "SETUP_AUTH_REQUIRED", 4),
    ] {
        f.state.team_list_mode.store(mode, Ordering::SeqCst);
        let value = parsed(&f, &["setup", "--output", "json"], exit);
        assert_eq!(value["error"]["code"], code);
        assert_eq!(
            before,
            std::fs::read(teams::selection_path(&f.path)).unwrap()
        );
        let session: Value = private_file::read(&user_auth::session_path(&f.path), 16_384)
            .unwrap()
            .unwrap();
        assert_eq!(
            session["state"]["kind"],
            if mode == 4 {
                "expired"
            } else {
                "authenticated"
            }
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_404_is_distinct_from_selected_team_404_and_preserves_local_state() {
    let f = fixture().await;
    selected(&f, 51, 71);
    let before = std::fs::read(teams::selection_path(&f.path)).unwrap();
    for mode in [5, 6, 7] {
        f.state.team_list_mode.store(mode, Ordering::SeqCst);
        f.state.calls.lock().unwrap().clear();
        let value = parsed(
            &f,
            &["setup", "--output", "json"],
            if mode == 5 { 7 } else { 4 },
        );
        assert_eq!(value["error"]["step"], "select_team");
        assert_eq!(value["error"]["outcome"], "not_applied");
        assert_eq!(value["stages"][3]["status"], "skipped");
        assert_eq!(
            before,
            std::fs::read(teams::selection_path(&f.path)).unwrap()
        );
        let session: Value = private_file::read(&user_auth::session_path(&f.path), 16_384)
            .unwrap()
            .unwrap();
        assert_eq!(session["state"]["kind"], "authenticated");
        if mode == 5 {
            assert_eq!(value["error"]["code"], "SETUP_TEAM_LIST_RESOURCE_NOT_FOUND");
            assert_eq!(
                value["error"]["recovery"]["action"],
                "inspect_central_team_data"
            );
            assert_eq!(value["error"]["recovery"]["automatic"], false);
            assert!(value["error"]["location"].is_null());
            assert!(
                value["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("server administrator")
            );
            assert_eq!(*f.state.calls.lock().unwrap(), ["me", "me", "team_list"]);
            let output = f.run(&["team", "list"], None);
            assert!(!output.status.success());
            assert!(text(&output).contains("RESOURCE_NOT_FOUND"));
            assert_no_secrets(&output);
        } else {
            assert_eq!(value["error"]["code"], "SETUP_TEAM_FORBIDDEN");
            assert_eq!(value["error"]["recovery"]["action"], "select_verified_team");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_selection_schema_is_preserved_instead_of_silently_replaced() {
    let f = fixture().await;
    selected(&f, 51, 71);
    let path = teams::selection_path(&f.path);
    let mut value: Value = private_file::read(&path, 16_384).unwrap().unwrap();
    value["schemaVersion"] = json!(99);
    private_file::write(&path, &value, 16_384).unwrap();
    let before = std::fs::read(&path).unwrap();
    let result = parsed(&f, &["setup", "--output", "json"], 5);
    assert_eq!(result["error"]["code"], "SETUP_TEAM_STORAGE_FAILED");
    assert_eq!(before, std::fs::read(path).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_profile_operation_cannot_publish_a_selection() {
    let f = fixture().await;
    let _control = config::lock(&config::state_directory(&f.path).join("control.lock")).unwrap();
    let origin = user_auth::origin(&Config::load(&f.path).unwrap()).unwrap();
    let Err(error) = teams::select_for_setup(&f.path, origin.as_str(), 51, Some(71)).await else {
        panic!("expected contention")
    };
    assert_eq!(
        error.downcast_ref::<teams::SetupTeamFault>().unwrap().kind,
        teams::SetupTeamFaultKind::Busy
    );
    assert!(!teams::selection_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_change_during_membership_response_cannot_save_or_reuse_the_old_team() {
    for existing in [false, true] {
        let f = fixture().await;
        if existing {
            selected(&f, 51, 71);
        }
        let before = std::fs::read(teams::selection_path(&f.path)).ok();
        f.state.pause_members.store(true, Ordering::SeqCst);
        let path = f.path.clone();
        let pending = tokio::task::spawn_blocking(move || {
            account_server::run_at(&path, &["setup", "--output", "json"], None)
        });
        if tokio::time::timeout(
            std::time::Duration::from_secs(15),
            f.state.members_entered.notified(),
        )
        .await
        .is_err()
        {
            f.state.members_resume.notify_one();
            let _ = pending.await;
            panic!("setup did not reach the membership response barrier");
        }
        // A real config command takes control.lock, independently of account.lock.
        let changed = f.run(
            &["config", "set", "serverUrl", "wss://changed.example.test"],
            None,
        );
        f.state.members_resume.notify_one();
        assert!(changed.status.success(), "{}", text(&changed));
        let output = pending.await.unwrap();
        assert_no_secrets(&output);
        assert_eq!(
            std::fs::read(teams::selection_path(&f.path)).ok(),
            before,
            "selection changed after the server origin changed"
        );
        assert_eq!(output.status.code(), Some(6), "{}", text(&output));
        assert!(output.stderr.is_empty());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "SETUP_AUTH_SESSION_CONFLICT");
        assert_eq!(value["error"]["step"], "select_team");
        assert_eq!(value["stages"][2]["status"], "failed");
        assert_eq!(value["stages"][2]["outcome"], "not_applied");
        assert_eq!(value["stages"][3]["status"], "skipped");
        assert_eq!(
            value["error"]["location"]["file"],
            f.path.to_string_lossy().as_ref()
        );
        assert_eq!(
            Config::load(&f.path).unwrap().server_url,
            "wss://changed.example.test"
        );
        assert_eq!(
            *f.state.calls.lock().unwrap(),
            ["me", "me", "team_list", "team_detail", "team_members"]
        );
    }
}

struct Input {
    calls: Arc<AtomicUsize>,
    state: account_server::MockState,
    path: std::path::PathBuf,
    mode: u8,
}
impl TeamInput for Input {
    fn available(&self) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn request(&mut self, choices: &[TeamChoice]) -> Result<u64, TeamInputError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(choices.len(), 2);
        match self.mode {
            1 => {
                self.state.joined.store(false, Ordering::SeqCst);
            }
            2 => return Err(TeamInputError::Cancelled),
            3 => {
                let mut s: Value = private_file::read(&user_auth::session_path(&self.path), 16_384)
                    .unwrap()
                    .unwrap();
                s["state"]["user"]["userId"] = json!(999);
                private_file::write(&user_auth::session_path(&self.path), &s, 16_384).unwrap();
            }
            4 => {
                let mut config = Config::load(&self.path).unwrap();
                config.server_url = "wss://must-not-contact.example.test".into();
                config::atomic_json(&self.path, &config).unwrap();
            }
            _ => {}
        }
        Ok(72)
    }
}
async fn injected(
    f: &Fixture,
    mode: u8,
) -> (
    setup::RunResult<devlink_broker::setup_connection::Report>,
    Vec<setup::ProgressEvent>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let input = Input {
        calls: calls.clone(),
        state: f.state.clone(),
        path: f.path.clone(),
        mode,
    };
    let mut events = Vec::new();
    let result = setup::run(
        &SetupContext {
            config_path: f.path.clone(),
        },
        SetupStages::new(NoLoginInput)
            .with_team_input(input)
            .with_project(devlink_broker::setup_project::ProjectOptions {
                path: Some(f._root.path().to_path_buf()),
                ..Default::default()
            })
            .with_mcp(account_server::tool_fixture::mcp_options(
                &f._root.path().join("mcp-runtime"),
            ))
            .with_tool(
                Some(devlink_broker::sessions::ToolType::Codex),
                Some(account_server::test_tool().to_path_buf()),
            ),
        |event| {
            events.push(event.clone());
            Ok(())
        },
    )
    .await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    (result, events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interactive_selection_emits_choices_resumes_only_team_step_and_checks_membership_again() {
    let f = fixture().await;
    multiple(&f);
    let (result, events) = injected(&f, 0).await;
    assert_eq!(result.exit_code, 2);
    let waiting = events
        .iter()
        .find(|e| e.step_status == StageStatus::WaitingInput)
        .unwrap();
    assert_eq!(waiting.step, Step::SelectTeam);
    assert_eq!(waiting.team_choices[1].id, "72");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.step == Step::Authenticate && e.step_status == StageStatus::Running)
            .count(),
        1
    );
    assert_eq!(
        teams::stored_selection(&f.path).unwrap().unwrap().team_id,
        72
    );
    assert_eq!(
        f.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| *c == "team_list")
            .count(),
        2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_membership_while_waiting_cannot_publish_a_cached_choice() {
    let f = fixture().await;
    multiple(&f);
    let (result, _) = injected(&f, 1).await;
    assert_eq!(result.exit_code, 4);
    assert!(!teams::selection_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_team_input_does_not_change_the_selection_or_repeat_authentication() {
    let f = fixture().await;
    multiple(&f);
    let (result, _) = injected(&f, 2).await;
    assert_eq!(result.exit_code, 130);
    assert_eq!(result.stages[2].status, StageStatus::Cancelled);
    assert_eq!(result.stages[2].outcome, Outcome::NotApplied);
    assert!(!teams::selection_path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changing_user_or_origin_while_waiting_stops_before_another_server_request() {
    for mode in [3, 4] {
        let f = fixture().await;
        multiple(&f);
        let (result, _) = injected(&f, mode).await;
        assert_eq!(result.exit_code, 6);
        let Completion::Failure { error } = result.completion else {
            panic!("expected conflict")
        };
        assert_eq!(error.code, setup::ErrorCode::AuthSessionConflict);
        assert_eq!(*f.state.calls.lock().unwrap(), ["me", "me", "team_list"]);
        assert!(!teams::selection_path(&f.path).exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_team_arguments_are_redacted_and_long_ids_remain_exact() {
    let f = fixture().await;
    for id in [
        "0",
        "01",
        "+71",
        " 71",
        "9223372036854775808",
        "private-invalid-id",
    ] {
        let output = f.run(&["setup", "--team-id", id], None);
        assert_eq!(output.status.code(), Some(2));
        assert!(!text(&output).contains(id));
    }
    f.state.team_list_mode.store(1, Ordering::SeqCst);
    let id = 9223372036854775807u64;
    f.state.teams.lock().unwrap().insert(
        id,
        json!({"teamId":id,"name":"Long ID","myRole":"MEMBER","createdAt":"2026-11-02"}),
    );
    let value = parsed(
        &f,
        &[
            "setup",
            "--team-id",
            "9223372036854775807",
            "--output",
            "json",
        ],
        2,
    );
    assert_eq!(value["error"]["known"]["teamId"], "9223372036854775807");
    assert_eq!(
        teams::stored_selection(&f.path).unwrap().unwrap().team_id,
        id
    );
}
