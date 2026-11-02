use super::*;
use crate::{credentials, private_file};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Component, Path},
};
fn invalid() -> ConfigError {
    ConfigError::new("SETUP_INPUT_INVALID")
}
fn unsafe_file() -> ConfigError {
    ConfigError::new("SETUP_UNSAFE_FILE")
}
fn write_error() -> ConfigError {
    ConfigError::new("SETUP_MCP_CONFIG_WRITE_FAILED")
}
pub(super) fn host_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    #[cfg(windows)]
    if let Some(rest) = value.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix(r"UNC\") {
            return format!(r"\\{unc}");
        }
        if rest.as_bytes().get(1) == Some(&b':') {
            return rest.to_owned();
        }
    }
    value.into_owned()
}
fn absolute(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path.to_str().is_none()
        || path
            .to_str()
            .is_some_and(|s| s.contains("${") || s.chars().any(char::is_control))
        || path
            .components()
            .any(|p| matches!(p, Component::ParentDir | Component::CurDir))
    {
        return Err(invalid());
    }
    Ok(())
}
pub(super) fn global_target(target: &Path, global_home: Option<&Path>) -> Result<bool> {
    let Some(home) = global_home else {
        return Ok(false);
    };
    let home = std::path::absolute(home).map_err(|_| invalid())?;
    let input = home.join("config.toml");
    let mut ancestor = input.as_path();
    let mut suffix = vec![];
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(ancestor.file_name().ok_or_else(invalid)?.to_owned());
                ancestor = ancestor.parent().ok_or_else(invalid)?;
            }
            Err(_) => return Err(unsafe_file()),
        }
    }
    let mut resolved = ancestor.canonicalize().map_err(|_| unsafe_file())?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(host_path(&resolved).eq_ignore_ascii_case(&host_path(target)))
}
pub(super) fn project_from_target(tool: ToolType, target: &Path) -> Result<PathBuf> {
    absolute(target)?;
    let parent = target.parent().ok_or_else(invalid)?;
    match tool {
        ToolType::Codex
            if target.file_name() == Some("config.toml".as_ref())
                && parent.file_name() == Some(".codex".as_ref()) =>
        {
            Ok(parent.parent().ok_or_else(invalid)?.to_path_buf())
        }
        ToolType::ClaudeCode if target.file_name() == Some(".mcp.json".as_ref()) => {
            Ok(parent.to_path_buf())
        }
        _ => Err(invalid()),
    }
}
pub(super) fn normalize(host: ToolType, input: PlanInput) -> Result<Normalized> {
    if host != input.tool_type {
        return Err(invalid());
    }
    absolute(&input.project_path)?;
    let root = input
        .project_path
        .canonicalize()
        .map_err(|_| ConfigError::new("SETUP_PROJECT_INVALID"))?;
    absolute(&root)?;
    credentials::verify_directory_permissions(&root).map_err(|_| unsafe_file())?;
    if root.parent().is_none() {
        return Err(invalid());
    }
    if dirs::home_dir()
        .and_then(|p| p.canonicalize().ok())
        .as_ref()
        == Some(&root)
    {
        return Err(invalid());
    }
    if project_from_target(host, &input.target_path)?
        .canonicalize()
        .map_err(|_| invalid())?
        != root
    {
        return Err(invalid());
    }
    let target = root.join(match host {
        ToolType::Codex => ".codex/config.toml",
        ToolType::ClaudeCode => ".mcp.json",
    });
    let configured_home = std::env::var_os("CODEX_HOME");
    if host == ToolType::Codex && global_target(&target, configured_home.as_deref().map(Path::new))?
    {
        return Err(invalid());
    }
    let parent = target.parent().ok_or_else(invalid)?;
    match fs::symlink_metadata(parent) {
        Ok(_) => credentials::verify_directory_permissions(parent).map_err(|_| unsafe_file())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(unsafe_file()),
    }
    credentials::ensure_safe_target_if_exists(&target).map_err(|_| unsafe_file())?;
    let broker = credential_path(&input.broker_file)?;
    if host_path(&broker).eq_ignore_ascii_case(&host_path(&target)) {
        return Err(invalid());
    }
    // v1 adapter is the tested Node stdio runtime only, never a shell command or token argv.
    absolute(&input.executable.command)?;
    let command = input
        .executable
        .command
        .canonicalize()
        .map_err(|_| ConfigError::new("SETUP_TOOL_UNAVAILABLE"))?;
    absolute(&command)?;
    let name = input
        .executable
        .command
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if !command.is_file()
        || !(name.eq_ignore_ascii_case("node.exe") || name == "node" || name == "nodejs")
        || input.executable.args.len() != 3
        || input.executable.args[1..] != ["--transport", "stdio"]
    {
        return Err(ConfigError::new("SETUP_TOOL_UNAVAILABLE"));
    }
    let supplied_script = PathBuf::from(&input.executable.args[0]);
    let script = supplied_script.clone();
    absolute(&script)?;
    let script = script
        .canonicalize()
        .map_err(|_| ConfigError::new("SETUP_TOOL_UNAVAILABLE"))?;
    absolute(&script)?;
    if !script.is_file()
        || !matches!(
            script.extension().and_then(|s| s.to_str()),
            Some("js" | "mjs")
        )
    {
        return Err(ConfigError::new("SETUP_TOOL_UNAVAILABLE"));
    }
    credentials::verify_public_file_permissions(&command, true).map_err(|_| unsafe_file())?;
    credentials::verify_public_file_permissions(&script, true).map_err(|_| unsafe_file())?;
    super::permissions::source_alias(&input.executable.command)?;
    super::permissions::source_alias(&supplied_script)?;
    let canonical_command = command;
    let canonical_script = script;
    let script = supplied_script.to_str().ok_or_else(invalid)?.to_owned();
    Ok(Normalized {
        tool: host,
        root,
        target,
        broker,
        canonical_command,
        canonical_script,
        executable: Executable {
            command: input.executable.command,
            args: vec![script, "--transport".into(), "stdio".into()],
        },
    })
}
fn credential_path(path: &Path) -> Result<PathBuf> {
    absolute(path)?;
    let mut ancestor = path.parent().ok_or_else(invalid)?;
    let mut suffix = vec![];
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => break,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(ancestor.file_name().ok_or_else(invalid)?.to_owned());
                ancestor = ancestor.parent().ok_or_else(invalid)?;
            }
            Err(_) => return Err(unsafe_file()),
        }
    }
    credentials::verify_directory_permissions(ancestor).map_err(|_| unsafe_file())?;
    let mut canonical = ancestor.canonicalize().map_err(|_| unsafe_file())?;
    for part in suffix.into_iter().rev() {
        canonical.push(part);
    }
    canonical.push(path.file_name().ok_or_else(invalid)?);
    if let Some(value) = private_file::read::<credentials::McpCredentials>(&canonical, 16384)
        .map_err(|_| unsafe_file())?
    {
        credentials::validate(&value).map_err(|_| unsafe_file())?;
    }
    Ok(canonical)
}
pub(super) fn require_credentials(path: &Path) -> Result<()> {
    let value = private_file::read::<credentials::McpCredentials>(path, 16384)
        .map_err(|_| unsafe_file())?
        .ok_or_else(invalid)?;
    credentials::validate(&value).map_err(|_| unsafe_file())
}
pub(super) fn revalidate(input: &Normalized) -> Result<()> {
    let current = normalize(
        input.tool,
        PlanInput {
            tool_type: input.tool,
            project_path: input.root.clone(),
            target_path: input.target.clone(),
            broker_file: input.broker.clone(),
            executable: input.executable.clone(),
        },
    )?;
    if current.root != input.root
        || current.target != input.target
        || current.broker != input.broker
        || current.canonical_command != input.canonical_command
        || current.canonical_script != input.canonical_script
        || current.executable.command != input.executable.command
        || current.executable.args != input.executable.args
    {
        return Err(unsafe_file());
    }
    Ok(())
}
pub(super) fn read(path: &Path) -> Result<Option<Vec<u8>>> {
    if !credentials::ensure_safe_target_if_exists(path).map_err(|_| unsafe_file())? {
        return Ok(None);
    }
    credentials::verify_parent_safety(path.parent().ok_or_else(invalid)?)
        .map_err(|_| unsafe_file())?;
    credentials::verify_public_file_permissions(path, false).map_err(|_| unsafe_file())?;
    let file = fs::File::open(path).map_err(|_| unsafe_file())?;
    let meta = file.metadata().map_err(|_| unsafe_file())?;
    if !meta.is_file() || meta.len() > LIMIT as u64 {
        return Err(unsafe_file());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unsafe_file())?;
    if bytes.len() > LIMIT {
        return Err(unsafe_file());
    }
    Ok(Some(bytes))
}
pub(super) fn digest(bytes: Option<&[u8]>) -> Option<String> {
    bytes.map(|b| format!("{:x}", Sha256::digest(b)))
}
fn private_dir(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            credentials::verify_state_directory_permissions(path).map_err(|_| unsafe_file())?
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            private_dir(path.parent().ok_or_else(invalid)?)?;
            match super::permissions::create_private_directory(path) {
                Ok(()) => credentials::set_private_permissions(path).map_err(|_| unsafe_file())?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(write_error()),
            }
            credentials::verify_state_directory_permissions(path).map_err(|_| unsafe_file())?;
        }
        Err(_) => return Err(unsafe_file()),
    }
    Ok(())
}
fn state(input: &Normalized) -> Result<PathBuf> {
    #[cfg(test)]
    let root = input
        .root
        .parent()
        .ok_or_else(invalid)?
        .join("mcp-provider-state");
    #[cfg(not(test))]
    let root = dirs::home_dir()
        .ok_or_else(unsafe_file)?
        .join(".devlink-mcp-config");
    let key = host_path(&input.target);
    #[cfg(windows)]
    let key = key.to_ascii_lowercase();
    let path = root.join(format!("{:x}", Sha256::digest(key.as_bytes())));
    private_dir(&path)?;
    Ok(path)
}
pub(super) fn lock(input: &Normalized) -> Result<std::fs::File> {
    let parent = input.target.parent().ok_or_else(invalid)?;
    match fs::symlink_metadata(parent) {
        Ok(_) => credentials::verify_directory_permissions(parent).map_err(|_| unsafe_file())?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            super::permissions::create_private_directory(parent).map_err(|_| write_error())?;
            credentials::set_private_permissions(parent).map_err(|_| unsafe_file())?;
            credentials::verify_directory_permissions(parent).map_err(|_| unsafe_file())?;
        }
        Err(_) => return Err(unsafe_file()),
    }
    let path = state(input)?.join("apply.lock");
    if credentials::ensure_safe_target_if_exists(&path).map_err(|_| unsafe_file())? {
        credentials::verify_private_permissions(&path).map_err(|_| unsafe_file())?;
    }
    let lock =
        crate::config::lock(&path).map_err(|_| ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"))?;
    credentials::set_private_permissions(&path).map_err(|_| unsafe_file())?;
    credentials::verify_private_permissions(&path).map_err(|_| unsafe_file())?;
    Ok(lock)
}
pub(super) fn backup(input: &Normalized, bytes: Option<&[u8]>) -> Result<Option<PathBuf>> {
    let Some(bytes) = bytes else { return Ok(None) };
    let parent = state(input)?.join("backups");
    private_dir(&parent)?;
    // Retain exact recovery files, never silently prune them. A full store requires manual review.
    if fs::read_dir(&parent)
        .map_err(|_| unsafe_file())?
        .take(64)
        .count()
        >= 64
    {
        let mut error = write_error();
        error.recovery_directory = Some(parent);
        return Err(error);
    }
    let path = parent.join(format!("config-{}.bak", uuid::Uuid::new_v4()));
    write(&path, bytes, true).map_err(|mut e| {
        e.backup_path = if path.is_file() {
            Some(path.clone())
        } else {
            None
        };
        e.outcome = "not_applied";
        e
    })?;
    Ok(Some(path))
}
pub(super) fn write(path: &Path, bytes: &[u8], create: bool) -> Result<()> {
    let expected = if create {
        None
    } else {
        digest(read(path)?.as_deref())
    };
    write_version(path, bytes, create, expected.as_deref())
}
pub(super) fn write_version(
    path: &Path,
    bytes: &[u8],
    create: bool,
    expected: Option<&str>,
) -> Result<()> {
    let parent = path.parent().ok_or_else(invalid)?;
    credentials::verify_directory_permissions(parent).map_err(|_| unsafe_file())?;
    if credentials::ensure_safe_target_if_exists(path).map_err(|_| unsafe_file())? {
        credentials::verify_public_file_permissions(path, false).map_err(|_| unsafe_file())?;
    }
    let mut temp = tempfile::Builder::new()
        .prefix(".devlink-mcp-")
        .tempfile_in(parent)
        .map_err(|_| write_error())?;
    credentials::set_private_permissions(temp.path()).map_err(|_| unsafe_file())?;
    temp.write_all(bytes).map_err(|_| write_error())?;
    temp.as_file().sync_all().map_err(|_| write_error())?;
    if !create {
        // Preserve existing Host read access and mode; backups remain newly private files.
        super::permissions::copy_host_permissions(path, temp.path())?;
        credentials::verify_public_file_permissions(temp.path(), false)
            .map_err(|_| unsafe_file())?;
    } else {
        credentials::verify_private_permissions(temp.path()).map_err(|_| unsafe_file())?;
    }
    // Place the digest fence after temporary data/ACL preparation, immediately
    // before the atomic replacement, not before potentially slow ACL checks.
    if digest(read(path)?.as_deref()).as_deref() != expected {
        return Err(ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"));
    }
    if create {
        temp.persist_noclobber(path).map_err(|e| {
            if e.error.kind() == std::io::ErrorKind::AlreadyExists {
                ConfigError::new("SETUP_MCP_CONFIG_CONFLICT")
            } else {
                write_error()
            }
        })?;
    } else {
        temp.persist(path).map_err(|_| write_error())?;
    }
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| ConfigError::uncertain("not_needed", None))?;
    Ok(())
}
