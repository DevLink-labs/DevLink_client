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

/// Upper bound on reaping a probe or startup child after it was told to terminate.
/// Termination may be delayed by the OS or security software; once this passes the
/// child is left to finish exiting on its own, so the exit is not guaranteed yet.
pub(crate) const REAP_TIMEOUT: Duration = Duration::from_millis(500);

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
/// The Codex desktop app's MSIX family. The suffix is derived from the publisher's
/// signing identity, so another publisher cannot register a package under it.
pub const CODEX_PACKAGE_FAMILY: &str = "OpenAI.Codex_2p2nqsd0c76g0";
const CODEX_PACKAGE_PREFIX: &str = "OpenAI.Codex_";
const CODEX_PACKAGE_PUBLISHER: &str = "__2p2nqsd0c76g0";

/// How a desktop app lays out the Host it ships, relative to [`BundledHost::root`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BundleLayout {
    /// `<root>\<package>\claude-code\<version>\<build>\claude.exe` (Claude desktop app).
    ClaudeDesktop,
    /// `<root>\OpenAI.Codex_<version>_<arch>__2p2nqsd0c76g0\app\resources\codex.exe`
    /// (Codex desktop app, an MSIX package under WindowsApps).
    CodexPackage,
}

/// A Host shipped inside a desktop app instead of being put on PATH. Its directory
/// carries the app version, so every app update moves the executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundledHost {
    pub tool_type: ToolType,
    /// The directory holding the current copy of the native executable.
    pub directory: PathBuf,
    /// Canonical directory shared by every version of this bundle.
    pub root: PathBuf,
    pub layout: BundleLayout,
}
impl BundledHost {
    /// Whether `path` is a canonical launcher of this bundle, of any version. Only
    /// such a saved launcher may be replaced by the current copy.
    pub fn contains(&self, path: &Path) -> bool {
        let Ok(rest) = path.strip_prefix(&self.root) else {
            return false;
        };
        let parts: Vec<_> = rest.iter().map(|part| part.to_str()).collect();
        let is = |part: Option<&str>, expected: &str| {
            part.is_some_and(|part| part.eq_ignore_ascii_case(expected))
        };
        match (self.layout, parts.as_slice()) {
            (BundleLayout::ClaudeDesktop, [Some(_), tool, Some(_), Some(_), exe]) => {
                is(*tool, "claude-code") && is(*exe, "claude.exe")
            }
            (BundleLayout::CodexPackage, [Some(package), app, resources, exe]) => {
                is_codex_package_folder(package)
                    && is(*app, "app")
                    && is(*resources, "resources")
                    && is(*exe, "codex.exe")
            }
            _ => false,
        }
    }
}
fn is_codex_package_folder(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.len() > CODEX_PACKAGE_PREFIX.len() + CODEX_PACKAGE_PUBLISHER.len()
        && lower.starts_with(&CODEX_PACKAGE_PREFIX.to_ascii_lowercase())
        && lower.ends_with(CODEX_PACKAGE_PUBLISHER)
}

#[derive(Clone)]
pub struct ToolSearch {
    pub directories: Vec<PathBuf>,
    /// Desktop-app installs, searched only after every directory.
    pub bundled: Vec<BundledHost>,
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
        #[cfg(windows)]
        let bundled = desktop_bundled_claude()
            .into_iter()
            .chain(codex_app_bundle())
            .collect();
        #[cfg(not(windows))]
        let bundled = Vec::new();
        Self {
            directories,
            bundled,
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

    /// Recheck the validated launcher form without discovering a replacement on
    /// PATH. A launcher saved from a desktop app's bundle may move to that same
    /// bundle's current copy, since an app update removes the old version folder.
    pub async fn recheck_saved(&self, saved: &SelectedTool) -> ToolDetection {
        let bundled = self
            .bundled
            .iter()
            .filter(|bundle| {
                bundle.tool_type == saved.tool_type
                    && saved.executable.args.is_empty()
                    && bundle.contains(&saved.executable.command)
            })
            .cloned()
            .collect();
        Self {
            directories: Vec::new(),
            bundled,
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
            let bundled = self
                .bundled
                .iter()
                .filter(|bundle| bundle.tool_type == tool)
                .map(|bundle| &bundle.directory);
            for directory in self
                .directories
                .iter()
                .chain(bundled)
                .filter(|p| p.is_absolute())
            {
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

/// The Claude desktop app ships Claude Code without adding it to PATH:
/// `%LOCALAPPDATA%\Claude-Data\<package>\claude-code\<version>\<build>\claude.exe`.
/// Only the newest version is returned, after every PATH and standard install dir.
#[cfg(windows)]
fn desktop_bundled_claude() -> Option<BundledHost> {
    let root = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Claude-Data")
        .canonicalize()
        .ok()?;
    let mut newest: Option<(Vec<u64>, PathBuf)> = None;
    for package in std::fs::read_dir(&root).ok()?.flatten() {
        let Ok(versions) = std::fs::read_dir(package.path().join("claude-code")) else {
            continue;
        };
        for version in versions.flatten() {
            let Some(key) = version.file_name().to_str().and_then(|name| {
                let parts: Option<Vec<u64>> = name.split('.').map(|p| p.parse().ok()).collect();
                parts.filter(|parts| parts.len() == 3)
            }) else {
                continue;
            };
            let Ok(builds) = std::fs::read_dir(version.path()) else {
                continue;
            };
            for build in builds.flatten() {
                let dir = build.path();
                if dir.join("claude.exe").is_file()
                    && newest.as_ref().is_none_or(|(best, _)| key > *best)
                {
                    newest = Some((key.clone(), dir));
                }
            }
        }
    }
    newest.map(|(_, directory)| BundledHost {
        tool_type: ToolType::ClaudeCode,
        directory,
        root,
        layout: BundleLayout::ClaudeDesktop,
    })
}

/// The Codex desktop app ships the Codex CLI without a PATH entry or an execution
/// alias: `<WindowsApps>\OpenAI.Codex_<version>_<arch>__2p2nqsd0c76g0\app\resources\codex.exe`.
/// The install location is asked from the current user's package registration, so
/// an app update that renames the folder is followed without scanning WindowsApps
/// (which ordinary users cannot list).
#[cfg(windows)]
fn codex_app_bundle() -> Option<BundledHost> {
    let (_, package) = appx::family_packages(CODEX_PACKAGE_FAMILY)
        .into_iter()
        .filter(|(_, path)| path.join(r"app\resources\codex.exe").is_file())
        .max_by(|a, b| a.0.cmp(&b.0))?;
    let package = package.canonicalize().ok()?;
    let bundle = BundledHost {
        tool_type: ToolType::Codex,
        directory: package.join("app").join("resources"),
        root: package.parent()?.to_path_buf(),
        layout: BundleLayout::CodexPackage,
    };
    // Accept only the documented layout, so the same bundle can be followed later.
    bundle
        .contains(&bundle.directory.join("codex.exe"))
        .then_some(bundle)
}

#[cfg(windows)]
mod appx {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf, ptr::null_mut};
    use windows_sys::Win32::{
        Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS},
        Storage::Packaging::Appx::{GetPackagePathByFullName, GetPackagesByPackageFamily},
    };

    /// Packages of `family` registered for the current user, as (version, install path).
    pub(super) fn family_packages(family: &str) -> Vec<(Vec<u64>, PathBuf)> {
        let family: Vec<u16> = family.encode_utf16().chain([0]).collect();
        let (mut count, mut length) = (0u32, 0u32);
        // SAFETY: the documented size query passes null output buffers.
        let status = unsafe {
            GetPackagesByPackageFamily(
                family.as_ptr(),
                &mut count,
                null_mut(),
                &mut length,
                null_mut(),
            )
        };
        if status != ERROR_INSUFFICIENT_BUFFER || count == 0 || length == 0 {
            return Vec::new();
        }
        let mut names = vec![null_mut::<u16>(); count as usize];
        let mut buffer = vec![0u16; length as usize];
        // SAFETY: both buffers have the sizes the first call reported.
        let status = unsafe {
            GetPackagesByPackageFamily(
                family.as_ptr(),
                &mut count,
                names.as_mut_ptr(),
                &mut length,
                buffer.as_mut_ptr(),
            )
        };
        // A package registered between the calls fails this lookup; it is not retried.
        if status != ERROR_SUCCESS {
            return Vec::new();
        }
        names
            .iter()
            .take(count as usize)
            .filter_map(|&name| {
                // Each name points into `buffer`; read it by offset, never past its end.
                let offset = (name as usize).checked_sub(buffer.as_ptr() as usize)? / 2;
                let rest = buffer.get(offset..)?;
                let full_name = &rest[..rest.iter().position(|&c| c == 0)? + 1];
                let version =
                    package_version(&String::from_utf16(&full_name[..full_name.len() - 1]).ok()?)?;
                Some((version, package_path(full_name)?))
            })
            .collect()
    }

    /// `full_name` must be NUL-terminated.
    fn package_path(full_name: &[u16]) -> Option<PathBuf> {
        let mut length = 0u32;
        // SAFETY: the documented size query passes a null path buffer.
        let status =
            unsafe { GetPackagePathByFullName(full_name.as_ptr(), &mut length, null_mut()) };
        if status != ERROR_INSUFFICIENT_BUFFER || length == 0 {
            return None;
        }
        let mut path = vec![0u16; length as usize];
        // SAFETY: `path` has the length the first call reported.
        let status =
            unsafe { GetPackagePathByFullName(full_name.as_ptr(), &mut length, path.as_mut_ptr()) };
        if status != ERROR_SUCCESS {
            return None;
        }
        let end = path.iter().position(|&c| c == 0).unwrap_or(path.len());
        Some(PathBuf::from(OsString::from_wide(&path[..end]))).filter(|p| p.is_absolute())
    }

    /// `Name_Version_Architecture_ResourceId_PublisherId` → numeric version parts.
    fn package_version(full_name: &str) -> Option<Vec<u64>> {
        full_name
            .split('_')
            .nth(1)?
            .split('.')
            .map(|part| part.parse().ok())
            .collect()
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
            // Termination may be delayed by the OS or security software. Keep
            // cleanup bounded as well as the version probe itself.
            let _ = child.start_kill();
            let _ = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await;
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
