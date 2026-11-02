//! Setup configuration preparation. No prompts, network, credential rotation, or deletion.
use crate::{
    agent_profile,
    config::{self, Config},
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
    /// A held broker lock can mean running or starting; it forbids configuration writes.
    pub broker_running: bool,
}

/// Validate before creating a profile, then serialize with the existing control/broker locks.
pub fn prepare(path: &Path) -> Result<PreparedConfig, Failure> {
    validate_path(path)?;
    let parent = config::directory(path);
    let parent_missing = matches!(fs::symlink_metadata(parent), Err(error) if error.kind() == io::ErrorKind::NotFound);
    credentials::ensure_safe_parent_directory(parent)
        .map_err(|_| Failure::new(FailureKind::Storage, parent))?;
    if parent_missing {
        credentials::set_private_permissions(parent)
            .map_err(|_| Failure::new(FailureKind::Storage, parent))?;
    }
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
    let created = if existing.is_none() {
        if broker_running {
            return Err(Failure::new(FailureKind::BrokerBusy, path));
        }
        config::initialize_missing(path, true).map_err(|_| {
            let mut error = Failure::new(FailureKind::Storage, path);
            error.mutation_uncertain = true;
            error
        })?
    } else {
        false
    };
    let config = match existing {
        Some(config) => config,
        None => read_config(path)
            .map_err(|mut error| {
                error.created = created;
                error
            })?
            .ok_or_else(|| {
                let mut error = Failure::new(FailureKind::Storage, path);
                error.created = created;
                error
            })?,
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
    let Some(bytes) = private_file::read_bytes(path, LIMIT)
        .map_err(|_| Failure::new(FailureKind::UnsafeFile, path))?
    else {
        return Ok(None);
    };
    let config: Config = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::new(FailureKind::InvalidConfig, path).field("$"))?;
    validate_config(&config, path)?;
    Ok(Some(config))
}

fn validate_config(config: &Config, path: &Path) -> Result<(), Failure> {
    let invalid = |field| Failure::new(FailureKind::InvalidConfig, path).field(field);
    // Reuse provider validators without forwarding their raw messages or values.
    if user_auth::origin(config).is_err() {
        // Config::validate also checks other fields. Identify the field using a
        // default copy so the public diagnostic remains useful and secret-free.
        let server_only = Config {
            server_url: config.server_url.clone(),
            ..Config::default()
        };
        if user_auth::origin(&server_only).is_err() {
            return Err(invalid("serverUrl"));
        }
    }
    let agent_only = Config {
        agent: config.agent.clone(),
        ..Config::default()
    };
    if agent_only.validate().is_err() {
        return Err(invalid("agent"));
    }
    let reconnect_only = Config {
        reconnect_initial_ms: config.reconnect_initial_ms,
        reconnect_max_ms: config.reconnect_max_ms,
        ..Config::default()
    };
    if reconnect_only.validate().is_err() {
        return Err(invalid("reconnectInitialMs/reconnectMaxMs"));
    }
    let timeout_only = Config {
        auth_timeout_ms: config.auth_timeout_ms,
        ..Config::default()
    };
    if timeout_only.validate().is_err() {
        return Err(invalid("authTimeoutMs"));
    }
    if config.local_port == 80 {
        return Err(invalid("localPort"));
    }
    config.validate().map_err(|_| invalid("$"))?;
    Ok(())
}

fn inspect_snapshots(path: &Path) -> Result<(), Failure> {
    for file in [
        credentials::path(path),
        user_auth::session_path(path),
        teams::selection_path(path),
        agent_profile::path(path),
    ] {
        let snapshot = private_file::read::<serde_json::Value>(&file, LIMIT)
            .map_err(|_| Failure::new(FailureKind::UnsafeFile, &file))?;
        if snapshot.is_some() && file == credentials::path(path) {
            credentials::load(path).map_err(|_| Failure::new(FailureKind::UnsafeFile, &file))?;
        }
    }
    Ok(())
}
