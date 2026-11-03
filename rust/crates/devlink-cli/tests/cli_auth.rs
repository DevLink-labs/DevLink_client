#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, text};
use devlink_broker::{
    config::{self, Config},
    credentials, user_auth,
};
use serde_json::Value;
use std::sync::atomic::Ordering;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_rotates_cookie_csrf_verifies_identity_and_persists_private_session() {
    let f = Fixture::new().await;
    let output = f.login();
    assert!(output.status.success(), "{}", text(&output));
    assert_no_secrets(&output);
    assert!(!text(&output).contains('\u{1b}') && !text(&output).contains('\u{202e}'));
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["user"]["userId"], 51);
    assert_eq!(data["user"]["email"], "verified@example.test");
    let path = user_auth::session_path(&f.path);
    credentials::verify_path_safety(&path).unwrap();
    credentials::verify_private_permissions(&path).unwrap();
    assert!(!credentials::path(&f.path).exists());
    let status = f.run(&["auth", "status"], None);
    assert!(status.status.success(), "{}", text(&status));
    assert_no_secrets(&status);
    assert_eq!(
        *f.state.calls.lock().unwrap(),
        ["csrf", "login", "me", "csrf", "me"]
    );
    let saved = std::fs::read_to_string(path).unwrap();
    assert!(saved.contains(account_server::COOKIE));
    assert!(!saved.contains(account_server::PASSWORD) && !saved.contains(account_server::CSRF));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_login_preserves_previous_session_and_never_echoes_server_reflection() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    let before = std::fs::read(user_auth::session_path(&f.path)).unwrap();
    let output = f.run(
        &[
            "auth",
            "login",
            "--email",
            "bad@example.test",
            "--password-stdin",
        ],
        Some("wrong-password\n"),
    );
    assert!(!output.status.success());
    assert!(text(&output).contains("INVALID_CREDENTIALS"));
    assert_no_secrets(&output);
    assert!(!text(&output).contains("wrong-password"));
    assert_eq!(
        before,
        std::fs::read(user_auth::session_path(&f.path)).unwrap()
    );
    assert!(f.run(&["auth", "status"], None).status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_password_stdin_reports_the_missing_value_instead_of_a_cancellation() {
    let f = Fixture::new().await;
    let output = f.run(
        &[
            "auth",
            "login",
            "--email",
            "empty@example.test",
            "--password-stdin",
        ],
        Some(""),
    );
    assert!(!output.status.success());
    assert!(text(&output).contains("No Password was received on stdin"));
    assert!(f.state.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expiry_clears_secret_and_future_user_commands_require_reauthentication() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    f.state.expired.store(true, Ordering::SeqCst);
    let output = f.run(&["auth", "status"], None);
    assert!(!output.status.success() && text(&output).contains("USER_SESSION_EXPIRED"));
    assert_no_secrets(&output);
    let saved = std::fs::read_to_string(user_auth::session_path(&f.path)).unwrap();
    assert!(!saved.contains(account_server::COOKIE));
    let count = f.state.calls.lock().unwrap().len();
    let status = f.run(&["auth", "status"], None);
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["state"], "expired");
    assert_eq!(count, f.state.calls.lock().unwrap().len());
    assert!(user_auth::UserClient::authenticated(&f.path).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logout_invalidates_remote_session_and_clears_only_user_secrets_even_on_failure() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    let mcp = credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    let result = f.run(&["auth", "logout"], None);
    assert!(result.status.success(), "{}", text(&result));
    assert!(f.state.expired.load(Ordering::SeqCst));
    assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    assert_no_secrets(&result);
    assert!(f.login().status.success());
    f.state.logout_error.store(true, Ordering::SeqCst);
    let output = f.run(&["auth", "logout"], None);
    assert!(!output.status.success() && text(&output).contains("central logout not confirmed"));
    assert_no_secrets(&output);
    assert!(
        !std::fs::read_to_string(user_auth::session_path(&f.path))
            .unwrap()
            .contains(account_server::COOKIE)
    );
    assert_eq!(credentials::load(&f.path).unwrap(), mcp);
    assert!(user_auth::UserClient::authenticated(&f.path).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_changes_and_cleartext_remote_auth_are_rejected_before_sending_secrets() {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    let count = f.state.calls.lock().unwrap().len();
    let mut config = Config::load(&f.path).unwrap();
    config.server_url = "ws://127.0.0.1:9".into();
    config::atomic_json(&f.path, &config).unwrap();
    let output = f.run(&["auth", "status"], None);
    assert!(output.status.success() && text(&output).contains("origin_mismatch"));
    assert_no_secrets(&output);
    config.server_url = "ws://central.example.test".into();
    config::atomic_json(&f.path, &config).unwrap();
    let output = f.login();
    assert!(!output.status.success() && text(&output).contains("HTTPS"));
    assert_no_secrets(&output);
    assert_eq!(count, f.state.calls.lock().unwrap().len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redirects_malformed_private_snapshots_and_password_argument_are_rejected() {
    let f = Fixture::new().await;
    f.state.redirect_login.store(true, Ordering::SeqCst);
    let output = f.login();
    assert!(!output.status.success() && text(&output).contains("redirect"));
    assert_no_secrets(&output);
    assert_eq!(*f.state.calls.lock().unwrap(), ["csrf", "login"]);
    assert!(!user_auth::session_path(&f.path).exists());
    let output = f.run(
        &[
            "auth",
            "login",
            "--email",
            "a@example.test",
            "--password",
            "not-allowed",
        ],
        None,
    );
    assert!(!output.status.success());
    assert!(!text(&output).contains("not-allowed"));
    let path = user_auth::session_path(&f.path);
    std::fs::write(&path, b"{\"cookie\":\"do-not-echo-malformed-secret\"").unwrap();
    credentials::set_private_permissions(&path).unwrap();
    let output = f.run(&["auth", "status"], None);
    assert!(output.status.success() && text(&output).contains("invalid_local_session"));
    assert!(!text(&output).contains("do-not-echo-malformed-secret"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_or_corrupt_safe_snapshot_can_be_replaced_without_contacting_old_origin() {
    let first = Fixture::new().await;
    let second = Fixture::new().await;
    assert!(first.login().status.success());
    let mut config = Config::load(&first.path).unwrap();
    config.server_url = Config::load(&second.path).unwrap().server_url;
    config::atomic_json(&first.path, &config).unwrap();
    assert!(text(&first.run(&["auth", "status"], None)).contains("origin_mismatch"));
    assert!(first.run(&["auth", "logout"], None).status.success());
    assert!(second.state.calls.lock().unwrap().is_empty());
    assert!(first.login().status.success());
    let path = user_auth::session_path(&first.path);
    std::fs::write(&path, b"{invalid-json").unwrap();
    credentials::set_private_permissions(&path).unwrap();
    assert!(text(&first.run(&["auth", "status"], None)).contains("invalid_local_session"));
    assert!(first.login().status.success());
    assert!(first.run(&["auth", "status"], None).status.success());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_verified_user_expires_and_5xx_preserves_the_original_snapshot() {
    let f = Fixture::new().await;
    let rejected = f.run(
        &[
            "auth",
            "login",
            "--email",
            "nobody@example.test",
            "--password-stdin",
        ],
        Some("wrong\n"),
    );
    assert!(!rejected.status.success());
    assert!(!user_auth::session_path(&f.path).exists());
    assert!(f.login().status.success());
    let before = std::fs::read(user_auth::session_path(&f.path)).unwrap();
    f.state.me_failure.store(true, Ordering::SeqCst);
    let failed = f.run(&["auth", "status"], None);
    assert!(!failed.status.success());
    assert_no_secrets(&failed);
    assert_eq!(
        before,
        std::fs::read(user_auth::session_path(&f.path)).unwrap()
    );
    f.state.me_failure.store(false, Ordering::SeqCst);
    f.state.changed_user.store(true, Ordering::SeqCst);
    let changed = f.run(&["auth", "status"], None);
    assert!(!changed.status.success() && text(&changed).contains("identity changed"));
    assert!(
        !std::fs::read_to_string(user_auth::session_path(&f.path))
            .unwrap()
            .contains(account_server::COOKIE)
    );
    assert!(text(&f.run(&["auth", "status"], None)).contains("expired"));
}
