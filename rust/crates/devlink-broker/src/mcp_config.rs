//! Project-only Host configuration provider for setup/v1. The CLI owns orchestration.
//! `hooks` is the exception: it installs the user-level Host prompt hooks.
mod format;
pub mod hooks;
mod permissions;
mod storage;
#[cfg(test)]
mod tests;
pub use crate::sessions::ToolType;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
};

pub const CODEX_VERSION: &str = "0.166.0";
pub const CLAUDE_VERSION: &str = "2.1.310";
const LIMIT: usize = 1024 * 1024;
type Result<T> = std::result::Result<T, ConfigError>;

/// The -108 adapter supplies its detected version. Unknown versions require manual setup.
pub struct VerifiedHost {
    tool: ToolType,
}
impl VerifiedHost {
    pub fn new(tool: ToolType, version: &str) -> Result<Self> {
        let expected = match tool {
            ToolType::Codex => CODEX_VERSION,
            ToolType::ClaudeCode => CLAUDE_VERSION,
        };
        if version != expected {
            return Err(ConfigError::new("SETUP_TOOL_UNAVAILABLE"));
        }
        Ok(Self { tool })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Executable {
    pub command: PathBuf,
    pub args: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanInput {
    pub tool_type: ToolType,
    pub project_path: PathBuf,
    pub target_path: PathBuf,
    pub broker_file: PathBuf,
    pub executable: Executable,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InspectionInput {
    pub tool_type: ToolType,
    pub target_path: PathBuf,
    pub broker_file: PathBuf,
    pub executable: Executable,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Create,
    Merge,
    Unchanged,
    Conflict,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigPlan {
    pub plan_id: String,
    pub tool_type: ToolType,
    pub target_path: PathBuf,
    pub expected_digest: Option<String>,
    pub change: Change,
    /// Only the proposed DevLink entry. No old configuration or environment is returned.
    pub diff: Option<serde_json::Value>,
    pub restart_required: bool,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigApplyResult {
    pub status: ApplyStatus,
    pub target_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
    pub restart_required: bool,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    Applied,
    Unchanged,
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InspectionStatus {
    Applied,
    Missing,
    Conflict,
    Unknown,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigInspection {
    pub status: InspectionStatus,
    pub target_path: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigConflict {
    pub target_path: PathBuf,
    /// Only the generated DevLink entry. Existing settings are never included.
    pub proposed: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigError {
    #[serde(skip)]
    pub path: Option<Box<PathBuf>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_directory: Option<Box<PathBuf>>,
    pub code: &'static str,
    pub outcome: &'static str,
    pub rollback: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<Box<ConfigConflict>>,
}
impl ConfigError {
    pub(crate) fn new(code: &'static str) -> Self {
        Self {
            code,
            outcome: "not_applied",
            rollback: "not_needed",
            backup_path: None,
            recovery_directory: None,
            path: None,
            conflict: None,
        }
    }
    pub(crate) fn at(mut self, path: &Path) -> Self {
        self.path = Some(Box::new(path.to_path_buf()));
        self
    }

    pub(crate) fn unsafe_at(path: &Path, error: anyhow::Error) -> Self {
        #[cfg(windows)]
        let path = error
            .downcast_ref::<crate::credentials::UnsafeWindowsPermissions>()
            .map_or(path, |error| error.0.as_path());
        #[cfg(not(windows))]
        let _ = error;
        Self::new("SETUP_UNSAFE_FILE").at(path)
    }

    pub(crate) fn uncertain(rollback: &'static str, backup: Option<PathBuf>) -> Self {
        Self {
            code: "SETUP_MCP_CONFIG_WRITE_FAILED",
            outcome: "unknown",
            rollback,
            backup_path: backup,
            recovery_directory: None,
            path: None,
            conflict: None,
        }
    }
}
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for ConfigError {}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManualConfig {
    pub tool_type: ToolType,
    pub suggested_target_path: PathBuf,
    pub executable: Executable,
    pub broker_file: PathBuf,
    pub snippet: String,
    pub restart_required: bool,
}

pub fn project_target(tool: ToolType, project: &std::path::Path) -> PathBuf {
    project.join(match tool {
        ToolType::Codex => ".codex/config.toml",
        ToolType::ClaudeCode => ".mcp.json",
    })
}

/// Check the setup paths that do not depend on the selected Node executable.
pub(crate) fn preflight_paths(
    tool: ToolType,
    project: &std::path::Path,
    target: &std::path::Path,
    broker_file: &std::path::Path,
    entry: &std::path::Path,
) -> Result<()> {
    storage::normalize_locations(tool, project, target, broker_file)?;
    storage::normalize_entry(entry)?;
    Ok(())
}

/// Read-only validation before Agent/token creation. The future credential path
/// is checked without requiring or creating a descriptor. Host formats stay here.
pub fn preflight(input: PlanInput, inspect_target: bool) -> Result<()> {
    let input = storage::normalize(input.tool_type, input)?;
    if inspect_target {
        let before = storage::read(&input.target)?;
        let proposed = format::entry(&input);
        let merged = match format::merge(input.tool, before.as_deref(), &proposed) {
            Ok((Change::Conflict, _)) => Err(ConfigError::new("SETUP_MCP_CONFIG_CONFLICT")),
            result => result,
        };
        let (_, after) = merged.map_err(|mut error| {
            if error.code == "SETUP_MCP_CONFIG_CONFLICT" {
                error.conflict = Some(Box::new(ConfigConflict {
                    target_path: input.target,
                    proposed,
                }));
            }
            error
        })?;
        if after.len() > LIMIT {
            return Err(ConfigError::new("SETUP_MCP_CONFIG_WRITE_FAILED"));
        }
    }
    Ok(())
}

/// Validate the same project/runtime/credential paths and render only DevLink.
/// This is guidance for manual review, including unverified Host versions; it writes nothing.
pub fn manual_config(input: PlanInput) -> Result<ManualConfig> {
    let normalized = storage::normalize(input.tool_type, input)?;
    storage::require_credentials(&normalized.broker)?;
    let (_, bytes) = format::merge(normalized.tool, None, &format::entry(&normalized))?;
    let snippet =
        String::from_utf8(bytes).map_err(|_| ConfigError::new("SETUP_MCP_CONFIG_WRITE_FAILED"))?;
    Ok(ManualConfig {
        tool_type: normalized.tool,
        suggested_target_path: normalized.target,
        executable: normalized.executable,
        broker_file: normalized.broker,
        snippet,
        restart_required: true,
    })
}

/// Use the same Windows path spelling as the generated Host entry.
pub fn host_path(path: &Path) -> String {
    storage::host_path(path)
}

#[derive(Clone)]
struct Normalized {
    canonical_command: PathBuf,
    canonical_script: PathBuf,
    tool: ToolType,
    root: PathBuf,
    target: PathBuf,
    broker: PathBuf,
    executable: Executable,
}
struct Prepared {
    public: ConfigPlan,
    input: Normalized,
    before: Option<Vec<u8>>,
    after: Vec<u8>,
}
/// A plan lives only in this provider instance. A new process must plan again.
pub struct McpConfig {
    host: VerifiedHost,
    plans: HashMap<String, Prepared>,
    order: VecDeque<String>,
    #[cfg(test)]
    fault: Option<tests::Fault>,
}
impl McpConfig {
    pub fn new(host: VerifiedHost) -> Self {
        Self {
            host,
            plans: HashMap::new(),
            order: VecDeque::new(),
            #[cfg(test)]
            fault: None,
        }
    }
    pub fn plan(&mut self, input: PlanInput) -> Result<ConfigPlan> {
        let input = storage::normalize(self.host.tool, input)?;
        let before = storage::read(&input.target)?;
        let (change, after) = format::merge(input.tool, before.as_deref(), &format::entry(&input))?;
        if after.len() > LIMIT {
            return Err(ConfigError::new("SETUP_MCP_CONFIG_WRITE_FAILED"));
        }
        let public = ConfigPlan {
            plan_id: uuid::Uuid::new_v4().to_string(),
            tool_type: input.tool,
            target_path: input.target.clone(),
            expected_digest: storage::digest(before.as_deref()),
            change,
            diff: if change == Change::Unchanged {
                None
            } else {
                Some(
                    serde_json::json!({"server":"devlink","change":change,"proposed":format::entry(&input)}),
                )
            },
            restart_required: matches!(change, Change::Create | Change::Merge),
        };
        if self.order.len() >= 32
            && let Some(expired) = self.order.pop_front()
        {
            self.plans.remove(&expired);
        }
        self.order.push_back(public.plan_id.clone());
        self.plans.insert(
            public.plan_id.clone(),
            Prepared {
                public: public.clone(),
                input,
                before,
                after,
            },
        );
        Ok(public)
    }
    pub fn inspect(&self, input: InspectionInput) -> Result<ConfigInspection> {
        Self::inspect_for(self.host.tool, input)
    }

    /// Status can recover the configured launch paths without running the entry.
    /// Normalize and compare the whole entry against the current profile afterward.
    pub(crate) fn inspect_configured(
        tool: ToolType,
        project: &Path,
        target: &Path,
        broker_file: &Path,
        node: Option<&Path>,
        entry: Option<&Path>,
    ) -> Result<InspectionStatus> {
        let (_, target, _) = storage::normalize_locations(tool, project, target, broker_file)?;
        let Some(bytes) = storage::read(&target)? else {
            return Ok(InspectionStatus::Missing);
        };
        let Some(configured) = format::configured_entry(tool, &bytes)? else {
            return Ok(InspectionStatus::Missing);
        };
        let Some(command) = configured["command"].as_str() else {
            return Ok(InspectionStatus::Conflict);
        };
        let Some(script) = configured["args"]
            .get(0)
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(InspectionStatus::Conflict);
        };
        let node = node.unwrap_or_else(|| Path::new(command));
        let script = entry.unwrap_or_else(|| Path::new(script));
        let inspection = Self::inspect_for(
            tool,
            InspectionInput {
                tool_type: tool,
                target_path: target,
                broker_file: broker_file.into(),
                executable: Executable {
                    command: node.into(),
                    args: vec![
                        storage::host_path(script),
                        "--transport".into(),
                        "stdio".into(),
                    ],
                },
            },
        )?;
        Ok(inspection.status)
    }

    /// Read-only comparison is independent of the Host version's apply support.
    pub(crate) fn inspect_for(tool: ToolType, input: InspectionInput) -> Result<ConfigInspection> {
        let root = storage::project_from_target(input.tool_type, &input.target_path)?;
        let target = input.target_path.clone();
        let input = match storage::normalize(
            tool,
            PlanInput {
                tool_type: input.tool_type,
                project_path: root,
                target_path: input.target_path,
                broker_file: input.broker_file,
                executable: input.executable,
            },
        ) {
            Ok(input) => input,
            Err(error)
                if [
                    "SETUP_UNSAFE_FILE",
                    "SETUP_TOOL_UNAVAILABLE",
                    "SETUP_PROJECT_INVALID",
                ]
                .contains(&error.code) =>
            {
                return Ok(ConfigInspection {
                    status: InspectionStatus::Unknown,
                    target_path: target,
                });
            }
            Err(error) => return Err(error),
        };
        let status = match storage::read(&input.target) {
            Ok(None) => InspectionStatus::Missing,
            Ok(Some(bytes)) => {
                match format::merge(input.tool, Some(&bytes), &format::entry(&input)) {
                    Ok((Change::Unchanged, _)) => InspectionStatus::Applied,
                    Ok((Change::Merge | Change::Create, _)) => InspectionStatus::Missing,
                    Ok((Change::Conflict, _)) | Err(_) => InspectionStatus::Conflict,
                }
            }
            Err(_) => InspectionStatus::Unknown,
        };
        Ok(ConfigInspection {
            status,
            target_path: input.target,
        })
    }
    pub fn apply(
        &mut self,
        plan_id: &str,
        expected_digest: Option<&str>,
    ) -> Result<ConfigApplyResult> {
        let prepared = self
            .plans
            .get(plan_id)
            .ok_or_else(|| ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"))?;
        if prepared.public.expected_digest.as_deref() != expected_digest
            || prepared.public.change == Change::Conflict
        {
            return Err(ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"));
        }
        // Revalidate paths/executables/descriptor; never trust an old plan's filesystem.
        storage::revalidate(&prepared.input)?;
        storage::require_credentials(&prepared.input.broker)?;
        let _lock = storage::lock(&prepared.input)?;
        let current = storage::read(&prepared.input.target)?;
        if current.as_deref() == Some(prepared.after.as_slice()) {
            return Ok(ConfigApplyResult {
                status: ApplyStatus::Unchanged,
                target_path: prepared.input.target.clone(),
                backup_path: None,
                restart_required: false,
            });
        }
        if storage::digest(current.as_deref()) != prepared.public.expected_digest {
            return Err(ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"));
        }
        let backup = storage::backup(&prepared.input, current.as_deref())?;
        #[cfg(test)]
        tests::before_commit(self.fault, &prepared.input).map_err(|mut e| {
            e.backup_path = backup.clone();
            e
        })?;
        // A second digest check also catches edits made during backup preparation.
        if storage::digest(
            storage::read(&prepared.input.target)
                .map_err(|mut e| {
                    e.backup_path = backup.clone();
                    e
                })?
                .as_deref(),
        ) != prepared.public.expected_digest
        {
            let mut error = ConfigError::new("SETUP_MCP_CONFIG_CONFLICT");
            error.backup_path = backup;
            return Err(error);
        }
        if let Err(mut error) = storage::write_version(
            &prepared.input.target,
            &prepared.after,
            prepared.before.is_none(),
            prepared.public.expected_digest.as_deref(),
        ) {
            if error.outcome == "unknown" {
                return Err(self.restore_owned(prepared, backup));
            }
            error.backup_path = backup;
            return Err(error);
        }
        #[cfg(test)]
        tests::after_commit(self.fault, &prepared.input);
        let checked = storage::read(&prepared.input.target);
        let valid = checked
            .as_ref()
            .ok()
            .and_then(|v| v.as_deref())
            .is_some_and(|bytes| {
                matches!(
                    format::merge(
                        prepared.input.tool,
                        Some(bytes),
                        &format::entry(&prepared.input)
                    ),
                    Ok((Change::Unchanged, _))
                )
            });
        #[cfg(test)]
        let valid = valid && !tests::fail_inspection(self.fault);
        if !valid {
            return Err(self.restore_owned(prepared, backup));
        }
        Ok(ConfigApplyResult {
            status: ApplyStatus::Applied,
            target_path: prepared.input.target.clone(),
            backup_path: backup,
            restart_required: true,
        })
    }
    fn restore_owned(&self, prepared: &Prepared, backup: Option<PathBuf>) -> ConfigError {
        match storage::read(&prepared.input.target) {
            Ok(Some(bytes)) if bytes == prepared.after => {}
            Ok(_) => return ConfigError::uncertain("conflict", backup),
            Err(_) => return ConfigError::uncertain("failed", backup),
        }
        #[cfg(test)]
        if matches!(self.fault, Some(tests::Fault::RestoreFailure)) {
            return ConfigError::uncertain("failed", backup);
        }
        let expected = storage::digest(Some(&prepared.after));
        let restored = match &prepared.before {
            Some(bytes) => {
                storage::write_version(&prepared.input.target, bytes, false, expected.as_deref())
            }
            // Portable remove-by-path cannot atomically compare and delete against an editor.
            // Keep the newly written file and report uncertainty rather than risk deleting an edit.
            None => return ConfigError::uncertain("not_needed", backup),
        };
        if let Err(error) = restored {
            return ConfigError::uncertain(
                if error.code == "SETUP_MCP_CONFIG_CONFLICT" {
                    "conflict"
                } else {
                    "failed"
                },
                backup,
            );
        }
        ConfigError {
            code: "SETUP_MCP_CONFIG_WRITE_FAILED",
            outcome: "not_applied",
            rollback: "restored",
            backup_path: backup,
            recovery_directory: None,
            path: None,
            conflict: None,
        }
    }
}
