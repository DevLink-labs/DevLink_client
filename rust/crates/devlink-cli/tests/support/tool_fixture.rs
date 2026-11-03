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
