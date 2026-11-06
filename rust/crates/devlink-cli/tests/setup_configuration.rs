use devlink_broker::{
    config::{self, Config},
    credentials, private_file, setup_config,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};

fn setup(path: &Path, format: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_devlink"))
        .arg("--config")
        .arg(path)
        .args(["setup", "--output", format])
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn result(output: &Output) -> Value {
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn write_config(path: &Path, value: &Value) {
    private_file::write(path, value, 16_384).unwrap();
}

fn assert_failure(path: &Path, code: &str, exit: i32, field: Option<&str>) -> Value {
    let before = fs::read(path).ok();
    let output = setup(path, "json");
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value = result(&output);
    assert_eq!(value["error"]["code"], code);
    assert_eq!(value["error"]["step"], "validate");
    assert_eq!(value["error"]["recovery"]["automatic"], false);
    assert!(value["error"]["location"]["file"].is_string());
    if let Some(field) = field {
        assert_eq!(value["error"]["location"]["field"], field);
    }
    assert_eq!(fs::read(path).ok(), before);
    value
}

#[test]
fn explicit_batch_server_initializes_a_new_profile_and_preserves_an_existing_binding() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("profile/config.json");
    let server = "wss://central.example.test/ws/broker";
    setup_config::preflight_server(&path, Some(server)).unwrap();
    assert!(!path.parent().unwrap().exists());
    let prepared = setup_config::prepare_with_server(&path, Some(server)).unwrap();
    assert!(prepared.created);
    assert_eq!(Config::load(&path).unwrap().server_url, server);
    let original = fs::read(&path).unwrap();
    let reused =
        setup_config::prepare_with_server(&path, Some("wss://central.example.test")).unwrap();
    assert!(!reused.created);
    assert_eq!(reused.config.server_url, server);
    let conflict =
        setup_config::preflight_server(&path, Some("wss://other.example.test")).unwrap_err();
    assert_eq!(conflict.kind, setup_config::FailureKind::ServerConflict);
    assert_eq!(fs::read(&path).unwrap(), original);
}

#[test]
fn fresh_setup_creates_private_defaults_and_preserves_them_on_second_run() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("프로필 with spaces/config.json");
    let first = setup(&path, "json");
    let value = result(&first);
    assert_eq!(first.status.code(), Some(4));
    assert_eq!(value["stages"][0]["outcome"], "applied");
    assert_eq!(value["error"]["step"], "authenticate");
    let config = Config::load(&path).unwrap();
    assert_eq!(config.server_url, Config::default().server_url);
    assert_eq!(config.local_port, 0);
    assert!(config.local_port_pending);
    credentials::verify_private_permissions(&path).unwrap();
    let before = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    let second = result(&setup(&path, "json"));
    assert_eq!(second["stages"][0]["outcome"], "not_applied");
    assert_eq!(second["stages"][0]["reused"], true);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
}

#[test]
fn valid_legacy_three_field_settings_keep_their_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("runtime.json");
    write_config(
        &path,
        &json!({"serverUrl":"ws://127.0.0.1:8080/ws/broker","agent":"custom-agent","autoReconnect":false}),
    );
    let before = fs::read(&path).unwrap();
    let prepared = setup_config::prepare(&path).unwrap();
    assert!(!prepared.created);
    assert!(!prepared.broker_running);
    assert!(!prepared.config.auto_reconnect);
    assert_eq!(prepared.config.local_port, 43127);
    assert_eq!(fs::read(&path).unwrap(), before);
}

#[test]
fn init_and_manually_written_settings_work_without_private_permissions() {
    for initialized in [true, false] {
        let root = tempfile::tempdir().unwrap();
        // Like a home profile directory: only trusted principals may write. %TEMP% may
        // grant other users Modify, which setup correctly rejects for config.json.
        credentials::set_private_permissions(root.path()).unwrap();
        let path = root.path().join("config.json");
        if initialized {
            assert!(config::init(&path).unwrap());
        } else {
            fs::write(&path, serde_json::to_vec(&Config::default()).unwrap()).unwrap();
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            assert!(
                Command::new("icacls.exe")
                    .arg(&path)
                    .args(["/grant", "*S-1-1-0:(R)"])
                    .stdin(Stdio::null())
                    .creation_flags(0x08000000)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let before = fs::read(&path).unwrap();
        let permissions = fs::metadata(&path).unwrap().permissions();
        let value = result(&setup(&path, "json"));
        assert_eq!(value["stages"][0]["status"], "succeeded");
        assert_eq!(value["stages"][0]["reused"], true);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::metadata(&path).unwrap().permissions(), permissions);
    }
}

#[test]
fn malformed_schema_and_values_are_preserved_with_safe_field_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    write_config(&path, &serde_json::to_value(Config::default()).unwrap());
    for bytes in [
        b"{broken-private-marker".as_slice(),
        br#"{"serverUrl":7,"agent":"codex","autoReconnect":true}"#,
        br#"{"serverUrl":"wss://example.test","agent":"codex","autoReconnect":true,"unknown":"private-marker"}"#,
        br#"{"serverUrl":"wss://example.test","agent":"codex","agent":"other","autoReconnect":true}"#,
    ] {
        fs::write(&path, bytes).unwrap();
        let value = assert_failure(&path, "SETUP_CONFIG_INVALID", 2, Some("$"));
        assert!(!value.to_string().contains("private-marker"));
    }
    for (field, invalid) in [
        ("serverUrl", json!("wss://user:private-marker@example.test")),
        ("serverUrl", json!("ws://remote.example.test")),
        ("serverUrl", json!("wss://example.test/unsupported")),
        ("agent", json!("bad private-marker")),
        ("localPort", json!(80)),
        ("authTimeoutMs", json!(1)),
        ("reconnectInitialMs", json!(1)),
    ] {
        let mut config = serde_json::to_value(Config::default()).unwrap();
        config[field] = invalid;
        write_config(&path, &config);
        let expected = if field == "reconnectInitialMs" {
            "reconnectInitialMs/reconnectMaxMs"
        } else {
            field
        };
        let value = assert_failure(&path, "SETUP_CONFIG_INVALID", 2, Some(expected));
        assert_eq!(
            value["error"]["recovery"]["action"],
            "repair_config_then_retry"
        );
        assert!(!value.to_string().contains("private-marker"));
    }
}

#[test]
fn oversized_directory_and_reserved_targets_are_not_initialized() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    write_config(&path, &serde_json::to_value(Config::default()).unwrap());
    fs::write(&path, vec![b'x'; 16_385]).unwrap();
    assert_failure(&path, "SETUP_UNSAFE_FILE", 5, None);
    let directory = root.path().join("directory");
    fs::create_dir(&directory).unwrap();
    assert_failure(&directory, "SETUP_UNSAFE_FILE", 5, None);
    let reserved = root.path().join(".devlink-runtime/user-session.json");
    assert_failure(&reserved, "SETUP_CONFIG_PATH_INVALID", 2, None);
    assert!(!reserved.exists());
}

#[test]
fn control_lock_contention_fails_before_creating_settings() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    let control = config::state_directory(&path).join("control.lock");
    let _guard = config::lock(&control).unwrap();
    let value = assert_failure(&path, "SETUP_CONFIG_BUSY", 6, None);
    assert_eq!(
        value["error"]["location"]["file"],
        control.to_str().unwrap()
    );
    assert!(!path.exists());
}

#[test]
fn held_broker_lock_blocks_missing_config_but_allows_read_only_validation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    let _broker = config::lock(&config::state_directory(&path).join("broker.lock")).unwrap();
    assert_failure(&path, "SETUP_BROKER_BUSY", 6, None);
    write_config(&path, &serde_json::to_value(Config::default()).unwrap());
    let before = fs::read(&path).unwrap();
    let prepared = setup_config::prepare(&path).unwrap();
    assert!(prepared.broker_running);
    assert!(!prepared.created);
    assert_eq!(fs::read(&path).unwrap(), before);
    let output = result(&setup(&path, "json"));
    assert_eq!(output["stages"][0]["status"], "succeeded");
    assert_eq!(output["stages"][0]["reused"], true);
}

#[test]
fn existing_credentials_are_preserved_and_never_printed_or_rotated() {
    let root = tempfile::tempdir().unwrap();
    credentials::set_private_permissions(root.path()).unwrap();
    let path = root.path().join("config.json");
    let creds = credentials::publish(&path, "http://127.0.0.1:43127").unwrap();
    let credential_path = credentials::path(&path);
    let before = fs::read(&credential_path).unwrap();
    let output = setup(&path, "json");
    assert_eq!(output.status.code(), Some(4));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(&creds.local_token));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(&creds.local_token));
    assert_eq!(fs::read(&credential_path).unwrap(), before);
    assert_eq!(credentials::load(&path).unwrap(), creds);
}

#[test]
fn invalid_private_snapshot_prevents_config_creation_without_repairing_the_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    let credential_path = credentials::path(&path);
    private_file::write(
        &credential_path,
        &json!({"localToken":"private-marker"}),
        16_384,
    )
    .unwrap();
    let before = fs::read(&credential_path).unwrap();
    let value = assert_failure(&path, "SETUP_UNSAFE_FILE", 5, None);
    assert_eq!(
        value["error"]["location"]["file"],
        credential_path.to_str().unwrap()
    );
    assert!(!value.to_string().contains("private-marker"));
    assert_eq!(fs::read(&credential_path).unwrap(), before);
    assert!(!path.exists());
}

#[test]
fn human_errors_include_location_and_recovery_without_the_invalid_value() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    let mut config = serde_json::to_value(Config::default()).unwrap();
    config["serverUrl"] = json!("wss://user:private-marker@example.test");
    write_config(&path, &config);
    let output = setup(&path, "human");
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Location:"));
    assert!(text.contains("serverUrl"));
    assert!(text.contains("repair_config_then_retry"));
    assert!(!text.contains("private-marker"));
}

#[test]
fn concurrent_setup_processes_publish_defaults_at_most_once() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("profile/config.json");
    let spawn = || {
        Command::new(env!("CARGO_BIN_EXE_devlink"))
            .arg("--config")
            .arg(&path)
            .args(["setup", "--output", "json"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = spawn();
    let second = spawn();
    let outputs = [
        first.wait_with_output().unwrap(),
        second.wait_with_output().unwrap(),
    ];
    let mut created = 0;
    for output in outputs {
        assert!(
            matches!(output.status.code(), Some(4 | 6)),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        let value = result(&output);
        if value["stages"][0]["outcome"] == "applied" {
            created += 1;
        }
    }
    assert_eq!(created, 1);
    Config::load(&path).unwrap();
    credentials::verify_private_permissions(&path).unwrap();
}

#[tokio::test]
async fn setup_excludes_the_same_profile_until_cancellation_or_process_exit() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("profile");
    let path = profile.join("config.json");
    let alias = profile.join("other-config.json");
    let independent = root.path().join("other-profile/config.json");
    let lock = config::state_directory(&path).join("setup.lock");

    for forced_exit in [false, true] {
        let mut first = tokio::process::Command::new(env!("CARGO_BIN_EXE_devlink"))
            .arg("--config")
            .arg(&path)
            .args([
                "setup",
                "--output",
                "jsonl",
                "--email",
                "tester@example.invalid",
                "--password-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(first.stdout.take().unwrap()).lines();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("setup exited early");
                let event: Value = serde_json::from_str(&line).unwrap();
                if event["step"] == "authenticate" && event["stepStatus"] == "running" {
                    break;
                }
            }
        })
        .await
        .expect("first setup did not reach authentication");

        let busy = assert_failure(&alias, "SETUP_CONFIG_BUSY", 6, None);
        assert_eq!(busy["error"]["location"]["file"], lock.to_str().unwrap());
        assert_eq!(busy["stages"][0]["outcome"], "not_applied");
        assert_eq!(busy["stages"][1]["status"], "skipped");
        assert!(!alias.exists());
        assert_eq!(setup(&independent, "json").status.code(), Some(4));

        let status = Command::new(env!("CARGO_BIN_EXE_devlink"))
            .arg("--config")
            .arg(&path)
            .args(["setup", "status", "--output", "json"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert_eq!(result(&status)["data"]["connection"]["broker"], "stopped");

        if forced_exit {
            first.kill().await.unwrap();
        } else {
            drop(first.stdin.take());
            let exit = tokio::time::timeout(Duration::from_secs(30), first.wait())
                .await
                .expect("setup did not exit after EOF")
                .unwrap();
            assert_eq!(exit.code(), Some(130));
        }
        assert!(lock.is_file());
        assert_eq!(setup(&path, "json").status.code(), Some(4));
    }
}

#[cfg(windows)]
#[test]
fn a_junction_ancestor_is_rejected_before_creating_a_profile() {
    use std::os::windows::process::CommandExt;
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("destination");
    fs::create_dir(&target).unwrap();
    let sentinel = target.join("sentinel.txt");
    fs::write(&sentinel, b"preserved").unwrap();
    let link = root.path().join("linked");
    assert!(
        Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .stdin(Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .unwrap()
            .status
            .success()
    );
    struct Junction(std::path::PathBuf);
    impl Drop for Junction {
        fn drop(&mut self) {
            let _ = fs::remove_dir(&self.0);
        }
    }
    let _cleanup = Junction(link.clone());
    let path = link.join("profile/config.json");
    assert_failure(&path, "SETUP_UNSAFE_FILE", 5, None);
    assert!(!target.join("profile").exists());
    assert_eq!(fs::read(sentinel).unwrap(), b"preserved");
}

#[cfg(windows)]
#[test]
fn public_config_permissions_are_allowed_but_private_snapshot_permissions_are_rejected() {
    use std::os::windows::process::CommandExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    write_config(&path, &serde_json::to_value(Config::default()).unwrap());
    assert!(
        Command::new("icacls.exe")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(R)"])
            .stdin(Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .unwrap()
            .status
            .success()
    );
    let before = fs::read(&path).unwrap();
    let value = result(&setup(&path, "json"));
    assert_eq!(value["stages"][0]["status"], "succeeded");
    assert_eq!(fs::read(&path).unwrap(), before);

    credentials::publish(&path, "http://127.0.0.1:43127").unwrap();
    let private = credentials::path(&path);
    assert!(
        Command::new("icacls.exe")
            .arg(&private)
            .args(["/grant", "*S-1-1-0:(R)"])
            .stdin(Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .unwrap()
            .status
            .success()
    );
    let value = assert_failure(&path, "SETUP_UNSAFE_FILE", 5, None);
    assert_eq!(
        value["error"]["location"]["file"],
        private.to_str().unwrap()
    );
}

// serverUrl decides where setup sends the password, so other principals must not write it.
#[cfg(windows)]
#[test]
fn config_writable_by_other_principals_is_rejected_without_changing_it() {
    use std::os::windows::process::CommandExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config.json");
    write_config(&path, &serde_json::to_value(Config::default()).unwrap());
    assert!(
        Command::new("icacls.exe")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(M)"])
            .stdin(Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .unwrap()
            .status
            .success()
    );
    let value = assert_failure(&path, "SETUP_UNSAFE_FILE", 5, None);
    assert_eq!(value["error"]["location"]["file"], path.to_str().unwrap());
}

#[cfg(unix)]
#[test]
fn config_writable_by_group_or_others_is_rejected_without_changing_it() {
    use std::os::unix::fs::PermissionsExt;
    for mode in [0o664, 0o646] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.json");
        write_config(&path, &serde_json::to_value(Config::default()).unwrap());
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        let value = assert_failure(&path, "SETUP_UNSAFE_FILE", 5, None);
        assert_eq!(value["error"]["location"]["file"], path.to_str().unwrap());
    }
}

#[cfg(unix)]
#[test]
fn symbolic_link_target_is_rejected_without_touching_its_destination() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target.json");
    write_config(&target, &serde_json::to_value(Config::default()).unwrap());
    let path = root.path().join("config.json");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let before = fs::read(&target).unwrap();
    assert!(!config::init(&path).unwrap());
    assert_failure(&path, "SETUP_UNSAFE_FILE", 5, None);
    assert_eq!(fs::read(&target).unwrap(), before);
}
