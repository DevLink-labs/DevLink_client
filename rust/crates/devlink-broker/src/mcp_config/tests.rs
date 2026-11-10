use super::*;
use crate::credentials;
use serde_json::json;
use std::fs;

#[test]
fn guide_classifies_json_without_exposing_existing_values_or_writing_files() {
    let f = Fixture::new();
    let input = storage::normalize(ToolType::ClaudeCode, f.input(ToolType::ClaudeCode)).unwrap();
    for (bytes, state, kind, scope) in [
        (None, "file_missing", "create_file", "whole_file"),
        (
            Some(br#"{"custom":"SECRET"}"#.as_slice()),
            "entry_missing",
            "add_container",
            "container",
        ),
        (
            Some(br#"{"mcpServers":{"other":{"env":{"token":"SECRET"}}}}"#.as_slice()),
            "entry_missing",
            "add_entry",
            "entry",
        ),
        (
            Some(br#"{"mcpServers":{"devlink":{"command":"OLD_SECRET"}}}"#.as_slice()),
            "different",
            "replace_entry",
            "entry",
        ),
    ] {
        let (actual, edit, items, _) = guide::classify(&input, bytes).unwrap();
        assert_eq!((actual, edit, items[0].scope), (state, kind, scope));
        assert!(!items[0].snippet.contains("SECRET"));
        let applied = match scope {
            "whole_file" => items[0].snippet.clone(),
            "container" => format!(r#"{{"custom":"SECRET",{}}}"#, items[0].snippet),
            _ => format!(
                r#"{{"mcpServers":{{"other":{{"command":"keep"}},{}}}}}"#,
                items[0].snippet
            ),
        };
        assert_eq!(
            format::configured_entry(input.tool, applied.as_bytes()).unwrap(),
            Some(format::entry(&input))
        );
    }
    for bytes in [
        br#"{"mcpServers":[]}"#.as_slice(),
        br#"{"mcpServers":{},"mcpServers":{}}"#,
        b"{ broken",
    ] {
        let (state, _, copies, _) = guide::classify(&input, Some(bytes)).unwrap();
        assert_eq!(state, "invalid");
        assert!(copies.is_empty());
    }
    let (_, applied) = format::merge(input.tool, None, &format::entry(&input)).unwrap();
    assert_eq!(
        guide::classify(&input, Some(&applied)).unwrap().0,
        "matches"
    );
    assert!(!f.input(input.tool).target_path.exists());
}

#[test]
fn guide_rejects_copied_credentials_even_when_devlink_entry_matches() {
    let f = Fixture::new();
    let input = storage::normalize(ToolType::ClaudeCode, f.input(ToolType::ClaudeCode)).unwrap();
    let (_, applied) = format::merge(input.tool, None, &format::entry(&input)).unwrap();
    let mut root: serde_json::Value = serde_json::from_slice(&applied).unwrap();
    root["localToken"] = json!("SECRET");
    root["brokerUrl"] = json!("http://127.0.0.1:43127");
    let leaked = serde_json::to_vec(&root).unwrap();
    assert_eq!(
        format::configured_entry(input.tool, &leaked).unwrap(),
        Some(format::entry(&input))
    );
    assert_eq!(
        guide::classify(&input, Some(&leaked)).unwrap_err().code,
        "SETUP_UNSAFE_FILE"
    );
    assert_eq!(
        format::merge(input.tool, Some(&leaked), &format::entry(&input))
            .unwrap_err()
            .code,
        "SETUP_UNSAFE_FILE"
    );
}

#[test]
fn guide_codex_table_and_inline_snippets_preserve_other_settings() {
    let f = Fixture::new();
    let input = storage::normalize(ToolType::Codex, f.input(ToolType::Codex)).unwrap();
    let (_, _, items, _) = guide::classify(
        &input,
        Some(b"model='keep'\n[mcp_servers.other]\ncommand='SECRET'\n"),
    )
    .unwrap();
    assert!(!items[0].snippet.contains("SECRET"));
    assert!(items[0].snippet.contains("[mcp_servers.devlink]"));
    let applied = format!(
        "model='keep'\n[mcp_servers.other]\ncommand='SECRET'\n{}",
        items[0].snippet
    );
    assert_eq!(
        format::configured_entry(input.tool, applied.as_bytes()).unwrap(),
        Some(format::entry(&input))
    );
    let (_, _, copies, reason) =
        guide::classify(&input, Some(b"mcp_servers={other={command='SECRET'}}\n")).unwrap();
    assert_eq!(reason, "edit_inline_container");
    let inline = format!(
        "mcp_servers={{other={{command='SECRET'}},{}}}\n",
        copies[0].snippet
    );
    assert_eq!(
        format::configured_entry(input.tool, inline.as_bytes()).unwrap(),
        Some(format::entry(&input))
    );
    for bytes in [
        b"[mcp_servers.devlink]\ncommand='old'\nstartup_timeout_sec=20\n".as_slice(),
        b"[mcp_servers]\ndevlink={command='old'}\n".as_slice(),
        b"[mcp_servers.devlink]\ncommand='old'\n[mcp_servers.devlink.env]\nsecret='SECRET'\n",
    ] {
        let (state, kind, copies, _) = guide::classify(&input, Some(bytes)).unwrap();
        assert_eq!((state, kind), ("different", "replace_entry"));
        assert!(!copies[0].snippet.contains("SECRET"));
    }
    let (state, _, copies, _) = guide::classify(&input, Some(b"[mcp_servers\n")).unwrap();
    assert_eq!(state, "invalid");
    assert!(copies.is_empty());
}
#[derive(Clone, Copy)]
pub(super) enum Fault {
    BeforeCommitFailure,
    ConcurrentBeforeCommit,
    AfterCommitFailure,
    ConcurrentAfterCommit,
    RestoreFailure,
}
pub(super) fn before_commit(fault: Option<Fault>, input: &Normalized) -> Result<()> {
    match fault {
        Some(Fault::BeforeCommitFailure) => Err(ConfigError::new("SETUP_MCP_CONFIG_WRITE_FAILED")),
        Some(Fault::ConcurrentBeforeCommit) => {
            fs::write(&input.target, outside(input.tool)).unwrap();
            Ok(())
        }
        _ => Ok(()),
    }
}
pub(super) fn after_commit(fault: Option<Fault>, input: &Normalized) {
    if matches!(fault, Some(Fault::ConcurrentAfterCommit)) {
        fs::write(&input.target, outside(input.tool)).unwrap();
    }
}
pub(super) fn fail_inspection(fault: Option<Fault>) -> bool {
    matches!(
        fault,
        Some(Fault::AfterCommitFailure | Fault::RestoreFailure)
    )
}
fn outside(tool: ToolType) -> &'static [u8] {
    match tool {
        ToolType::Codex => b"# external edit\nmodel='outside'\n",
        ToolType::ClaudeCode => b"{\"external\":true}",
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    broker: PathBuf,
    executable: Executable,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        credentials::set_private_permissions(dir.path()).unwrap();
        let root = dir.path().join("project");
        fs::create_dir(&root).unwrap();
        credentials::verify_directory_permissions(&root).expect("fixture project directory ACL");
        let profile = dir.path().join("profile");
        fs::create_dir(&profile).unwrap();
        let config = profile.join("config.json");
        crate::config::init(&config).unwrap();
        credentials::publish(&config, "http://127.0.0.1:43127").unwrap();
        let command = dir
            .path()
            .join(if cfg!(windows) { "node.exe" } else { "node" });
        let script = dir.path().join("index.js");
        fs::write(&command, b"fixture executable, never run").unwrap();
        fs::write(&script, b"// fixture script, never run").unwrap();
        Self {
            _dir: dir,
            root,
            broker: credentials::path(&config),
            executable: Executable {
                command,
                args: vec![
                    script.to_str().unwrap().into(),
                    "--transport".into(),
                    "stdio".into(),
                ],
            },
        }
    }
    fn input(&self, tool: ToolType) -> PlanInput {
        PlanInput {
            tool_type: tool,
            project_path: self.root.clone(),
            target_path: self.root.join(match tool {
                ToolType::Codex => ".codex/config.toml",
                ToolType::ClaudeCode => ".mcp.json",
            }),
            broker_file: self.broker.clone(),
            executable: self.executable.clone(),
        }
    }
    fn inspect(&self, tool: ToolType) -> InspectionInput {
        let p = self.input(tool);
        InspectionInput {
            tool_type: p.tool_type,
            target_path: p.target_path,
            broker_file: p.broker_file,
            executable: p.executable,
        }
    }
    fn manager(tool: ToolType) -> McpConfig {
        McpConfig::new(VerifiedHost::new(tool, &minimum(tool)).unwrap())
    }
    fn seed(&self, tool: ToolType, bytes: &[u8]) {
        let p = self.input(tool);
        let parent = p.target_path.parent().unwrap();
        if !parent.exists() {
            fs::create_dir(parent).unwrap();
            credentials::set_private_permissions(parent).unwrap();
        }
        storage::write(&p.target_path, bytes, !p.target_path.exists()).unwrap();
    }
}
fn original(tool: ToolType) -> Vec<u8> {
    match tool {
    ToolType::Codex=>b"# preserve comment\nmodel='keep'\n[mcp_servers.other]\ncommand='other'\n[mcp_servers.other.env]\nAPI_KEY='fixture-secret'\n".to_vec(),
    ToolType::ClaudeCode=>serde_json::to_vec(&json!({"editor":{"theme":"dark"},"mcpServers":{"other":{"command":"other","env":{"API_KEY":"fixture-secret"}}}})).unwrap(),
}
}
#[test]
fn plan_is_read_only_create_and_repeated_apply_are_idempotent() {
    for tool in [ToolType::Codex, ToolType::ClaudeCode] {
        let f = Fixture::new();
        let mut m = Fixture::manager(tool);
        let input = f.input(tool);
        let plan = m.plan(input.clone()).unwrap();
        assert_eq!(plan.change, Change::Create);
        assert_eq!(plan.expected_digest, None);
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 0);
        let result = m.apply(&plan.plan_id, None).unwrap();
        assert_eq!(result.status, ApplyStatus::Applied);
        assert!(result.backup_path.is_none());
        assert_eq!(
            m.inspect(f.inspect(tool)).unwrap().status,
            InspectionStatus::Applied
        );
        let bytes = fs::read(&result.target_path).unwrap();
        let again = m.apply(&plan.plan_id, None).unwrap();
        assert_eq!(again.status, ApplyStatus::Unchanged);
        assert_eq!(fs::read(&result.target_path).unwrap(), bytes);
        let next = m.plan(input).unwrap();
        assert_eq!(next.change, Change::Unchanged);
        assert!(next.diff.is_none());
        credentials::verify_private_permissions(&result.target_path).unwrap();
    }
}
#[test]
fn merge_preserves_other_settings_secrets_and_private_exact_backup() {
    for tool in [ToolType::Codex, ToolType::ClaudeCode] {
        let f = Fixture::new();
        let mut m = Fixture::manager(tool);
        let before = original(tool);
        f.seed(tool, &before);
        let plan = m.plan(f.input(tool)).unwrap();
        assert_eq!(plan.change, Change::Merge);
        assert!(
            !serde_json::to_string(&plan)
                .unwrap()
                .contains("fixture-secret")
        );
        let applied = m
            .apply(&plan.plan_id, plan.expected_digest.as_deref())
            .unwrap();
        let backup = applied.backup_path.unwrap();
        assert_eq!(fs::read(&backup).unwrap(), before);
        credentials::verify_private_permissions(&backup).unwrap();
        let after = fs::read_to_string(applied.target_path).unwrap();
        assert!(after.contains("fixture-secret"));
        assert!(after.contains("other"));
        if tool == ToolType::Codex {
            assert!(after.contains("# preserve comment"));
            assert!(after.contains("model='keep'"));
        } else {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&after).unwrap()["editor"]["theme"],
                "dark"
            );
        }
    }
}
#[test]
fn conflicts_and_external_edits_never_overwrite_the_existing_file() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let mut m = Fixture::manager(tool);
    let before = original(tool);
    f.seed(tool, &before);
    let plan = m.plan(f.input(tool)).unwrap();
    fs::write(&f.input(tool).target_path, outside(tool)).unwrap();
    assert_eq!(
        m.apply(&plan.plan_id, plan.expected_digest.as_deref())
            .unwrap_err()
            .code,
        "SETUP_MCP_CONFIG_CONFLICT"
    );
    assert_eq!(fs::read(f.input(tool).target_path).unwrap(), outside(tool));
    f.seed(tool,b"{\"mcpServers\":{\"devlink\":{\"command\":\"unrelated\",\"env\":{\"SECRET\":\"fixture-secret\"}}}}");
    let conflict = m.plan(f.input(tool)).unwrap();
    assert_eq!(conflict.change, Change::Conflict);
    assert!(
        !serde_json::to_string(&conflict)
            .unwrap()
            .contains("fixture-secret")
    );
    assert_eq!(
        m.apply(&conflict.plan_id, conflict.expected_digest.as_deref())
            .unwrap_err()
            .code,
        "SETUP_MCP_CONFIG_CONFLICT"
    );
}

#[test]
fn replacement_preserves_other_settings_and_backs_up_the_exact_previous_file() {
    for tool in [ToolType::Codex, ToolType::ClaudeCode] {
        let f = Fixture::new();
        let other = Fixture::new();
        let mut m = Fixture::manager(tool);
        f.seed(tool, &original(tool));
        let first = m.plan(f.input(tool)).unwrap();
        m.apply(&first.plan_id, first.expected_digest.as_deref())
            .unwrap();
        let before = fs::read(f.input(tool).target_path).unwrap();
        let mut next = f.input(tool);
        next.broker_file = other.broker.clone();
        assert_eq!(m.plan(next.clone()).unwrap().change, Change::Conflict);
        let plan = m.plan_replacing_devlink(next.clone()).unwrap();
        assert_eq!(plan.change, Change::Merge);
        assert!(
            !serde_json::to_string(&plan)
                .unwrap()
                .contains("fixture-secret")
        );
        let applied = m
            .apply(&plan.plan_id, plan.expected_digest.as_deref())
            .unwrap();
        let backup = applied.backup_path.unwrap();
        assert_eq!(fs::read(&backup).unwrap(), before);
        credentials::verify_private_permissions(&backup).unwrap();
        let after = fs::read_to_string(f.input(tool).target_path).unwrap();
        assert!(after.contains("fixture-secret"));
        if tool == ToolType::Codex {
            assert!(after.contains("# preserve comment"));
            assert!(after.contains("model='keep'"));
        } else {
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&after).unwrap()["editor"]["theme"],
                "dark"
            );
        }
        assert_eq!(
            m.inspect(InspectionInput {
                broker_file: other.broker,
                ..f.inspect(tool)
            })
            .unwrap()
            .status,
            InspectionStatus::Applied
        );
        assert_eq!(
            m.plan_replacing_devlink(next).unwrap().change,
            Change::Unchanged
        );
    }
}

#[test]
fn replacement_rejects_concurrent_edits_and_malformed_settings() {
    for tool in [ToolType::Codex, ToolType::ClaudeCode] {
        let f = Fixture::new();
        let mut m = Fixture::manager(tool);
        f.seed(tool, &original(tool));
        let plan = m.plan_replacing_devlink(f.input(tool)).unwrap();
        fs::write(&f.input(tool).target_path, outside(tool)).unwrap();
        assert_eq!(
            m.apply(&plan.plan_id, plan.expected_digest.as_deref())
                .unwrap_err()
                .code,
            "SETUP_MCP_CONFIG_CONFLICT"
        );
        assert_eq!(fs::read(f.input(tool).target_path).unwrap(), outside(tool));
        f.seed(
            tool,
            match tool {
                ToolType::Codex => b"[broken",
                ToolType::ClaudeCode => b"{broken",
            },
        );
        assert_eq!(
            m.plan_replacing_devlink(f.input(tool)).unwrap_err().code,
            "SETUP_MCP_CONFIG_CONFLICT"
        );
    }
}
#[test]
fn write_and_post_inspection_failures_restore_only_the_owned_version() {
    for fault in [
        Fault::BeforeCommitFailure,
        Fault::ConcurrentBeforeCommit,
        Fault::AfterCommitFailure,
        Fault::ConcurrentAfterCommit,
        Fault::RestoreFailure,
    ] {
        let f = Fixture::new();
        let tool = ToolType::ClaudeCode;
        let before = original(tool);
        f.seed(tool, &before);
        let mut m = Fixture::manager(tool);
        let plan = m.plan(f.input(tool)).unwrap();
        m.fault = Some(fault);
        let error = m
            .apply(&plan.plan_id, plan.expected_digest.as_deref())
            .unwrap_err();
        let actual = fs::read(f.input(tool).target_path).unwrap();
        match fault {
            Fault::BeforeCommitFailure => {
                assert_eq!(actual, before);
                assert_eq!(error.outcome, "not_applied");
            }
            Fault::ConcurrentBeforeCommit => {
                assert_eq!(actual, outside(tool));
                assert_eq!(error.code, "SETUP_MCP_CONFIG_CONFLICT");
            }
            Fault::AfterCommitFailure => {
                assert_eq!(actual, before);
                assert_eq!(error.rollback, "restored");
            }
            Fault::ConcurrentAfterCommit => {
                assert_eq!(actual, outside(tool));
                assert_eq!(error.rollback, "conflict");
                assert_eq!(error.outcome, "unknown");
            }
            Fault::RestoreFailure => {
                assert_ne!(actual, before);
                assert_eq!(error.rollback, "failed");
                assert!(error.backup_path.unwrap().exists());
            }
        }
    }
}

fn minimum(tool: ToolType) -> String {
    let [major, minor, patch] = match tool {
        ToolType::Codex => CODEX_MIN_VERSION,
        ToolType::ClaudeCode => CLAUDE_MIN_VERSION,
    };
    format!("{major}.{minor}.{patch}")
}

#[test]
fn the_minimum_host_version_and_later_releases_are_verified() {
    for (tool, verified, older) in [
        (
            ToolType::Codex,
            [
                "0.166.0",
                "0.166.0+build.7",
                // A hyphen inside build metadata is not a pre-release marker.
                "0.166.0+build-1",
                "0.166.1",
                "0.167.0",
                "0.167.0-alpha.1",
                "1.0.0",
                "0.1000.0",
            ],
            [
                "0.165.9",
                "0.166.0-beta.1",
                "0.166.0-alpha.6+build",
                "0.99.999",
            ],
        ),
        (
            ToolType::ClaudeCode,
            [
                "2.1.317",
                "2.1.317+build.7",
                "2.1.317+exp-sha.5",
                "2.1.319",
                "2.2.0",
                "2.2.0-rc.1",
                "3.0.0",
                "2.1.1000",
            ],
            ["2.1.316", "2.1.317-beta.1", "2.0.999", "1.9.999"],
        ),
    ] {
        assert!(VerifiedHost::new(tool, &minimum(tool)).is_ok());
        for version in verified {
            assert!(VerifiedHost::new(tool, version).is_ok(), "{version}");
        }
        for version in older {
            assert!(VerifiedHost::new(tool, version).is_err(), "{version}");
        }
        // Anything that is not `major.minor.patch` is never compared as a version.
        for version in [
            "",
            "unverified",
            "9",
            "9.9",
            "9.9.9.9",
            "9..9",
            ".9.9",
            "9.9.",
            "9.9.x",
            "+9.9.9",
            "-9.9.9",
            "v9.9.9",
            " 9.9.9",
            "9.9.99999999999999999999999",
        ] {
            assert!(VerifiedHost::new(tool, version).is_err(), "{version}");
        }
    }
}

#[test]
fn unsafe_inputs_duplicate_json_and_unrelated_targets_do_not_write() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let mut m = Fixture::manager(tool);
    assert_eq!(
        VerifiedHost::new(tool, "unverified").err().unwrap().code,
        "SETUP_TOOL_UNAVAILABLE"
    );
    let mut outside = f.input(tool);
    outside.target_path = f.root.parent().unwrap().join(".mcp.json");
    assert!(m.plan(outside).is_err());
    let mut traversal = f.input(tool);
    traversal.target_path = f.root.join("../.mcp.json");
    assert!(m.plan(traversal).is_err());
    let mut shell = f.input(tool);
    shell.executable.args.push("--token=fixture-secret".into());
    let error = m.plan(shell).unwrap_err();
    assert!(!error.to_string().contains("fixture-secret"));
    let expanding = f._dir.path().join("${UNSET_VARIABLE}.js");
    fs::write(&expanding, b"// literal filename").unwrap();
    let mut expansion = f.input(tool);
    expansion.executable.args[0] = expanding.to_str().unwrap().into();
    assert!(m.plan(expansion).is_err());
    assert_eq!(fs::read_dir(&f.root).unwrap().count(), 0);
    f.seed(tool, b"{\"mcpServers\":{\"other\":1,\"other\":2}}");
    assert_eq!(
        m.plan(f.input(tool)).unwrap_err().code,
        "SETUP_MCP_CONFIG_CONFLICT"
    );
    assert!(
        fs::read_to_string(f.input(tool).target_path)
            .unwrap()
            .contains("\"other\":1")
    );
    let descriptor = credentials::McpCredentials {
        schema_version: 1,
        broker_url: "http://127.0.0.1:43127".into(),
        local_token: "synthetic-descriptor-token".into(),
    };
    f.seed(tool, &serde_json::to_vec(&descriptor).unwrap());
    let mut collision = f.input(tool);
    collision.broker_file = collision.target_path.clone();
    assert!(m.plan(collision).is_err());
    assert_eq!(
        format::merge(
            tool,
            Some(&serde_json::to_vec(&descriptor).unwrap()),
            &json!({})
        )
        .unwrap_err()
        .code,
        "SETUP_UNSAFE_FILE"
    );
    let codex_home = f.root.join(".codex");
    assert!(
        storage::global_target(
            &f.root.canonicalize().unwrap().join(".codex/config.toml"),
            Some(&codex_home)
        )
        .unwrap()
    );
    assert!(!codex_home.exists());
}
#[test]
fn inline_toml_and_large_host_configs_preserve_their_other_entries() {
    let f = Fixture::new();
    let mut codex = Fixture::manager(ToolType::Codex);
    f.seed(
        ToolType::Codex,
        b"mcp_servers = { other = { command = 'other', env = { API_KEY = 'fixture-secret' } } }\n",
    );
    let p = codex.plan(f.input(ToolType::Codex)).unwrap();
    codex
        .apply(&p.plan_id, p.expected_digest.as_deref())
        .unwrap();
    assert_eq!(
        codex.inspect(f.inspect(ToolType::Codex)).unwrap().status,
        InspectionStatus::Applied
    );
    let mut claude = Fixture::manager(ToolType::ClaudeCode);
    let large = json!({"mcpServers":{"other":{"env":{"DATA":"x".repeat(20*1024)}}}});
    f.seed(ToolType::ClaudeCode, &serde_json::to_vec(&large).unwrap());
    let p = claude.plan(f.input(ToolType::ClaudeCode)).unwrap();
    claude
        .apply(&p.plan_id, p.expected_digest.as_deref())
        .unwrap();
    let after: serde_json::Value =
        serde_json::from_slice(&fs::read(f.input(ToolType::ClaudeCode).target_path).unwrap())
            .unwrap();
    assert_eq!(after["mcpServers"]["other"], large["mcpServers"]["other"]);
}
#[test]
fn unrelated_json_numbers_are_preserved_without_floating_point_rounding() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let mut m = Fixture::manager(tool);
    f.seed(tool,b"{\"metadata\":123456789012345678901234567890,\"mcpServers\":{\"other\":{\"value\":18446744073709551617}}}");
    let p = m.plan(f.input(tool)).unwrap();
    m.apply(&p.plan_id, p.expected_digest.as_deref()).unwrap();
    let after = fs::read_to_string(p.target_path).unwrap();
    assert!(after.contains("123456789012345678901234567890"));
    assert!(after.contains("18446744073709551617"));
}
#[test]
fn stale_plans_from_another_provider_cannot_replace_the_applied_profile() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let mut a = Fixture::manager(tool);
    let mut b = Fixture::manager(tool);
    let pa = a.plan(f.input(tool)).unwrap();
    let mut input = f.input(tool);
    input.broker_file = f.broker.parent().unwrap().join("other-broker.json");
    let credentials = credentials::McpCredentials {
        schema_version: 1,
        broker_url: "http://127.0.0.1:43128".into(),
        local_token: "synthetic-other-local-token".into(),
    };
    crate::private_file::write(&input.broker_file, &credentials, 16384).unwrap();
    let pb = b.plan(input).unwrap();
    a.apply(&pa.plan_id, None).unwrap();
    let bytes = fs::read(&pa.target_path).unwrap();
    assert_eq!(
        b.apply(&pb.plan_id, None).unwrap_err().code,
        "SETUP_MCP_CONFIG_CONFLICT"
    );
    assert_eq!(fs::read(pa.target_path).unwrap(), bytes);
}
#[cfg(windows)]
#[test]
fn occupied_destination_write_failure_keeps_original_and_backup() {
    use std::os::windows::fs::OpenOptionsExt;
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let before = original(tool);
    f.seed(tool, &before);
    let mut m = Fixture::manager(tool);
    let p = m.plan(f.input(tool)).unwrap();
    let _held = fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&p.target_path)
        .unwrap();
    let error = m
        .apply(&p.plan_id, p.expected_digest.as_deref())
        .unwrap_err();
    assert_eq!(error.code, "SETUP_MCP_CONFIG_WRITE_FAILED");
    assert_eq!(error.outcome, "not_applied");
    assert_eq!(fs::read(&p.target_path).unwrap(), before);
    assert_eq!(fs::read(error.backup_path.unwrap()).unwrap(), before);
}

#[test]
fn linked_parent_is_rejected_without_writing_the_destination() {
    let f = Fixture::new();
    let destination = f._dir.path().join("outside");
    fs::create_dir(&destination).unwrap();
    let link = f.root.join(".codex");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&destination, &link).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output=std::process::Command::new("powershell.exe")
            .args(["-NoProfile","-NonInteractive","-Command","$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:DEVLINK_MCP_TEST_LINK -Target $env:DEVLINK_MCP_TEST_DESTINATION | Out-Null"])
            .env("DEVLINK_MCP_TEST_LINK",&link).env("DEVLINK_MCP_TEST_DESTINATION",&destination)
            .creation_flags(0x08000000).output().unwrap();
        assert!(output.status.success());
    }
    let error = Fixture::manager(ToolType::Codex)
        .plan(f.input(ToolType::Codex))
        .unwrap_err();
    assert_eq!(error.code, "SETUP_UNSAFE_FILE");
    assert_eq!(fs::read_dir(destination).unwrap().count(), 0);
}

#[test]
fn create_inspection_failure_retains_file_with_explicit_uncertainty() {
    for tool in [ToolType::Codex, ToolType::ClaudeCode] {
        let f = Fixture::new();
        let mut m = Fixture::manager(tool);
        let p = m.plan(f.input(tool)).unwrap();
        m.fault = Some(Fault::AfterCommitFailure);
        let error = m.apply(&p.plan_id, None).unwrap_err();
        assert_eq!(error.outcome, "unknown");
        assert_eq!(error.rollback, "not_needed");
        assert!(error.backup_path.is_none());
        assert!(p.target_path.is_file());
        assert_eq!(
            m.inspect(f.inspect(tool)).unwrap().status,
            InspectionStatus::Applied
        );
    }
}
#[test]
fn final_digest_fence_and_backup_capacity_preserve_external_content() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    f.seed(tool, &original(tool));
    let input = storage::normalize(tool, f.input(tool)).unwrap();
    let before = storage::read(&input.target).unwrap();
    let expected = storage::digest(before.as_deref());
    fs::write(&input.target, outside(tool)).unwrap();
    assert_eq!(
        storage::write_version(&input.target, b"{}", false, expected.as_deref())
            .unwrap_err()
            .code,
        "SETUP_MCP_CONFIG_CONFLICT"
    );
    assert_eq!(fs::read(&input.target).unwrap(), outside(tool));
    let backup = storage::backup(&input, Some(b"original")).unwrap().unwrap();
    assert!(!backup.starts_with(&f.root));
    assert_eq!(fs::read(&backup).unwrap(), b"original");
    for n in 1..64 {
        fs::write(
            backup.parent().unwrap().join(format!("retained-{n}")),
            b"fixture",
        )
        .unwrap();
    }
    let full = storage::backup(&input, Some(b"another")).unwrap_err();
    assert_eq!(full.outcome, "not_applied");
    assert_eq!(
        full.recovery_directory.as_deref().map(PathBuf::as_path),
        backup.parent()
    );
    assert!(full.backup_path.is_none());
    assert_eq!(fs::read(&input.target).unwrap(), outside(tool));
}
#[test]
fn inspection_reports_missing_runtime_as_unknown_without_creating_files() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    fs::remove_file(&f.executable.command).unwrap();
    assert_eq!(
        Fixture::manager(tool)
            .inspect(f.inspect(tool))
            .unwrap()
            .status,
        InspectionStatus::Unknown
    );
    assert_eq!(fs::read_dir(&f.root).unwrap().count(), 0);
}
#[test]
fn json_merge_preserves_root_and_server_key_order() {
    let (_, after) = format::merge(
        ToolType::ClaudeCode,
        Some(
            br#"{"z":1,"mcpServers":{"zServer":{"command":"z"},"aServer":{"command":"a"}},"a":2}"#,
        ),
        &json!({"command":"node"}),
    )
    .unwrap();
    let text = String::from_utf8(after).unwrap();
    assert!(text.find("\"z\"").unwrap() < text.find("\"mcpServers\"").unwrap());
    assert!(text.find("zServer").unwrap() < text.find("aServer").unwrap());
    assert!(text.find("aServer").unwrap() < text.find("devlink").unwrap());
}
#[cfg(unix)]
#[test]
fn public_readable_host_config_is_accepted_but_other_writers_are_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    f.seed(tool, &original(tool));
    let path = f.input(tool).target_path;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        Fixture::manager(tool).plan(f.input(tool)).unwrap().change,
        Change::Merge
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
    assert_eq!(
        Fixture::manager(tool).plan(f.input(tool)).unwrap_err().code,
        "SETUP_UNSAFE_FILE"
    );
}

#[test]
fn oversized_proposed_config_is_rejected_before_host_write() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let input = serde_json::to_vec(&json!({"data":"x".repeat(LIMIT-64)})).unwrap();
    assert!(input.len() < LIMIT);
    f.seed(tool, &input);
    let error = Fixture::manager(tool).plan(f.input(tool)).unwrap_err();
    assert_eq!(error.code, "SETUP_MCP_CONFIG_WRITE_FAILED");
    assert_eq!(fs::read(f.input(tool).target_path).unwrap(), input);
}
#[cfg(windows)]
fn host_permission_snapshot(path: &std::path::Path) -> String {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "[System.IO.File]::GetAccessControl($env:DEVLINK_MCP_TEST_ACL).GetSecurityDescriptorSddlForm([System.Security.AccessControl.AccessControlSections]::All)",
        ])
        .env("DEVLINK_MCP_TEST_ACL", path)
        .creation_flags(0x08000000)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}
#[test]
fn existing_read_access_is_preserved_on_merge_and_restore() {
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let before = original(tool);
    f.seed(tool, &before);
    let path = f.input(tool).target_path;
    #[cfg(windows)]
    let expected = {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("icacls.exe")
            .arg(&path)
            .args(["/grant", "*S-1-5-32-545:(R)"])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(output.status.success());
        host_permission_snapshot(&path)
    };
    #[cfg(unix)]
    let expected = {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        0o644
    };
    let mut m = Fixture::manager(tool);
    let p = m.plan(f.input(tool)).unwrap();
    m.fault = Some(Fault::AfterCommitFailure);
    let error = m
        .apply(&p.plan_id, p.expected_digest.as_deref())
        .unwrap_err();
    assert_eq!(error.rollback, "restored");
    assert_eq!(fs::read(&path).unwrap(), before);
    #[cfg(windows)]
    assert_eq!(host_permission_snapshot(&path), expected);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            expected
        );
    }
    credentials::verify_private_permissions(&error.backup_path.unwrap()).unwrap();
}
#[cfg(unix)]
#[test]
fn stable_alias_in_writable_parent_cannot_receive_credentials() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let f = Fixture::new();
    let tool = ToolType::ClaudeCode;
    let alias = f._dir.path().join("unsafe-alias");
    fs::create_dir(&alias).unwrap();
    let script = alias.join("index.js");
    symlink(&f.executable.args[0], &script).unwrap();
    fs::set_permissions(&alias, fs::Permissions::from_mode(0o777)).unwrap();
    let mut input = f.input(tool);
    input.executable.args[0] = script.to_str().unwrap().into();
    assert_eq!(
        Fixture::manager(tool).plan(input).unwrap_err().code,
        "SETUP_UNSAFE_FILE"
    );
    assert_eq!(fs::read_dir(&f.root).unwrap().count(), 0);
}

#[test]
fn manual_guidance_uses_provider_formats_without_writing_host_files_or_credentials() {
    let fixture = Fixture::new();
    let original = fs::read(&fixture.broker).unwrap();
    let token: serde_json::Value = serde_json::from_slice(&original).unwrap();
    for tool in [ToolType::Codex, ToolType::ClaudeCode] {
        let input = fixture.input(tool);
        let target = input.target_path.clone();
        let manual = manual_config(input).unwrap();
        assert!(!target.exists());
        assert!(
            !manual
                .snippet
                .contains(token["localToken"].as_str().unwrap())
        );
        assert!(manual.snippet.contains("DEVLINK_BROKER_FILE"));
        assert_eq!(fs::read(&fixture.broker).unwrap(), original);
    }
}

#[test]
fn preflight_checks_both_host_formats_without_requiring_or_creating_credentials() {
    let fixture = Fixture::new();
    fs::remove_file(&fixture.broker).unwrap();
    for tool in [ToolType::Codex, ToolType::ClaudeCode] {
        let input = fixture.input(tool);
        preflight(input.clone(), true).unwrap();
        assert!(!input.target_path.exists());
        if let Some(parent) = input.target_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let conflict: &[u8] = match tool {
            ToolType::Codex => b"[mcp_servers]\ndevlink={command='other'}\n",
            ToolType::ClaudeCode => br#"{"mcpServers":{"devlink":{"command":"other"}}}"#,
        };
        fs::write(&input.target_path, conflict).unwrap();
        let error = preflight(input.clone(), true).unwrap_err();
        assert_eq!(error.code, "SETUP_MCP_CONFIG_CONFLICT");
        let conflict_details = error.conflict.unwrap();
        assert_eq!(
            conflict_details.target_path,
            input.target_path.canonicalize().unwrap()
        );
        assert_ne!(conflict_details.proposed["command"], "other");
        assert_eq!(fs::read(&input.target_path).unwrap(), conflict);
        preflight(input, false).unwrap();
        assert!(!fixture.broker.exists());
    }
}
