#[path = "support/account_server.rs"]
mod account_server;
use account_server::{Fixture, assert_no_secrets, text};
use devlink_broker::git_identity::{self, IdentityCache, IdentityClient};
use serde_json::{Value, json};
use std::{process::Output, sync::atomic::Ordering, time::Duration};
use tokio_util::sync::CancellationToken;

const EMAIL: &str = "private-git-identity@example.test";

fn public(output: Output) -> Value {
    assert_no_secrets(&output);
    assert!(!text(&output).contains(EMAIL));
    assert!(!text(&output).contains(&git_identity::email_hash(EMAIL)));
    assert!(output.status.success(), "{}", text(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn fixture() -> Fixture {
    let f = Fixture::new().await;
    assert!(f.login().status.success());
    f
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn user_commands_require_a_live_user_session() {
    let f = Fixture::new().await;
    for command in ["status", "delete"] {
        let output = f.run(&["git-identity", command], None);
        assert!(!output.status.success());
        assert!(text(&output).contains("USER_LOGIN_REQUIRED"));
    }
    assert!(f.login().status.success());
    f.state.expired.store(true, Ordering::SeqCst);
    let output = f.run(
        &["git-identity", "register", "--email-stdin"],
        Some(&format!("{EMAIL}\n")),
    );
    assert!(!output.status.success());
    assert!(!text(&output).contains(EMAIL));
    assert!(f.state.git_hashes.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_broker_start_wires_the_git_identity_refresh_task() {
    struct Stop<'a>(&'a Fixture);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            let _ = self.0.run(&["stop"], None);
        }
    }
    let f = fixture().await;
    let _client = agent(&f).await;
    let _stop = Stop(&f);
    let started = f.run(&["start"], None);
    assert!(started.status.success(), "{}", text(&started));
    tokio::time::timeout(Duration::from_secs(5), async {
        while f.state.git_identity_calls.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn periodic_refresh_observes_a_deleted_registration_without_restart() {
    let f = fixture().await;
    public(f.run(
        &["git-identity", "register", "--email-stdin"],
        Some(&format!("{EMAIL}\n")),
    ));
    let client = agent(&f).await;
    let cache = IdentityCache::default();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(git_identity::run(client, cache.clone(), cancel.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while cache.attribute(EMAIL).await.author_user_id.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    public(f.run(&["git-identity", "delete"], None));
    let refreshed = tokio::time::timeout(
        git_identity::REFRESH_INTERVAL + Duration::from_secs(5),
        async {
            while f.state.git_identity_calls.load(Ordering::SeqCst) < 2
                || cache.attribute(EMAIL).await.author_user_id.is_some()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        },
    )
    .await;
    let after_delete = cache.attribute(EMAIL).await;
    cancel.cancel();
    task.await.unwrap();
    assert!(refreshed.is_ok());
    assert_eq!(after_delete.author_user_id, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_register_status_replace_delete_are_session_scoped_and_never_print_email() {
    let f = fixture().await;
    assert_eq!(
        public(f.run(&["git-identity", "status"], None)),
        json!({"registered":false,"registeredAt":null})
    );
    for email in [
        format!("  {} \r\n", EMAIL.to_uppercase()),
        format!("{EMAIL}\n"),
        "replacement@example.test\n".into(),
    ] {
        let output = f.run(&["git-identity", "register", "--email-stdin"], Some(&email));
        assert!(!text(&output).contains("replacement@example.test"));
        assert_eq!(public(output)["registered"], true);
        assert_eq!(f.state.git_hashes.lock().unwrap().len(), 1);
    }
    assert_eq!(
        public(f.run(&["git-identity", "status"], None))["registered"],
        true
    );
    for _ in 0..2 {
        assert_eq!(
            public(f.run(&["git-identity", "delete"], None)),
            json!({"registered":false,"registeredAt":null})
        );
    }
    assert!(f.state.git_hashes.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_and_invalid_arguments_are_redacted_and_never_retried() {
    let f = fixture().await;
    f.state
        .git_hashes
        .lock()
        .unwrap()
        .insert(88, git_identity::email_hash(EMAIL));
    let duplicate = f.run(
        &["git-identity", "register", "--email-stdin"],
        Some(&format!("{EMAIL}\n")),
    );
    assert!(!duplicate.status.success());
    assert!(text(&duplicate).contains("GIT_EMAIL_ALREADY_REGISTERED"));
    assert!(!text(&duplicate).contains(EMAIL));
    assert_eq!(
        f.state
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == "git_register")
            .count(),
        1
    );
    let before = f.state.calls.lock().unwrap().len();
    for input in [
        "invalid\n".into(),
        "\n".into(),
        format!("{}@example.test\n", "a".repeat(256)),
    ] {
        assert!(
            !f.run(&["git-identity", "register", "--email-stdin"], Some(&input))
                .status
                .success()
        );
    }
    for args in [
        vec!["git-identity", "register", "--email", EMAIL],
        vec!["git-identity", EMAIL],
    ] {
        let output = f.run(&args, None);
        assert_eq!(output.status.code(), Some(2));
        assert!(!text(&output).contains(EMAIL));
    }
    assert_eq!(f.state.calls.lock().unwrap().len(), before);
    f.state.git_identity_mode.store(9, Ordering::SeqCst);
    let reflected = f.run(
        &["git-identity", "register", "--email-stdin"],
        Some(&format!("{EMAIL}\n")),
    );
    assert!(!reflected.status.success());
    assert!(!text(&reflected).contains(EMAIL));
}

async fn agent(f: &Fixture) -> IdentityClient {
    public(f.run(&["team", "select", "--id", "71"], None));
    public(f.run(
        &[
            "agent",
            "create",
            "--name",
            "Git identity",
            "--tool",
            "codex",
            "--project-name",
            "Git project",
            "--project",
            f._root.path().to_str().unwrap(),
        ],
        None,
    ));
    let token = f
        .state
        .tokens
        .lock()
        .unwrap()
        .keys()
        .next()
        .cloned()
        .unwrap();
    let config = devlink_broker::config::Config::load(&f.path).unwrap();
    IdentityClient::new(&config.server_url, &token).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_uses_agent_owner_preserves_transient_failures_and_clears_after_deletion() {
    let f = fixture().await;
    public(f.run(
        &["git-identity", "register", "--email-stdin"],
        Some(&format!("{EMAIL}\n")),
    ));
    let client = agent(&f).await;
    let cache = IdentityCache::default();
    f.state.user_id.store(88, Ordering::SeqCst);
    cache.refresh(&client).await.unwrap();
    assert_eq!(
        cache.attribute(EMAIL).await.author_user_id.as_deref(),
        Some("51")
    );
    assert_eq!(
        cache
            .attribute("different@example.test")
            .await
            .author_user_id,
        None
    );
    for mode in [1, 2, 3, 4, 5] {
        f.state.git_identity_mode.store(0, Ordering::SeqCst);
        cache.refresh(&client).await.unwrap();
        assert!(cache.attribute(EMAIL).await.author_user_id.is_some());
        f.state.git_identity_mode.store(mode, Ordering::SeqCst);
        assert!(cache.refresh(&client).await.is_err());
        assert_eq!(
            cache.attribute(EMAIL).await.author_user_id.as_deref(),
            Some("51")
        );
    }
    f.state.git_identity_mode.store(0, Ordering::SeqCst);
    cache.refresh(&client).await.unwrap();
    f.state.git_hashes.lock().unwrap().remove(&51);
    cache.refresh(&client).await.unwrap();
    assert_eq!(cache.attribute(EMAIL).await.author_user_id, None);
    assert_eq!(
        serde_json::to_value(cache.attribute(EMAIL).await).unwrap(),
        json!({"authorUserId":null})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refresh_loop_fetches_at_start_and_clears_cache_on_shutdown() {
    let f = fixture().await;
    public(f.run(
        &["git-identity", "register", "--email-stdin"],
        Some(&format!("{EMAIL}\n")),
    ));
    let client = agent(&f).await;
    let cache = IdentityCache::default();
    let cancel = CancellationToken::new();
    let task = tokio::spawn(git_identity::run(client, cache.clone(), cancel.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while cache.attribute(EMAIL).await.author_user_id.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    task.await.unwrap();
    assert_eq!(cache.attribute(EMAIL).await.author_user_id, None);
}
