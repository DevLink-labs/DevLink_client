//! Setup configuration preparation. No prompts, network, credential rotation, or deletion.
use crate::{
    agent_profile,
    config::{self, Config, ConfigValidationError},
    credentials, private_file, teams, user_auth,
};
use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

const LIMIT: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    InvalidPath,
    InvalidConfig,
    ServerConflict,
    UnsafeFile,
    ConfigBusy,
    BrokerBusy,
    Storage,
}

#[derive(Debug)]
pub struct Failure {
    pub kind: FailureKind,
    pub file: PathBuf,
    pub field: Option<&'static str>,
    pub created: bool,
    pub mutation_uncertain: bool,
}

impl Failure {
    fn new(kind: FailureKind, file: &Path) -> Self {
        Self {
            kind,
            file: file.to_path_buf(),
            field: None,
            created: false,
            mutation_uncertain: false,
        }
    }

    fn field(mut self, field: &'static str) -> Self {
        self.field = Some(field);
        self
    }
}

pub struct PreparedConfig {
    pub config: Config,
    pub created: bool,
    /// Snapshot at preparation time, not a live status. A held broker lock forbids config writes.
    pub broker_running: bool,
}

/// Validate before creating a profile, then serialize with the existing control/broker locks.
pub fn prepare(path: &Path) -> Result<PreparedConfig, Failure> {
    prepare_with_server(path, None)
}

/// Read-only validation before collecting a password or starting mutations.
pub fn preflight_server(path: &Path, server_url: Option<&str>) -> Result<(), Failure> {
    validate_path(path)?;
    select_server(path, read_config(path)?.as_ref(), server_url)?;
    inspect_snapshots(path)
}

fn select_server(
    path: &Path,
    existing: Option<&Config>,
    requested: Option<&str>,
) -> Result<Config, Failure> {
    let mut config = existing.cloned().unwrap_or_default();
    if let Some(url) = requested {
        config.server_url = url.to_owned();
        validate_config(&config, path)?;
        if let Some(existing) = existing {
            if user_auth::origin(existing).ok() != user_auth::origin(&config).ok() {
                return Err(Failure::new(FailureKind::ServerConflict, path).field("serverUrl"));
            }
            // Preserve the existing spelling and other profile settings.
            return Ok(existing.clone());
        }
    }
    Ok(config)
}

pub fn prepare_with_server(
    path: &Path,
    server_url: Option<&str>,
) -> Result<PreparedConfig, Failure> {
    prepare_directory(path)?;
    prepare_profile(path, server_url)
}

/// Prepare the profile and reserve it for this setup run. This lock is always
/// taken before control/broker locks and is used only by setup orchestration.
pub fn prepare_for_run(
    path: &Path,
    server_url: Option<&str>,
) -> Result<(PreparedConfig, fs::File), Failure> {
    prepare_directory(path)?;
    let lock_path = config::state_directory(path).join("setup.lock");
    let lock = config::try_lock(&lock_path)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, &lock_path))?
        .ok_or_else(|| Failure::new(FailureKind::ConfigBusy, &lock_path))?;
    let prepared = prepare_profile(path, server_url)?;
    Ok((prepared, lock))
}

fn prepare_directory(path: &Path) -> Result<(), Failure> {
    validate_path(path)?;
    let parent = config::directory(path);
    let parent_missing = matches!(fs::symlink_metadata(parent), Err(error) if error.kind() == io::ErrorKind::NotFound);
    credentials::ensure_safe_parent_directory(parent)
        .map_err(|_| Failure::new(FailureKind::Storage, parent))?;
    if parent_missing {
        credentials::set_private_permissions(parent)
            .map_err(|_| Failure::new(FailureKind::Storage, parent))?;
    }
    Ok(())
}

fn prepare_profile(path: &Path, server_url: Option<&str>) -> Result<PreparedConfig, Failure> {
    let state = config::state_directory(path);
    let control_path = state.join("control.lock");
    let _control = config::try_lock(&control_path)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, &control_path))?
        .ok_or_else(|| Failure::new(FailureKind::ConfigBusy, &control_path))?;
    let broker_path = state.join("broker.lock");
    let broker = config::try_lock(&broker_path)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, &broker_path))?;
    let broker_running = broker.is_none();

    // Validate existing private snapshots before creating a missing config. These are
    // local storage checks, never proof of central identity or membership.
    inspect_snapshots(path)?;
    let existing = read_config(path)?;
    let selected = select_server(path, existing.as_ref(), server_url)?;
    let created = if existing.is_none() {
        if broker_running {
            return Err(Failure::new(FailureKind::BrokerBusy, path));
        }
        config::initialize_config_missing(path, true, &selected).map_err(|failure| {
            let mut error = Failure::new(FailureKind::Storage, path);
            error.created = failure.published;
            error.mutation_uncertain = failure.published;
            error
        })?
    } else {
        false
    };
    let config = match existing {
        Some(config) => config,
        None if created => selected,
        None => read_config(path)
            .map_err(|mut error| {
                error.created = created;
                error
            })?
            .ok_or_else(|| {
                let mut error = Failure::new(FailureKind::Storage, path);
                error.created = created;
                error
            })
            .and_then(|config| select_server(path, Some(&config), server_url))?,
    };
    Ok(PreparedConfig {
        config,
        created,
        broker_running,
    })
}

fn validate_path(path: &Path) -> Result<(), Failure> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path.components().any(|part| {
            matches!(part, Component::Normal(name)
            if name.to_str().is_some_and(|s| if cfg!(windows) {
                s.eq_ignore_ascii_case(".devlink-runtime")
            } else { s == ".devlink-runtime" }))
        })
    {
        return Err(Failure::new(FailureKind::InvalidPath, path));
    }
    // Inspect every ancestor before mkdir/open; canonicalize alone would hide links.
    for parent in path.ancestors().skip(1) {
        match fs::symlink_metadata(parent) {
            Ok(_) => credentials::verify_parent_safety(parent)
                .map_err(|_| Failure::new(FailureKind::UnsafeFile, parent))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(Failure::new(FailureKind::Storage, parent)),
        }
    }
    credentials::ensure_safe_target_if_exists(path)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, path))?;
    let state = config::state_directory(path);
    if fs::symlink_metadata(&state).is_ok() {
        credentials::verify_parent_safety(&state)
            .map_err(|_| Failure::new(FailureKind::UnsafeFile, &state))?;
    }
    Ok(())
}

fn read_config(path: &Path) -> Result<Option<Config>, Failure> {
    // Config has no secrets, so others may read it. Its serverUrl decides where the
    // password is sent, so only the current user and trusted principals may write it.
    if !credentials::ensure_safe_target_if_exists(path)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, path))?
    {
        return Ok(None);
    }
    credentials::verify_trusted_writer_permissions(path)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, path))?;
    let bytes = private_file::read_bounded(path, LIMIT).map_err(|error| {
        let io = error.chain().any(|cause| cause.is::<io::Error>());
        Failure::new(
            if io {
                FailureKind::Storage
            } else {
                FailureKind::UnsafeFile
            },
            path,
        )
    })?;
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::new(FailureKind::InvalidConfig, path).field("$"))?;
    validate_config(&config, path)?;
    Ok(Some(config))
}

fn validate_config(config: &Config, path: &Path) -> Result<(), Failure> {
    let invalid = |field| Failure::new(FailureKind::InvalidConfig, path).field(field);
    // origin validates Config first, then enforces HTTPS/loopback. Typed fields
    // avoid copying its validation rules or exposing the raw config values.
    user_auth::origin(config).map_err(|error| {
        invalid(
            error
                .downcast_ref::<ConfigValidationError>()
                .map_or("serverUrl", |error| error.field),
        )
    })?;
    Ok(())
}

fn inspect_snapshots(path: &Path) -> Result<(), Failure> {
    let file = credentials::path(path);
    if credentials::ensure_safe_target_if_exists(&file)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, &file))?
    {
        credentials::load(path).map_err(|_| Failure::new(FailureKind::UnsafeFile, &file))?;
    }
    for file in [
        user_auth::session_path(path),
        teams::selection_path(path),
        agent_profile::path(path),
        crate::setup_tools::path(path),
    ] {
        private_file::read::<serde_json::Value>(&file, LIMIT)
            .map_err(|_| Failure::new(FailureKind::UnsafeFile, &file))?;
    }
    Ok(())
}
