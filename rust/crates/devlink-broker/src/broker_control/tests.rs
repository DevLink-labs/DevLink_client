use super::*;
use std::{
    fs,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    credentials::set_private_permissions(root.path()).unwrap();
    let path = root.path().join("config.json");
    config::init(&path).unwrap();
    (root, path)
}

fn child(exit: bool) -> Child {
    static HELPER: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let exe = &HELPER
        .get_or_init(|| {
            let root = tempfile::tempdir().unwrap();
            let source = root.path().join("child.rs");
            fs::write(
                &source,
                r#"fn main() {
            if std::env::args().any(|arg| arg == "--exit") { return; }
            std::thread::sleep(std::time::Duration::from_secs(30));
        }"#,
            )
            .unwrap();
            let exe = root
                .path()
                .join(if cfg!(windows) { "child.exe" } else { "child" });
            let mut build = Command::new("rustc");
            build.arg(source).arg("-o").arg(&exe);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                build.creation_flags(0x08000000);
            }
            let output = build.output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            (root, exe)
        })
        .1;
    let mut command = Command::new(exe);
    if exit {
        command.arg("--exit");
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn().unwrap()
}

#[tokio::test]
async fn timeout_cleans_up_and_reaps_only_the_spawned_child() {
    let (_root, path) = fixture();
    let mut child = Some(child(false));
    {
        let mut owned = StartingChild {
            child: &mut child,
            armed: true,
        };
        let error = wait_for_start(&path, owned.child(), Duration::from_millis(100))
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().kind,
            FailureKind::StartupTimedOut
        );
    }
    assert!(child.expect("reaped inline").try_wait().unwrap().is_some());
    // Waiting for an unpublished child must not open the lock it needs to start.
    assert!(!state_directory(&path).join("broker.lock").exists());
    assert!(!local::running(&path).unwrap());
}

#[tokio::test]
async fn cancellation_reaps_owned_child_while_control_lock_is_still_held() {
    let (_root, path) = fixture();
    let control = control_lock(&path).unwrap();
    let mut child = Some(child(false));
    let result = tokio::time::timeout(Duration::from_millis(100), async {
        let mut owned = StartingChild {
            child: &mut child,
            armed: true,
        };
        wait_for_start(&path, owned.child(), Duration::from_secs(30)).await
    })
    .await;
    assert!(result.is_err());
    assert!(child.expect("reaped inline").try_wait().unwrap().is_some());
    assert!(
        config::try_lock(&state_directory(&path).join("control.lock"))
            .unwrap()
            .is_none()
    );
    drop(control);
    assert!(
        config::try_lock(&state_directory(&path).join("control.lock"))
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn another_runtime_pid_cannot_satisfy_readiness_or_be_force_cleaned_up() {
    let (_root, path) = fixture();
    let _broker = config::lock(&state_directory(&path).join("broker.lock")).unwrap();
    let mut unrelated = Some(child(false));
    let mut unrelated = StartingChild {
        child: &mut unrelated,
        armed: true,
    };
    local::publish(
        &path,
        &Runtime {
            api_version: local::API_VERSION,
            pid: unrelated.child().id(),
            port: 9,
            instance_id: "other-instance".into(),
            control_token: "private-control-marker".into(),
        },
    )
    .unwrap();
    let before = fs::read(Runtime::path(&path)).unwrap();
    let mut spawned = Some(child(false));
    {
        let mut owned = StartingChild {
            child: &mut spawned,
            armed: true,
        };
        let error = wait_for_start(&path, owned.child(), Duration::from_millis(100))
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().kind,
            FailureKind::StartupTimedOut
        );
    }
    assert!(spawned.as_mut().unwrap().try_wait().unwrap().is_some());
    assert!(unrelated.child().try_wait().unwrap().is_none());
    assert_eq!(before, fs::read(Runtime::path(&path)).unwrap());
    assert!(local::running(&path).unwrap());
}

#[tokio::test]
async fn early_child_exit_is_reported_and_reaped() {
    let (_root, path) = fixture();
    let mut child = Some(child(true));
    {
        let mut owned = StartingChild {
            child: &mut child,
            armed: true,
        };
        let error = wait_for_start(&path, owned.child(), Duration::from_secs(5))
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().kind,
            FailureKind::ChildExited
        );
    }
    assert!(child.as_mut().unwrap().try_wait().unwrap().is_some());
}

#[tokio::test]
async fn stale_pid_without_os_lock_is_stopped_and_stale_metadata_is_cleaned_up() {
    let (_root, path) = fixture();
    local::publish(
        &path,
        &Runtime {
            api_version: local::API_VERSION,
            pid: std::process::id(),
            port: 9,
            instance_id: "stale".into(),
            control_token: "private-control-marker".into(),
        },
    )
    .unwrap();
    assert!(matches!(status(&path).await.unwrap(), Status::Stopped));
    assert!(!Runtime::path(&path).exists());
    assert_eq!(stop(&path).await.unwrap(), StopResult::AlreadyStopped);
}

#[test]
fn credential_change_guard_holds_both_locks_and_blocks_a_starting_broker() {
    let (_root, path) = fixture();
    let broker = config::lock(&state_directory(&path).join("broker.lock")).unwrap();
    let error = require_stopped(&path).err().unwrap();
    assert_eq!(
        error.downcast_ref::<Failure>().unwrap().kind,
        FailureKind::BrokerBusy
    );
    assert!(!Runtime::path(&path).exists());
    drop(broker);
    let guard = require_stopped(&path).unwrap();
    for name in ["control.lock", "broker.lock"] {
        assert!(
            config::try_lock(&state_directory(&path).join(name))
                .unwrap()
                .is_none()
        );
    }
    drop(guard);
    assert!(require_stopped(&path).is_ok());
}

#[tokio::test]
async fn wrong_service_version_pid_or_instance_cannot_be_reused_or_stopped() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    for (field, value) in [
        ("service", serde_json::json!("other-service")),
        ("apiVersion", serde_json::json!(999)),
        ("pid", serde_json::json!(1)),
        ("instanceId", serde_json::json!("other-instance")),
    ] {
        let (_root, path) = fixture();
        let _broker = config::lock(&state_directory(&path).join("broker.lock")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let runtime = Runtime {
            api_version: local::API_VERSION,
            pid: std::process::id(),
            port: listener.local_addr().unwrap().port(),
            instance_id: "expected-instance".into(),
            control_token: "private-control-marker".into(),
        };
        local::publish(&path, &runtime).unwrap();
        let mut response = serde_json::json!({"service":local::SERVICE,"apiVersion":local::API_VERSION,
            "pid":runtime.pid,"instanceId":runtime.instance_id,"centralServer":"Connected"});
        response[field] = value;
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let calls = shutdowns.clone();
        let app = Router::new()
            .route(
                "/health",
                get(move || {
                    let response = response.clone();
                    async { Json(response) }
                }),
            )
            .route(
                "/shutdown",
                post(move || {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let before = fs::read(Runtime::path(&path)).unwrap();
        for result in [
            status(&path).await.map(|_| ()),
            start(&path, Path::new("must-not-spawn")).await.map(|_| ()),
            stop(&path).await.map(|_| ()),
        ] {
            assert_eq!(
                result
                    .err()
                    .unwrap()
                    .downcast_ref::<Failure>()
                    .unwrap()
                    .kind,
                FailureKind::Unhealthy
            );
        }
        assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
        assert!(local::running(&path).unwrap());
        assert_eq!(before, fs::read(Runtime::path(&path)).unwrap());
        server.abort();
    }
}

#[tokio::test]
async fn a_held_lock_without_runtime_is_a_starting_broker_not_damaged_storage() {
    let (_root, path) = fixture();
    let _broker = config::lock(&state_directory(&path).join("broker.lock")).unwrap();
    for result in [
        status(&path).await.map(|_| ()),
        start(&path, Path::new("must-not-spawn")).await.map(|_| ()),
        stop(&path).await.map(|_| ()),
    ] {
        let error = result.err().unwrap();
        let fault = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(fault.kind, FailureKind::BrokerStarting);
        assert_eq!(fault.outcome, Outcome::NotApplied);
        assert_eq!(fault.file, Runtime::path(&path));
    }
    // Unreadable runtime metadata is still a storage fault, not a startup race.
    fs::write(Runtime::path(&path), b"{").unwrap();
    assert_eq!(
        status(&path)
            .await
            .err()
            .unwrap()
            .downcast_ref::<Failure>()
            .unwrap()
            .kind,
        FailureKind::Storage
    );
}

#[tokio::test]
async fn a_linked_lock_after_verified_health_is_retried_and_reported_as_storage_at_the_deadline() {
    use axum::{Json, Router, routing::get};
    let (root, path) = fixture();
    let mut child = Some(child(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let runtime = Runtime {
        api_version: local::API_VERSION,
        pid: child.as_ref().unwrap().id(),
        port: listener.local_addr().unwrap().port(),
        instance_id: "expected-instance".into(),
        control_token: "private-control-marker".into(),
    };
    local::publish(&path, &runtime).unwrap();
    let target = root.path().join("link-target");
    fs::create_dir(&target).unwrap();
    let sentinel = target.join("sentinel");
    fs::write(&sentinel, "preserved").unwrap();
    let lock = state_directory(&path).join("broker.lock");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&sentinel, &lock).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // A junction exercises reparse-point rejection without requiring symlink privilege.
        assert!(
            Command::new("cmd.exe")
                .args(["/c", "mklink", "/J"])
                .arg(&lock)
                .arg(&target)
                .creation_flags(0x08000000)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    struct Link(PathBuf);
    impl Drop for Link {
        fn drop(&mut self) {
            #[cfg(unix)]
            let _ = fs::remove_file(&self.0);
            #[cfg(windows)]
            let _ = fs::remove_dir(&self.0);
        }
    }
    let _link = Link(lock.clone());
    let response = serde_json::json!({"service":local::SERVICE,"apiVersion":local::API_VERSION,
        "pid":runtime.pid,"instanceId":runtime.instance_id,"centralServer":"Connected"});
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/health",
                get(move || {
                    let response = response.clone();
                    async { Json(response) }
                }),
            ),
        )
        .await
        .unwrap();
    });
    // A verified-healthy child is not killed by one failing lock inspection; the
    // error is retried until the deadline and then reported in place of a timeout.
    let started = Instant::now();
    {
        let mut owned = StartingChild {
            child: &mut child,
            armed: true,
        };
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_start(&path, owned.child(), Duration::from_millis(400)),
        )
        .await
        .expect("unsafe lock must be reported at the readiness deadline")
        .err()
        .unwrap();
        let fault = error.downcast_ref::<Failure>().unwrap();
        assert_eq!(fault.kind, FailureKind::Storage);
        assert_eq!(fault.outcome, Outcome::Unknown);
        assert_eq!(fault.file, lock);
    }
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert!(child.as_mut().unwrap().try_wait().unwrap().is_some());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "preserved");
    server.abort();
}

#[test]
fn a_child_still_exiting_after_the_bound_is_reaped_in_the_background() {
    // Not killed and no wait budget: the caller returns at once and the reaper owns it.
    let mut child = child(false);
    let id = child.id();
    assert!(matches!(child.try_wait(), Ok(None)));
    let started = std::time::Instant::now();
    assert!(!reaped_within(&mut child, Duration::ZERO));
    let reaper = hand_to_reaper(child).expect("handed to a reaper");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(!reaper.is_finished());
    // Ending the process lets the reaper's wait return, so nothing is left unreaped.
    kill_pid(id);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !reaper.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        reaper.is_finished(),
        "reaper did not finish after the child exited"
    );
    reaper.join().unwrap();
}

#[test]
fn a_child_that_exits_within_the_bound_is_reaped_inline() {
    let mut child = child(false);
    child.kill().unwrap();
    assert!(reaped_within(&mut child, Duration::from_secs(5)));
    assert!(child.try_wait().unwrap().is_some());
}

fn kill_pid(id: u32) {
    #[cfg(windows)]
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &id.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(unix)]
    let status = std::process::Command::new("kill")
        .args(["-9", &id.to_string()])
        .status();
    assert!(status.unwrap().success());
}
