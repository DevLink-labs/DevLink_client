//! Shared broker lifecycle execution. No terminal I/O and no PID-based process control.
use crate::{
    broker,
    config::{self, Config, state_directory},
    credentials,
    local::{self, Health, Runtime},
};
use anyhow::Result;
use serde::Serialize;
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::time::{Instant, sleep};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const STOP_TIMEOUT: Duration = Duration::from_secs(8);

#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    InvalidConfig,
    ControlLockFailed,
    ConfigBusy,
    BrokerLockFailed,
    BrokerBusy,
    Storage,
    Unhealthy,
    TokenUnavailable,
    SpawnFailed,
    ChildExited,
    StartupTimedOut,
    ShutdownFailed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    NotApplied,
    Unknown,
}
#[derive(Debug)]
pub struct Failure {
    pub kind: FailureKind,
    pub outcome: Outcome,
    pub file: PathBuf,
    // Preserve existing CLI diagnostics. setup maps kind/outcome/file only.
    diagnostic: Option<String>,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(diagnostic) = &self.diagnostic {
            return f.write_str(diagnostic);
        }
        f.write_str(match self.kind {
            FailureKind::InvalidConfig => "Cannot load broker configuration",
            FailureKind::ControlLockFailed => "Cannot lock broker control file",
            FailureKind::ConfigBusy => "Another DevLink operation is running",
            FailureKind::BrokerLockFailed => "Cannot inspect broker lock",
            FailureKind::BrokerBusy => "Stop the broker before changing Agent credentials (running or starting)",
            FailureKind::Storage => "Cannot inspect broker runtime files",
            FailureKind::Unhealthy => "Broker identity or health could not be verified",
            FailureKind::TokenUnavailable => "Cannot prepare the central Agent token",
            FailureKind::SpawnFailed => "Cannot spawn broker process",
            FailureKind::ChildExited => "Broker exited during startup; check configuration and local port, or run devlink broker in foreground",
            FailureKind::StartupTimedOut => "Broker startup timed out; child process cleaned up",
            FailureKind::ShutdownFailed => "Broker shutdown timed out; process was not force-killed",
        })
    }
}
impl std::error::Error for Failure {}
fn failure(kind: FailureKind, file: &Path, outcome: Outcome) -> Failure {
    Failure {
        kind,
        outcome,
        file: file.to_path_buf(),
        diagnostic: None,
    }
}
fn classify(
    error: anyhow::Error,
    kind: FailureKind,
    file: &Path,
    outcome: Outcome,
) -> anyhow::Error {
    let mut fault = failure(kind, file, outcome);
    fault.diagnostic = Some(error.to_string());
    error.context(fault)
}

#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Status {
    Stopped,
    Running { health: Health },
}
#[derive(Serialize)]
pub struct StartResult {
    pub reused: bool,
    pub health: Health,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopResult {
    AlreadyStopped,
    Stopped,
}

/// Observe the OS lock, then authenticate service/version/PID/instance identity.
/// Stale discovery cleanup follows local::running's existing locked policy.
pub async fn status(path: &Path) -> Result<Status> {
    if !local::running(path).map_err(|e| {
        classify(
            e,
            FailureKind::Storage,
            &Runtime::path(path),
            Outcome::NotApplied,
        )
    })? {
        return Ok(Status::Stopped);
    }
    let runtime = Runtime::load(path).map_err(|e| {
        classify(
            e,
            FailureKind::Storage,
            &Runtime::path(path),
            Outcome::NotApplied,
        )
    })?;
    let health = runtime.health().await.map_err(|e| {
        classify(
            e,
            FailureKind::Unhealthy,
            &Runtime::path(path),
            Outcome::NotApplied,
        )
    })?;
    Ok(Status::Running { health })
}

fn control_lock(path: &Path) -> Result<File> {
    let lock = state_directory(path).join("control.lock");
    config::try_lock(&lock)
        .map_err(|e| {
            classify(
                e,
                FailureKind::ControlLockFailed,
                &lock,
                Outcome::NotApplied,
            )
        })?
        .ok_or_else(|| failure(FailureKind::ConfigBusy, &lock, Outcome::NotApplied).into())
}

/// Keep both locks until the credential/profile mutation completes. A held lock,
/// even without runtime.json, requires an explicit stop; never kill a discovered PID.
pub struct StoppedGuard {
    _control: File,
    _broker: File,
}
pub fn require_stopped(path: &Path) -> Result<StoppedGuard> {
    let directory = state_directory(path);
    credentials::ensure_safe_parent_directory(&directory)?;
    let lock = directory.join("control.lock");
    if std::fs::symlink_metadata(&lock).is_ok() {
        credentials::verify_path_safety(&lock).map_err(|e| {
            classify(
                e,
                FailureKind::ControlLockFailed,
                &lock,
                Outcome::NotApplied,
            )
        })?;
    }
    let control = control_lock(path)?;
    let lock = directory.join("broker.lock");
    let broker = config::try_lock(&lock)
        .map_err(|e| classify(e, FailureKind::BrokerLockFailed, &lock, Outcome::NotApplied))?
        .ok_or_else(|| failure(FailureKind::BrokerBusy, &lock, Outcome::NotApplied))?;
    Ok(StoppedGuard {
        _control: control,
        _broker: broker,
    })
}

pub fn startup_timeout(path: &Path) -> Duration {
    Duration::from_secs(8)
        + credentials::startup_validation_allowance()
        + crate::agent_profile::startup_allowance(path)
}

/// The caller supplies the CLI executable; current_exe in a library can be a test
/// harness or another application. Arguments and detached stdio are fixed here.
pub async fn start(path: &Path, executable: &Path) -> Result<StartResult> {
    Config::load(path)
        .map_err(|e| classify(e, FailureKind::InvalidConfig, path, Outcome::NotApplied))?;
    let _control = control_lock(path)?;
    if let Status::Running { health } = status(path).await? {
        return Ok(StartResult {
            reused: true,
            health,
        });
    }
    broker::central_token_for(path).map_err(|e| {
        classify(
            e,
            FailureKind::TokenUnavailable,
            &crate::agent_profile::path(path),
            Outcome::NotApplied,
        )
    })?;
    let mut command = Command::new(executable);
    command
        .arg("--config")
        .arg(path)
        .arg("broker")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        prevent_stdio_inheritance()
            .map_err(|e| classify(e, FailureKind::SpawnFailed, path, Outcome::NotApplied))?;
        command.creation_flags(0x00000008 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|e| {
        classify(
            anyhow::Error::new(e).context("Cannot spawn broker process"),
            FailureKind::SpawnFailed,
            path,
            Outcome::NotApplied,
        )
    })?;
    let mut owned = StartingChild {
        child: &mut child,
        armed: true,
    };
    let health = wait_for_start(path, owned.child, startup_timeout(path)).await?;
    owned.armed = false; // std::process::Child drop deliberately leaves a ready broker running.
    Ok(StartResult {
        reused: false,
        health,
    })
}

async fn wait_for_start(path: &Path, child: &mut Child, timeout: Duration) -> Result<Health> {
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .map_err(|e| classify(e.into(), FailureKind::ChildExited, path, Outcome::Unknown))?
            .is_some()
        {
            return Err(failure(FailureKind::ChildExited, path, Outcome::Unknown).into());
        }
        // Probing broker.lock before runtime publication can compete with the
        // child's initial acquisition. Verify the published child first.
        if let Ok(runtime) = Runtime::load(path)
            && runtime.pid == child.id()
            && let Ok(health) = runtime.health().await
            && local::running(path).map_err(|error| {
                classify(
                    error,
                    FailureKind::Storage,
                    &state_directory(path).join("broker.lock"),
                    Outcome::Unknown,
                )
            })?
        {
            return Ok(health);
        }
        if Instant::now() >= deadline {
            return Err(failure(FailureKind::StartupTimedOut, path, Outcome::Unknown).into());
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Only a Child handle obtained by our spawn is ever force-cleaned up. This also
/// handles errors or cancellation while waiting, before releasing control.lock.
struct StartingChild<'a> {
    child: &'a mut Child,
    armed: bool,
}
impl Drop for StartingChild<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Graceful authenticated stop only. An unhealthy/replaced broker is never killed.
pub async fn stop(path: &Path) -> Result<StopResult> {
    let directory = state_directory(path);
    if std::fs::symlink_metadata(&directory).is_ok() {
        credentials::verify_parent_safety(&directory)
            .map_err(|e| classify(e, FailureKind::Storage, &directory, Outcome::NotApplied))?;
    }
    if !directory.exists() {
        return Ok(StopResult::AlreadyStopped);
    }
    let _control = control_lock(path)?;
    if !local::running(path)
        .map_err(|e| classify(e, FailureKind::Storage, &directory, Outcome::NotApplied))?
    {
        return Ok(StopResult::AlreadyStopped);
    }
    let runtime = Runtime::load(path).map_err(|e| {
        classify(
            e,
            FailureKind::Storage,
            &Runtime::path(path),
            Outcome::NotApplied,
        )
    })?;
    // Same health-then-authenticated-shutdown sequence as Runtime::stop.
    runtime.health().await.map_err(|e| {
        classify(
            e,
            FailureKind::Unhealthy,
            &Runtime::path(path),
            Outcome::NotApplied,
        )
    })?;
    runtime.shutdown_verified().await.map_err(|e| {
        classify(
            e,
            FailureKind::ShutdownFailed,
            &Runtime::path(path),
            Outcome::Unknown,
        )
    })?;
    let deadline = Instant::now() + STOP_TIMEOUT;
    while local::running(path)
        .map_err(|e| classify(e, FailureKind::Storage, &directory, Outcome::Unknown))?
    {
        if Instant::now() >= deadline {
            return Err(failure(
                FailureKind::ShutdownFailed,
                &Runtime::path(path),
                Outcome::Unknown,
            )
            .into());
        }
        sleep(POLL_INTERVAL).await;
    }
    Ok(StopResult::Stopped)
}

#[cfg(windows)]
fn prevent_stdio_inheritance() -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation},
        System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
    };
    // Redirecting the child's stdio alone does not prevent inherited capture pipes
    // from the CLI. Clear flags on those borrowed handles; never close them.
    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle borrows a process-owned handle.
        let handle = unsafe { GetStdHandle(stream) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: Only the inheritance flag of the valid borrowed handle changes.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            anyhow::bail!("Cannot prevent CLI standard handle inheritance");
        }
    }
    Ok(())
}
