//! Read-only project preparation. Local paths never belong to registration metadata.
use crate::{agent_profile, config};
use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Clone, Default)]
pub struct ProjectOptions {
    pub path: Option<PathBuf>,
    pub project_name: Option<String>,
    pub agent_name: Option<String>,
    pub role_description: Option<String>,
}

/// Only this metadata may be used in a central registration request.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AgentMetadata {
    pub project_name: String,
    #[serde(rename = "name")]
    pub agent_name: String,
    pub role_description: Option<String>,
}

// Deliberately not Serialize: a registration request must select public metadata.
#[derive(Clone, Debug)]
pub struct PreparedProject {
    pub root: PathBuf,
    pub metadata: AgentMetadata,
    pub reused: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    InvalidPath,
    InvalidMetadata,
    BindingConflict,
    Storage,
    Busy,
}

#[derive(Debug)]
pub struct Failure {
    pub kind: FailureKind,
    pub file: Option<PathBuf>,
    pub field: &'static str,
}

fn failure(kind: FailureKind, file: Option<PathBuf>, field: &'static str) -> Failure {
    Failure { kind, file, field }
}

/// Resolve only the directory, before authentication or configuration creation.
/// Never creates a directory. Display names are not checked here: a saved profile
/// may still supply them in `prepare`, so a root without a folder name must not
/// stop setup before sign-in.
pub fn resolve_root(options: &ProjectOptions) -> Result<PathBuf, Failure> {
    let requested = options
        .path
        .clone()
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(|_| failure(FailureKind::InvalidPath, None, "project.path"))?;
    agent_profile::project(&requested)
        .map(PathBuf::from)
        .map_err(|_| failure(FailureKind::InvalidPath, Some(requested), "project.path"))
}

/// Resolve the directory and the explicit or folder-derived names, validated.
pub fn resolve(options: &ProjectOptions) -> Result<PreparedProject, Failure> {
    let prepared = defaults(options)?;
    validate_metadata(&prepared.metadata)?;
    Ok(prepared)
}

/// Unvalidated defaults; callers validate after any saved metadata is merged.
fn defaults(options: &ProjectOptions) -> Result<PreparedProject, Failure> {
    let root = resolve_root(options)?;
    let folder = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    // Keep defaults within the existing central UTF-16 limits without splitting a character.
    let metadata = AgentMetadata {
        project_name: options
            .project_name
            .clone()
            .unwrap_or_else(|| truncate(folder, 120)),
        agent_name: options
            .agent_name
            .clone()
            .unwrap_or_else(|| truncate(folder, 100)),
        role_description: options.role_description.clone(),
    };
    Ok(PreparedProject {
        root,
        metadata,
        reused: false,
    })
}

fn truncate(value: &str, limit: usize) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|c| {
            units += c.len_utf16();
            units <= limit
        })
        .collect()
}

fn validate_metadata(metadata: &AgentMetadata) -> Result<(), Failure> {
    for (value, limit, field) in [
        (metadata.project_name.as_str(), 120, "project.name"),
        (metadata.agent_name.as_str(), 100, "agent.name"),
    ] {
        if value.trim().is_empty() || value.encode_utf16().count() > limit {
            return Err(failure(FailureKind::InvalidMetadata, None, field));
        }
    }
    if metadata
        .role_description
        .as_ref()
        .is_some_and(|s| s.encode_utf16().count() > 255)
    {
        return Err(failure(
            FailureKind::InvalidMetadata,
            None,
            "agent.roleDescription",
        ));
    }
    Ok(())
}

/// Compare under the same control lock used by profile writers. No profile is saved here.
pub fn prepare(config_path: &Path, options: &ProjectOptions) -> Result<PreparedProject, Failure> {
    let mut prepared = defaults(options)?;
    let lock = config::state_directory(config_path).join("control.lock");
    let _guard = config::try_lock(&lock)
        .map_err(|_| failure(FailureKind::Storage, Some(lock.clone()), "project.path"))?
        .ok_or_else(|| failure(FailureKind::Busy, Some(lock), "project.path"))?;
    if let Some(existing) = agent_profile::setup_project_binding(config_path).map_err(|_| {
        failure(
            FailureKind::Storage,
            Some(agent_profile::path(config_path)),
            "project.path",
        )
    })? {
        let existing_root = agent_profile::project(Path::new(&existing.root)).map_err(|_| {
            failure(
                FailureKind::BindingConflict,
                Some(agent_profile::path(config_path)),
                "project.path",
            )
        })?;
        if Path::new(&existing_root) != prepared.root {
            return Err(failure(
                FailureKind::BindingConflict,
                Some(agent_profile::path(config_path)),
                "project.path",
            ));
        }
        if let Some(metadata) = existing.metadata {
            // Reuse saved display names; a folder rename must not rename a central Agent.
            if options.project_name.is_none() {
                prepared.metadata.project_name = metadata.project_name;
            }
            if options.agent_name.is_none() {
                prepared.metadata.agent_name = metadata.agent_name;
            }
            if options.role_description.is_none() {
                prepared.metadata.role_description = metadata.role_description;
            }
        }
        prepared.reused = true;
    }
    // Validate once, after saved names had the chance to fill an empty folder default.
    validate_metadata(&prepared.metadata)?;
    Ok(prepared)
}
