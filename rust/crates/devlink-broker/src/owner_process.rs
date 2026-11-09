//! Owner process of a local MCP session (DevLink_client#108, S15P31D108-144).
//!
//! Codex on Windows ends its MCP child with a hard termination: no stdin EOF, no signal, so
//! the MCP server never releases its session and the Agent stays bound until the 90 s lease
//! expires. The MCP reports its own process ID when it claims a session; the broker records
//! that process (with its start time, so a reused PID is not mistaken for it) and expires the
//! session as soon as the process has definitely ended. When the state cannot be read, the
//! session is kept and the lease applies as before.

/// A process recorded at claim time. `started` is a platform start-time token (0 = unknown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OwnerProcess {
    pid: u32,
    started: u64,
}

enum Probe {
    Running(u64),
    Gone,
    Unknown,
}

impl OwnerProcess {
    /// The running process with this ID, or `None` when it is not running or cannot be read.
    pub fn identify(pid: u32) -> Option<Self> {
        match probe(pid) {
            Probe::Running(started) => Some(Self { pid, started }),
            Probe::Gone | Probe::Unknown => None,
        }
    }

    /// True only when this process has definitely ended or its ID now names another process.
    pub fn is_gone(&self) -> bool {
        match probe(self.pid) {
            Probe::Gone => true,
            Probe::Running(started) => self.started != 0 && started != 0 && started != self.started,
            Probe::Unknown => false,
        }
    }

    #[cfg(test)]
    pub(crate) fn synthetic(pid: u32, started: u64) -> Self {
        Self { pid, started }
    }
}

#[cfg(windows)]
fn probe(pid: u32) -> Probe {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INVALID_PARAMETER, FILETIME, GetLastError, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        WaitForSingleObject,
    };
    if pid == 0 {
        return Probe::Unknown;
    }
    // SAFETY: the handle is checked for null, used only with these query functions, and closed
    // once; every out-parameter is a valid local.
    unsafe {
        let handle = OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        );
        if handle.is_null() {
            // No process with this ID. Access denied and other errors are not proof of exit.
            return if GetLastError() == ERROR_INVALID_PARAMETER {
                Probe::Gone
            } else {
                Probe::Unknown
            };
        }
        let empty = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (empty, empty, empty, empty);
        let has_times =
            GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        // A process handle is signaled at exit. GetProcessTimes leaves the exit time
        // undefined for live processes, and an exit code of 259 is legal.
        let state = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        match state {
            WAIT_OBJECT_0 => Probe::Gone,
            WAIT_TIMEOUT if has_times => Probe::Running(
                (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            ),
            _ => Probe::Unknown,
        }
    }
}

#[cfg(unix)]
fn probe(pid: u32) -> Probe {
    let Ok(raw) = i32::try_from(pid) else {
        return Probe::Unknown;
    };
    if raw <= 0 {
        return Probe::Unknown;
    }
    // SAFETY: signal 0 only checks that the process exists; nothing is delivered.
    if unsafe { libc::kill(raw, 0) } != 0 {
        return if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            Probe::Gone
        } else {
            Probe::Unknown
        };
    }
    match linux_stat(pid) {
        // A zombie (Z) or dead (X) process has ended; only its parent has not reaped it yet.
        Some((state, _)) if matches!(state, 'Z' | 'X') => Probe::Gone,
        Some((_, started)) => Probe::Running(started),
        // Not Linux (or unreadable): running, start time unknown (PID reuse not detected).
        None => Probe::Running(0),
    }
}

/// State (field 3) and start time in clock ticks (field 22) of /proc/<pid>/stat; `None` off Linux.
#[cfg(unix)]
fn linux_stat(pid: u32) -> Option<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name (field 2) may contain spaces; fields after it follow the last ')'.
    let mut fields = stat[stat.rfind(')')? + 1..].split_whitespace();
    let state = fields.next()?.chars().next()?;
    Some((state, fields.nth(18)?.parse().ok()?))
}

#[cfg(not(any(windows, unix)))]
fn probe(_pid: u32) -> Probe {
    Probe::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_current_process_and_keeps_it_alive() {
        let me = OwnerProcess::identify(std::process::id()).expect("current process");
        assert!(!me.is_gone());
    }

    #[test]
    fn an_ended_child_is_gone_and_a_reused_id_with_another_start_is_gone() {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let owner = OwnerProcess::identify(child.id());
        child.wait().unwrap();
        if let Some(owner) = owner {
            assert!(owner.is_gone(), "ended child must be gone");
        }
        let me = OwnerProcess::identify(std::process::id()).unwrap();
        if me.started != 0 {
            let other_start = OwnerProcess::synthetic(me.pid, me.started + 1);
            assert!(
                other_start.is_gone(),
                "same ID with another start time is another process"
            );
        }
    }

    #[test]
    fn unreadable_ids_are_never_reported_gone() {
        assert!(OwnerProcess::identify(0).is_none());
        assert!(!OwnerProcess::synthetic(0, 0).is_gone());
    }

    /// An ID no process can have: Windows reports "no such process" (gone); on Unix it does
    /// not fit pid_t, so it is unknown and never gone.
    #[test]
    fn a_nonexistent_id_is_not_identified() {
        let impossible = u32::MAX - 3;
        assert!(OwnerProcess::identify(impossible).is_none());
        assert_eq!(
            OwnerProcess::synthetic(impossible, 1).is_gone(),
            cfg!(windows)
        );
    }
}
