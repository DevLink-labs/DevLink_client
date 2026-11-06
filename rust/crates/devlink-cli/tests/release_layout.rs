//! The release bundle (.github/scripts/package-release.mjs) puts `mcp-server/` next to the
//! `devlink` executable. Setup must resolve that sibling entry without `--mcp-entry`.
#[path = "support/tool_fixture.rs"]
mod tool_fixture;

use devlink_broker::{
    credentials,
    sessions::ToolType,
    setup_mcp::{self, Mode, Options, PreflightContext},
};
use std::{fs, path::Path};

fn context(root: &Path) -> PreflightContext {
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let profile = root.join("profile");
    fs::create_dir_all(&profile).unwrap();
    PreflightContext {
        config_path: profile.join("config.json"),
        project,
        tool_type: ToolType::Codex,
        host_launcher: None,
        mcp_automatic_supported: false,
    }
}

/// Entry discovery only looks at the executable's directory, so a placeholder file is enough.
fn bundle_executable(bundle: &Path) -> std::path::PathBuf {
    fs::create_dir_all(bundle).unwrap();
    let executable = bundle.join(if cfg!(windows) {
        "devlink.exe"
    } else {
        "devlink"
    });
    fs::write(&executable, "").unwrap();
    executable
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_resolves_the_mcp_server_beside_a_release_executable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    credentials::set_private_permissions(root).unwrap();
    let (node, _) = tool_fixture::mcp_runtime(&root.join("node"));
    let bundle = root.join("devlink-0.0.0-test");
    let executable = bundle_executable(&bundle);
    let entry = bundle.join("mcp-server").join("dist").join("index.js");
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, "// bundled MCP entry\n").unwrap();

    setup_mcp::preflight(
        context(root),
        Options {
            mode: Mode::Manual,
            node: Some(node.clone()),
            ..Default::default()
        },
        executable,
    )
    .await
    .unwrap_or_else(|e| panic!("{}", serde_json::to_string(&e).unwrap()));

    // The synthetic Node logs its arguments: the probe ran the sibling entry over stdio.
    // Compare by path components; the resolved entry may mix `/` and `\` on Windows.
    let calls = fs::read_to_string(node.with_extension("mcp-calls")).unwrap();
    assert!(
        calls.lines().any(|line| {
            let args: Vec<_> = line.split('|').collect();
            args.len() == 3 && Path::new(args[0]) == entry && args[1..] == ["--transport", "stdio"]
        }),
        "probe calls: {calls}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_executable_without_the_mcp_server_reports_the_tool_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    credentials::set_private_permissions(root).unwrap();
    let (node, _) = tool_fixture::mcp_runtime(&root.join("node"));
    let executable = bundle_executable(&root.join("devlink-0.0.0-test"));
    let options = Options {
        mode: Mode::Manual,
        node: Some(node.clone()),
        ..Default::default()
    };

    let error = setup_mcp::preflight(context(root), options.clone(), executable.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "SETUP_TOOL_UNAVAILABLE");
    assert!(!node.with_extension("mcp-calls").exists());

    // Control: the same state succeeds once an entry is given, so only discovery failed.
    let entry = root.join("elsewhere").join("index.js");
    fs::create_dir_all(entry.parent().unwrap()).unwrap();
    fs::write(&entry, "// explicit MCP entry\n").unwrap();
    setup_mcp::preflight(
        context(root),
        Options {
            entry: Some(entry),
            ..options
        },
        executable,
    )
    .await
    .unwrap_or_else(|e| panic!("{}", serde_json::to_string(&e).unwrap()));
}
