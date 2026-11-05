//! Host discovery runs only a bounded `--version` probe, never an AI session.
use crate::{
    mcp_config::{ConfigError, VerifiedHost},
    sessions::ToolType,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HostExecutable {
    pub command: PathBuf,
    /// Empty for a native binary; the package entry for a Node-based npm shim.
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectedTool {
    pub tool_type: ToolType,
    pub executable: HostExecutable,
    pub version: String,
}
impl SelectedTool {
    /// The provider alone decides whether this Host version's settings are supported.
    pub fn verified_host(&self) -> Result<VerifiedHost, ConfigError> {
        VerifiedHost::new(self.tool_type, &self.version)
    }
    pub(crate) fn valid_stored(&self) -> bool {
        let output = match self.tool_type {
            ToolType::Codex => format!("codex-cli {}", self.version),
            ToolType::ClaudeCode => format!("{} (Claude Code)", self.version),
        };
        launcher_shape(self.tool_type, &self.executable)
            && parse_version(self.tool_type, output.as_bytes()).is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectionStatus {
    Available,
    Missing,
    NotExecutable,
    VersionCheckFailed,
    TimedOut,
    InvalidVersion,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDetection {
    pub tool_type: ToolType,
    pub status: DetectionStatus,
    #[serde(rename = "installation", skip_serializing_if = "Option::is_none")]
    pub selected: Option<SelectedTool>,
    pub install_url: &'static str,
    pub mcp_automatic_supported: bool,
}

pub fn parse_tool(value: &str) -> Result<ToolType, &'static str> {
    match value {
        "codex" => Ok(ToolType::Codex),
        "claude-code" => Ok(ToolType::ClaudeCode),
        _ => Err("Choose codex or claude-code"),
    }
}
#[derive(Clone)]
pub struct ToolSearch {
    pub directories: Vec<PathBuf>,
    pub timeout: Duration,
}
impl Default for ToolSearch {
    fn default() -> Self {
        let mut directories: Vec<_> = std::env::var_os("PATH")
            .map(|v| {
                std::env::split_paths(&v)
                    .filter(|p| p.is_absolute())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(home) = dirs::home_dir() {
            directories.push(home.join(".local/bin"));
            directories.push(home.join(".cargo/bin"));
        }
        #[cfg(windows)]
        if let Some(appdata) = std::env::var_os("APPDATA") {
            directories.push(PathBuf::from(appdata).join("npm"));
        }
        Self {
            directories,
            timeout: Duration::from_secs(5),
        }
    }
}
impl ToolSearch {
    /// An explicit/saved executable is checked first, then PATH and standard install dirs.
    pub async fn detect(
        &self,
        preferred: Option<&SelectedTool>,
        explicit: Option<(ToolType, &Path)>,
    ) -> Vec<ToolDetection> {
        let (codex, claude) = tokio::join!(
            self.detect_one(ToolType::Codex, preferred, explicit),
            self.detect_one(ToolType::ClaudeCode, preferred, explicit)
        );
        vec![codex, claude]
    }

    /// Recheck the validated launcher form without discovering a replacement.
    pub async fn recheck_saved(&self, saved: &SelectedTool) -> ToolDetection {
        Self {
            directories: Vec::new(),
            timeout: self.timeout,
        }
        .detect_one(saved.tool_type, Some(saved), None)
        .await
    }

    /// Probe one tool only. A caller that already knows which tool it needs
    /// (explicit, bound, or chosen) must not pay for the other host's probe.
    pub async fn detect_one(
        &self,
        tool: ToolType,
        preferred: Option<&SelectedTool>,
        explicit: Option<(ToolType, &Path)>,
    ) -> ToolDetection {
        let mut report = ToolDetection {
            tool_type: tool,
            status: DetectionStatus::Missing,
            selected: None,
            install_url: match tool {
                ToolType::Codex => "https://learn.chatgpt.com/docs/codex/cli",
                ToolType::ClaudeCode => "https://code.claude.com/docs/en/setup",
            },
            mcp_automatic_supported: false,
        };
        let explicit = explicit
            .filter(|(kind, _)| *kind == tool)
            .map(|(_, path)| path);
        let mut candidates = Vec::new();
        if let Some(path) = explicit {
            if let Some(executable) = self.launcher(tool, path) {
                candidates.push(executable);
            } else {
                report.status = DetectionStatus::NotExecutable;
                return report;
            }
        } else {
            if let Some(saved) = preferred.filter(|saved| saved.tool_type == tool) {
                // Never replay arbitrary persisted argv. Only the supported launch forms are accepted.
                if let Some(executable) = reusable_launcher(tool, &saved.executable) {
                    candidates.push(executable);
                }
            }
            let name = match tool {
                ToolType::Codex => "codex",
                ToolType::ClaudeCode => "claude",
            };
            for directory in self.directories.iter().filter(|p| p.is_absolute()) {
                #[cfg(windows)]
                let names = [format!("{name}.exe"), format!("{name}.cmd")];
                #[cfg(not(windows))]
                let names = [name.to_owned()];
                for name in names {
                    let path = directory.join(name);
                    if path.exists() {
                        report.status = DetectionStatus::NotExecutable;
                        if let Some(executable) = self.launcher(tool, &path)
                            && !candidates.contains(&executable)
                        {
                            candidates.push(executable);
                        }
                        // Respect PATH precedence; do not bypass an unusable command with a later one.
                        break;
                    }
                }
                if report.status != DetectionStatus::Missing {
                    break;
                }
            }
        }
        for executable in candidates {
            match probe(tool, &executable, self.timeout).await {
                Ok(version) => {
                    report.status = DetectionStatus::Available;
                    report.selected = Some(SelectedTool {
                        tool_type: tool,
                        executable,
                        version,
                    });
                    report.mcp_automatic_supported = report
                        .selected
                        .as_ref()
                        .is_some_and(|s| s.verified_host().is_ok());
                    return report;
                }
                Err(status) => report.status = status,
            }
        }
        report
    }

    fn launcher(&self, _tool: ToolType, path: &Path) -> Option<HostExecutable> {
        let path = path.canonicalize().ok()?;
        if !path.is_file() {
            return None;
        }
        #[cfg(windows)]
        {
            if path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd"))
            {
                // Resolve npm shims to Node + a known package entry; never execute batch text.
                let parent = path.parent()?;
                let entry = parent.join(package_entry(_tool)).canonicalize().ok()?;
                if !entry.is_file() {
                    return None;
                }
                let node = std::iter::once(parent.to_path_buf())
                    .chain(self.directories.clone())
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join("node.exe"))
                    .find(|p| p.is_file())?
                    .canonicalize()
                    .ok()?;
                return Some(HostExecutable {
                    command: node,
                    args: vec![node_script_argument(&entry)?],
                });
            }
            if !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            {
                return None;
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&path).ok()?.permissions().mode() & 0o111 == 0 {
                return None;
            }
        }
        Some(HostExecutable {
            command: path,
            args: Vec::new(),
        })
    }
}

fn package_entry(tool: ToolType) -> &'static str {
    match tool {
        ToolType::Codex => "node_modules/@openai/codex/bin/codex.js",
        ToolType::ClaudeCode => "node_modules/@anthropic-ai/claude-code/cli.js",
    }
}
fn reusable_launcher(tool: ToolType, executable: &HostExecutable) -> Option<HostExecutable> {
    if !launcher_shape(tool, executable) || !executable.command.is_file() {
        return None;
    }
    let mut executable = executable.clone();
    if let Some(script) = executable.args.first_mut() {
        // Also repair argv saved by an older build without requiring PATH rediscovery.
        *script = node_script_argument(Path::new(script))?;
    }
    Some(executable)
}

fn node_script_argument(path: &Path) -> Option<String> {
    let canonical = path.canonicalize().ok()?;
    if !canonical.is_file() {
        return None;
    }
    let text = canonical.to_str()?;
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        // Node's module loader fails on verbatim script argv. Keep filesystem checks
        // canonical, but pass an ordinary drive/UNC path to the Node CLI.
        let Component::Prefix(prefix) = canonical.components().next()? else {
            return None;
        };
        let argument = match prefix.kind() {
            Prefix::VerbatimDisk(_) => text.strip_prefix(r"\\?\")?.to_owned(),
            Prefix::VerbatimUNC(_, _) => format!(r"\\{}", text.strip_prefix(r"\\?\UNC\")?),
            Prefix::Disk(_) | Prefix::UNC(_, _) => text.to_owned(),
            _ => return None,
        };
        // Removing the prefix must not change the target (e.g. trailing dots/spaces).
        (Path::new(&argument).canonicalize().ok()? == canonical).then_some(argument)
    }
    #[cfg(not(windows))]
    {
        Some(text.to_owned())
    }
}
fn launcher_shape(tool: ToolType, executable: &HostExecutable) -> bool {
    if !executable.command.is_absolute() {
        return false;
    }
    if executable.args.is_empty() {
        #[cfg(windows)]
        {
            return executable
                .command
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"));
        }
        #[cfg(not(windows))]
        {
            return true;
        }
    }
    executable.command.file_stem().is_some_and(|name| {
        if cfg!(windows) {
            name.eq_ignore_ascii_case(OsStr::new("node"))
        } else {
            name == OsStr::new("node")
        }
    }) && executable.args.len() == 1
        && Path::new(&executable.args[0]).is_absolute()
        && Path::new(&executable.args[0]).ends_with(package_entry(tool))
}

async fn bounded_read(stream: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.take(8193).read_to_end(&mut bytes).await?;
    if bytes.len() > 8192 {
        return Err(std::io::Error::other("Version output limit"));
    }
    Ok(bytes)
}
async fn probe(
    tool: ToolType,
    executable: &HostExecutable,
    timeout: Duration,
) -> Result<String, DetectionStatus> {
    let cwd = tempfile::tempdir().map_err(|_| DetectionStatus::VersionCheckFailed)?;
    let mut command = Command::new(&executable.command);
    command
        .args(&executable.args)
        .arg("--version")
        .current_dir(cwd.path())
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command
        .spawn()
        .map_err(|_| DetectionStatus::NotExecutable)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(DetectionStatus::VersionCheckFailed)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(DetectionStatus::VersionCheckFailed)?;
    let result = tokio::time::timeout(timeout, async {
        tokio::try_join!(bounded_read(stdout), bounded_read(stderr), child.wait())
    })
    .await;
    match result {
        Ok(Ok((stdout, _, status))) if status.success() => {
            parse_version(tool, &stdout).ok_or(DetectionStatus::InvalidVersion)
        }
        failure => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(if failure.is_err() {
                DetectionStatus::TimedOut
            } else {
                DetectionStatus::VersionCheckFailed
            })
        }
    }
}
fn parse_version(tool: ToolType, bytes: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let version = match tool {
        ToolType::Codex => text.strip_prefix("codex-cli ")?,
        ToolType::ClaudeCode => text.strip_suffix(" (Claude Code)")?,
    };
    if version.len() > 64
        || !version
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))
    {
        return None;
    }
    let core = version.split(['-', '+']).next()?;
    let parts: Vec<_> = core.split('.').collect();
    (parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
    .then(|| version.to_owned())
}
