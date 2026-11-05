#![allow(dead_code)]
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

fn binary() -> &'static Path {
    static COMPILED: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &COMPILED
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let binary = directory.path().join(if cfg!(windows) {
                "test-host.exe"
            } else {
                "test-host"
            });
            let result = Command::new("rustc")
                .arg("--edition=2024")
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/tool_host.rs"))
                .arg("-o")
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            (directory, binary)
        })
        .1
}
pub fn host(directory: &Path, name: &str, output: &str) -> PathBuf {
    fs::create_dir_all(directory).unwrap();
    let target = directory.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    });
    fs::copy(binary(), &target).unwrap();
    fs::write(target.with_extension("probe"), output).unwrap();
    target
}
pub fn codex() -> &'static Path {
    static HOST: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    &HOST
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            let path = host(directory.path(), "codex", "codex-cli 0.166.0");
            (directory, path)
        })
        .1
}

pub fn mcp_runtime(directory: &Path) -> (PathBuf, PathBuf) {
    static RUNTIME: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();
    let compiled = &RUNTIME
        .get_or_init(|| {
            let temp = tempfile::tempdir().unwrap();
            let binary = temp.path().join(if cfg!(windows) {
                "mcp-node.exe"
            } else {
                "mcp-node"
            });
            let result = Command::new("rustc")
                .arg("--edition=2024")
                .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/mcp_node.rs"))
                .arg("-o")
                .arg(&binary)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            (temp, binary)
        })
        .1;
    fs::create_dir_all(directory).unwrap();
    let node = directory.join(if cfg!(windows) { "node.exe" } else { "node" });
    fs::copy(compiled, &node).unwrap();
    let entry = directory.join("index.mjs");
    fs::write(&entry, "// synthetic MCP runtime fixture\n").unwrap();
    (node, entry)
}

pub fn mcp_options(directory: &Path) -> devlink_broker::setup_mcp::Options {
    let (node, entry) = mcp_runtime(directory);
    devlink_broker::setup_mcp::Options {
        node: Some(node),
        entry: Some(entry),
        ..Default::default()
    }
}

pub fn default_mcp_options() -> devlink_broker::setup_mcp::Options {
    static RUNTIME: OnceLock<(tempfile::TempDir, devlink_broker::setup_mcp::Options)> =
        OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            let directory = tempfile::tempdir().unwrap();
            devlink_broker::credentials::set_private_permissions(directory.path()).unwrap();
            let options = mcp_options(directory.path());
            (directory, options)
        })
        .1
        .clone()
}
