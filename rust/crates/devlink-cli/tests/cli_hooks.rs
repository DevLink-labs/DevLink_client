//! `devlink hooks` against isolated Host config directories (CLAUDE_CONFIG_DIR /
//! CODEX_HOME). The user's real Host settings are never read or written here. As with
//! the setup MCP tests, the provider's lock and backup state for these temporary targets
//! is kept under the real `~/.devlink-mcp-config`, which cannot be redirected on Windows.
use devlink_broker::{config, credentials};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    _dir: tempfile::TempDir,
    config: PathBuf,
    claude: PathBuf,
    codex: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        credentials::set_private_permissions(dir.path()).unwrap();
        let config = dir.path().join("profile").join("config.json");
        config::init(&config).unwrap();
        let claude = dir.path().join("claude");
        let codex = dir.path().join("codex");
        for path in [&claude, &codex] {
            fs::create_dir(path).unwrap();
            credentials::set_private_permissions(path).unwrap();
        }
        Self {
            _dir: dir,
            config,
            claude,
            codex,
        }
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_devlink"))
            .arg("--config")
            .arg(&self.config)
            .arg("hooks")
            .args(args)
            .env("CLAUDE_CONFIG_DIR", &self.claude)
            .env("CODEX_HOME", &self.codex)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn target(&self, host: &str) -> PathBuf {
        match host {
            "codex" => self.codex.join("hooks.json"),
            _ => self.claude.join("settings.json"),
        }
    }
}
fn stdout(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "stdout: {} stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn install_plans_first_writes_with_yes_and_then_reuses() {
    let f = Fixture::new();
    for host in ["codex", "claude-code"] {
        let target = f.target(host);
        let status = stdout(&f.run(&["status", "--host", host]));
        assert_eq!(status["status"], "missing");

        let planned = f.run(&["install", "--host", host]);
        assert!(planned.status.success());
        let planned = stdout(&planned);
        assert_eq!(planned["applied"], false);
        assert_eq!(planned["plan"]["change"], "create");
        assert!(!target.exists(), "a plan never writes");

        let applied = f.run(&["install", "--host", host, "--yes"]);
        assert!(applied.status.success());
        let applied = stdout(&applied);
        assert_eq!(applied["result"]["status"], "applied");
        let next = applied["nextActions"].as_array().unwrap();
        assert_eq!(next.contains(&json!("trust_codex_hooks")), host == "codex");
        assert!(next.contains(&json!("restart_host")));

        let value = read(&target);
        for (event, flag) in [
            ("SessionStart", "session-start"),
            ("UserPromptSubmit", "user-prompt-submit"),
        ] {
            let command = value["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap();
            assert!(command.ends_with(&format!("updates-hook --host {host} --event {flag}")));
            assert!(command.contains("--config "));
            assert!(!command.contains('\\') && !command.contains('"'));
        }
        assert_eq!(
            stdout(&f.run(&["status", "--host", host]))["status"],
            "installed"
        );
        let again = stdout(&f.run(&["install", "--host", host, "--yes"]));
        assert_eq!(again["result"]["status"], "unchanged");
        assert_eq!(again["nextActions"], json!([]));
    }
}

#[test]
fn another_devlink_entry_is_kept_until_replace_and_uninstall_keeps_other_hooks() {
    let f = Fixture::new();
    let target = f.target("claude-code");
    let old = "C:/old/devlink.exe --config C:/old/config.json updates-hook --host claude-code --event user-prompt-submit";
    let seeded = serde_json::to_vec_pretty(&json!({
        "theme": "dark",
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": "node wrapper.cjs claude-code"}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": old, "timeout": 10}]}]
        }
    }))
    .unwrap();
    fs::write(&target, &seeded).unwrap();
    assert_eq!(
        stdout(&f.run(&["status", "--host", "claude-code"]))["status"],
        "partial"
    );

    let refused = f.run(&["install", "--host", "claude-code", "--yes"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--replace"));
    assert_eq!(
        stdout(&refused)["plan"]["existing"]["UserPromptSubmit"],
        json!([old])
    );
    assert_eq!(fs::read(&target).unwrap(), seeded);

    let replaced = f.run(&["install", "--host", "claude-code", "--replace", "--yes"]);
    assert!(replaced.status.success());
    assert!(stdout(&replaced)["result"]["backupPath"].is_string());
    let value = read(&target);
    assert_eq!(value["theme"], "dark");
    assert_eq!(
        value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "node wrapper.cjs claude-code"
    );
    assert!(!fs::read_to_string(&target).unwrap().contains("C:/old/"));

    let planned = stdout(&f.run(&["uninstall", "--host", "claude-code"]));
    assert_eq!(planned["plan"]["change"], "remove");
    assert!(
        fs::read_to_string(&target)
            .unwrap()
            .contains("updates-hook")
    );
    assert!(
        f.run(&["uninstall", "--host", "claude-code", "--yes"])
            .status
            .success()
    );
    let value = read(&target);
    assert_eq!(value["theme"], "dark");
    assert_eq!(
        value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
        "node wrapper.cjs claude-code"
    );
    assert!(value["hooks"].get("UserPromptSubmit").is_none());
    assert!(
        !fs::read_to_string(&target)
            .unwrap()
            .contains("updates-hook")
    );
}

#[test]
fn install_requires_an_existing_profile() {
    let f = Fixture::new();
    fs::remove_file(&f.config).unwrap();
    let output = f.run(&["install", "--host", "codex", "--yes"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("devlink setup"));
    assert!(!f.target("codex").exists());
}
