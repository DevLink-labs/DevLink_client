//! Private tool selection storage. Host configuration paths and formats belong to McpConfig.
use crate::{
    agent_profile,
    config::{self, Config},
    private_file,
    sessions::ToolType,
    teams,
    tool_discovery::SelectedTool,
    user_auth,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectionBinding {
    pub origin: String,
    pub user_id: u64,
    pub team_id: u64,
    pub project_root: PathBuf,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredTool {
    schema_version: u8,
    binding: SelectionBinding,
    selected: SelectedTool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionSnapshot {
    binding: SelectionBinding,
    stored: Option<StoredTool>,
    pub bound_tool: Option<ToolType>,
}
impl SelectionSnapshot {
    pub fn preferred(&self) -> Option<&SelectedTool> {
        self.stored
            .as_ref()
            .filter(|s| s.binding == self.binding)
            .map(|s| &s.selected)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Storage,
    Busy,
    BindingConflict,
    SessionConflict,
    SelectionConflict,
}
#[derive(Debug)]
pub struct Failure {
    pub kind: FailureKind,
    pub file: PathBuf,
    pub mutation_uncertain: bool,
}
fn failure(kind: FailureKind, file: PathBuf) -> Failure {
    Failure {
        kind,
        file,
        mutation_uncertain: false,
    }
}
pub fn path(config_path: &Path) -> PathBuf {
    config::state_directory(config_path).join("selected-tool.json")
}

fn inspect(config_path: &Path, binding: &SelectionBinding) -> Result<SelectionSnapshot, Failure> {
    let storage = |file| failure(FailureKind::Storage, file);
    if !binding.project_root.is_absolute()
        || binding.project_root.canonicalize().ok().as_ref() != Some(&binding.project_root)
        || !binding.project_root.is_dir()
    {
        return Err(failure(
            FailureKind::BindingConflict,
            binding.project_root.clone(),
        ));
    }
    let current = Config::load(config_path)
        .and_then(|config| user_auth::origin(&config))
        .map_err(|_| storage(config_path.to_path_buf()))?;
    let selected_team = teams::stored_selection(config_path)
        .map_err(|_| storage(teams::selection_path(config_path)))?;
    if !user_auth::setup_session_matches(config_path, &binding.origin, binding.user_id)
        .map_err(|_| storage(user_auth::session_path(config_path)))?
    {
        return Err(failure(
            FailureKind::SessionConflict,
            user_auth::session_path(config_path),
        ));
    }
    if current.as_str() != binding.origin
        || !selected_team.is_some_and(|s| {
            s.origin == binding.origin
                && s.user_id == binding.user_id
                && s.team_id == binding.team_id
        })
    {
        return Err(failure(
            FailureKind::SessionConflict,
            config_path.to_path_buf(),
        ));
    }
    let profile = agent_profile::setup_project_binding(config_path)
        .map_err(|_| storage(agent_profile::path(config_path)))?;
    let bound_tool = if let Some(profile) = profile {
        let root = agent_profile::project(Path::new(&profile.root)).map_err(|_| {
            failure(
                FailureKind::BindingConflict,
                agent_profile::path(config_path),
            )
        })?;
        if Path::new(&root) != binding.project_root
            || (profile.tool.is_some()
                && (profile.origin != binding.origin
                    || profile.user_id != binding.user_id
                    || profile.team_id != binding.team_id))
        {
            return Err(failure(
                FailureKind::BindingConflict,
                agent_profile::path(config_path),
            ));
        }
        profile.tool
    } else {
        None
    };
    let stored: Option<StoredTool> =
        private_file::read(&path(config_path), 16_384).map_err(|_| storage(path(config_path)))?;
    if stored.as_ref().is_some_and(|s| {
        s.schema_version != 1 || !s.binding.project_root.is_absolute() || !s.selected.valid_stored()
    }) {
        return Err(storage(path(config_path)));
    }
    Ok(SelectionSnapshot {
        binding: binding.clone(),
        stored,
        bound_tool,
    })
}
pub fn read(config_path: &Path, binding: &SelectionBinding) -> Result<SelectionSnapshot, Failure> {
    let _account = account_lock(config_path)?;
    let lock = config::state_directory(config_path).join("control.lock");
    let _guard = config::try_lock(&lock)
        .map_err(|_| failure(FailureKind::Storage, lock.clone()))?
        .ok_or_else(|| failure(FailureKind::Busy, lock))?;
    inspect(config_path, binding)
}
/// Recheck after probing or user input; never overwrite a concurrent selection or Agent binding.
pub fn publish(
    config_path: &Path,
    snapshot: &SelectionSnapshot,
    selected: &SelectedTool,
) -> Result<bool, Failure> {
    let _account = account_lock(config_path)?;
    let lock = config::state_directory(config_path).join("control.lock");
    let _guard = config::try_lock(&lock)
        .map_err(|_| failure(FailureKind::Storage, lock.clone()))?
        .ok_or_else(|| failure(FailureKind::Busy, lock))?;
    let current = inspect(config_path, &snapshot.binding)?;
    if current != *snapshot
        || current
            .bound_tool
            .is_some_and(|tool| tool != selected.tool_type)
    {
        return Err(failure(FailureKind::SelectionConflict, path(config_path)));
    }
    let stored = StoredTool {
        schema_version: 1,
        binding: snapshot.binding.clone(),
        selected: selected.clone(),
    };
    if current.stored.as_ref() == Some(&stored) {
        return Ok(true);
    }
    private_file::write(&path(config_path), &stored, 16_384).map_err(|_| Failure {
        kind: FailureKind::Storage,
        file: path(config_path),
        mutation_uncertain: true,
    })?;
    Ok(false)
}

fn account_lock(config_path: &Path) -> Result<std::fs::File, Failure> {
    let lock = config::state_directory(config_path).join("account.lock");
    config::try_lock(&lock)
        .map_err(|_| failure(FailureKind::Storage, lock.clone()))?
        .ok_or_else(|| failure(FailureKind::Busy, lock))
}
