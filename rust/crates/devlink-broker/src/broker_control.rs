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
    /// broker.lock is held but runtime.json is not published yet: a broker is
    /// starting right now. Transient; retry instead of repairing files.
    BrokerStarting,
    Storage,
    Unhealthy,
    TokenUnavailable,
    SpawnFailed,
    Port(PortProblem),
    ChildExited,
    StartupTimedOut,
    ShutdownFailed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortProblem {
    Unavailable,
    AssignmentFailed,
}

impl PortProblem {
    pub fn message(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "The configured broker loopback port is in use or the OS denied the bind. Run devlink --config <FILE> config set localPort <FREE_PORT> with this profile's configuration file, then retry. Reopen any running Host if the port changes."
            }
            Self::AssignmentFailed => {
                "Setup could not assign a broker port outside the system's dynamic TCP port ranges. Check the active IPv4 TCP ranges and port restrictions, or set an available fixed port with devlink --config <FILE> config set localPort <FREE_PORT> and retry."
            }
        }
    }
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
            FailureKind::BrokerStarting => "Broker is starting; wait a moment and retry",
            FailureKind::Storage => "Cannot inspect broker runtime files",
            FailureKind::Unhealthy => "Broker identity or health could not be verified",
            FailureKind::TokenUnavailable => "Cannot prepare the central Agent token",
            FailureKind::SpawnFailed => "Cannot spawn broker process",
            FailureKind::Port(problem) => problem.message(),
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
    Ok(match running_broker(path).await? {
        Some((health, _)) => Status::Running { health },
        None => Status::Stopped,
    })
}

async fn running_broker(path: &Path) -> Result<Option<(Health, u16)>> {
    if !local::running(path).map_err(|e| {
        classify(
            e,
            FailureKind::Storage,
            &Runtime::path(path),
            Outcome::NotApplied,
        )
    })? {
        return Ok(None);
    }
    let runtime = Runtime::load(path).map_err(|e| {
        // The lock holder publishes runtime.json after acquiring the lock, so a
        // missing file here is a broker still starting, not damaged storage.
        let kind = if runtime_unpublished(path, &e) {
            FailureKind::BrokerStarting
        } else {
            FailureKind::Storage
        };
        classify(e, kind, &Runtime::path(path), Outcome::NotApplied)
    })?;
    let health = runtime.health().await.map_err(|e| {
        classify(
            e,
            FailureKind::Unhealthy,
            &Runtime::path(path),
            Outcome::NotApplied,
        )
    })?;
    Ok(Some((health, runtime.port)))
}

fn runtime_unpublished(path: &Path, error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<std::io::Error>()
        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
        && std::fs::symlink_metadata(Runtime::path(path)).is_err()
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
    let config = Config::load(path)
        .map_err(|e| classify(e, FailureKind::InvalidConfig, path, Outcome::NotApplied))?;
    if let Some((health, port)) = running_broker(path).await? {
        save_pending_port(path, &config, port)?;
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
        command.creation_flags(0x00000008 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // Only this spawn must not leak the CLI's standard handles; a long-running
    // caller such as setup keeps inheritance for the hosts it launches later.
    #[cfg(windows)]
    let restore_inheritance = StdioInheritanceGuard::suspend()
        .map_err(|e| classify(e, FailureKind::SpawnFailed, path, Outcome::NotApplied))?;
    let spawned = command.spawn();
    #[cfg(windows)]
    drop(restore_inheritance);
    let mut child = spawned.map_err(|e| {
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
    let (health, port) = wait_for_start(path, owned.child, startup_timeout(path)).await?;
    save_pending_port(path, &config, port)?;
    owned.armed = false; // std::process::Child drop deliberately leaves a ready broker running.
    Ok(StartResult {
        reused: false,
        health,
    })
}

fn save_pending_port(path: &Path, config: &Config, port: u16) -> Result<()> {
    if config.local_port_pending {
        config::save_setup_port(path, port)
            .map_err(|e| classify(e, FailureKind::Storage, path, Outcome::Unknown))?;
    }
    Ok(())
}

async fn wait_for_start(
    path: &Path,
    child: &mut Child,
    timeout: Duration,
) -> Result<(Health, u16)> {
    let deadline = Instant::now() + timeout;
    let lock = state_directory(path).join("broker.lock");
    let mut lock_error = None;
    loop {
        if let Some(exit) = child
            .try_wait()
            .map_err(|e| classify(e.into(), FailureKind::ChildExited, path, Outcome::Unknown))?
        {
            let (kind, outcome) = match exit.code() {
                Some(broker::PortFailure::UNAVAILABLE_EXIT) => (
                    FailureKind::Port(PortProblem::Unavailable),
                    Outcome::NotApplied,
                ),
                Some(broker::PortFailure::ASSIGNMENT_FAILED_EXIT) => (
                    FailureKind::Port(PortProblem::AssignmentFailed),
                    Outcome::NotApplied,
                ),
                _ => (FailureKind::ChildExited, Outcome::Unknown),
            };
            return Err(failure(kind, path, outcome).into());
        }
        // Probing broker.lock before runtime publication can compete with the
        // child's initial acquisition. Verify the published child first.
        if let Ok(runtime) = Runtime::load(path)
            && runtime.pid == child.id()
            && let Ok(health) = runtime.health().await
        {
            // The child already answered an authenticated health check, so a
            // failing lock inspection here must not end a healthy broker. Keep
            // polling; a persistent error is reported at the deadline instead.
            match local::running(path) {
                Ok(true) => return Ok((health, runtime.port)),
                Ok(false) => lock_error = None,
                Err(error) => lock_error = Some(error),
            }
        }
        if Instant::now() >= deadline {
            return Err(match lock_error {
                Some(error) => classify(error, FailureKind::Storage, &lock, Outcome::Unknown),
                None => failure(FailureKind::StartupTimedOut, path, Outcome::Unknown).into(),
            });
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Only a Child handle obtained by our spawn is ever force-cleaned up. This also
/// handles errors or cancellation while waiting, before releasing control.lock.
/// The kill is always sent, but reaping is bounded: if the exit takes longer, a
/// terminating child may still hold broker.lock briefly after control.lock is released,
/// and the next start or stop reports a busy broker until it is gone.
struct StartingChild<'a> {
    child: &'a mut Child,
    armed: bool,
}
impl Drop for StartingChild<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.child.kill();
            // Reap without blocking indefinitely: termination can be delayed by the OS
            // or security software, and this drop may run on an async worker thread.
            let deadline = std::time::Instant::now() + crate::tool_discovery::REAP_TIMEOUT;
            while matches!(self.child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
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
        let kind = if runtime_unpublished(path, &e) {
            FailureKind::BrokerStarting
        } else {
            FailureKind::Storage
        };
        classify(e, kind, &Runtime::path(path), Outcome::NotApplied)
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

/// Clears HANDLE_FLAG_INHERIT on the CLI's standard handles for one spawn and
/// restores the previous flags on drop. Redirecting the child's stdio alone does
/// not stop inherited capture pipes from keeping a parent's reader open.
#[cfg(windows)]
struct StdioInheritanceGuard {
    inheritable: Vec<windows_sys::Win32::Foundation::HANDLE>,
}
#[cfg(windows)]
impl StdioInheritanceGuard {
    fn suspend() -> Result<Self> {
        use windows_sys::Win32::{
            Foundation::{
                GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
                SetHandleInformation,
            },
            System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            },
        };
        let mut guard = Self {
            inheritable: Vec::new(),
        };
        for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle borrows a process-owned handle.
            let handle = unsafe { GetStdHandle(stream) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut flags = 0;
            // SAFETY: flags is a valid out-pointer for the borrowed handle.
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                anyhow::bail!("Cannot inspect CLI standard handle inheritance");
            }
            if flags & HANDLE_FLAG_INHERIT == 0 {
                continue;
            }
            // SAFETY: Only the inheritance flag of the valid borrowed handle changes.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
                anyhow::bail!("Cannot prevent CLI standard handle inheritance");
            }
            guard.inheritable.push(handle);
        }
        Ok(guard)
    }
}
#[cfg(windows)]
impl Drop for StdioInheritanceGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        for handle in self.inheritable.drain(..) {
            // SAFETY: Restores the flag this guard cleared on the same borrowed
            // handle. A failure only leaves it non-inheritable; nothing is closed.
            let _ =
                unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
        }
    }
}
