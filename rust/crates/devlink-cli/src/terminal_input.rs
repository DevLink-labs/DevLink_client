//! Terminal-only secret input, shared by the existing commands and setup adapter.
use anyhow::{Context, Result, bail};

/// Keep a copy outside the blocking password reader. If setup drops the
/// reader's future, its own raw-mode guard may never run before process exit.
#[cfg(unix)]
pub(crate) struct TerminalMode(std::fs::File, libc::termios);

#[cfg(unix)]
impl TerminalMode {
    pub(crate) fn capture() -> Option<Self> {
        use std::os::fd::AsRawFd;
        let tty = std::fs::File::open("/dev/tty").ok()?;
        let mut mode = std::mem::MaybeUninit::uninit();
        // SAFETY: tty is open and mode points to writable termios storage.
        if unsafe { libc::tcgetattr(tty.as_raw_fd(), mode.as_mut_ptr()) } != 0 {
            return None;
        }
        // SAFETY: tcgetattr succeeded and initialized mode.
        Some(Self(tty, unsafe { mode.assume_init() }))
    }
}

#[cfg(unix)]
impl Drop for TerminalMode {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: the guard still owns the tty and its original mode.
        unsafe { libc::tcsetattr(self.0.as_raw_fd(), libc::TCSANOW, &self.1) };
    }
}

#[cfg(windows)]
pub(crate) struct TerminalMode(Vec<(usize, u32)>);

#[cfg(windows)]
impl TerminalMode {
    pub(crate) fn capture() -> Option<Self> {
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE,
        };
        let mut modes = Vec::new();
        for selector in [STD_INPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: the selectors are standard handles and mode is writable.
            let handle = unsafe { GetStdHandle(selector) };
            let mut mode = 0;
            if unsafe { GetConsoleMode(handle, &mut mode) } != 0 {
                modes.push((handle as usize, mode));
            }
        }
        (!modes.is_empty()).then_some(Self(modes))
    }
}

#[cfg(windows)]
impl Drop for TerminalMode {
    fn drop(&mut self) {
        use windows_sys::Win32::{Foundation::HANDLE, System::Console::SetConsoleMode};
        for &(handle, mode) in &self.0 {
            // SAFETY: handles were obtained from this process and are not owned
            // by the guard. Setup never replaces the standard handles.
            unsafe { SetConsoleMode(handle as HANDLE, mode) };
        }
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) struct TerminalMode;

#[cfg(not(any(unix, windows)))]
impl TerminalMode {
    pub(crate) fn capture() -> Option<Self> {
        None
    }
}

pub fn secret_input(label: &str, limit: usize, from_stdin: bool) -> Result<String> {
    use std::io::{BufRead, Read};
    let password = if from_stdin {
        let mut bytes = Vec::new();
        let count = std::io::stdin()
            .lock()
            .take(1025)
            .read_until(b'\n', &mut bytes)
            .context("Cannot read password from stdin")?;
        if count == 0 {
            // Shared by auth/team commands: say what is missing. Setup maps the
            // UnexpectedEof kind to cancellation.
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
                .with_context(|| format!("No {label} was received on stdin"));
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("Password must be UTF-8"))?
    } else {
        rpassword::prompt_password(format!("{label}: ")).with_context(|| {
            let flag = if label == "Password" {
                "--password-stdin"
            } else {
                "--code-stdin"
            };
            format!("Cannot read hidden {label}; use {flag} for pipes")
        })?
    };
    if password.is_empty() || password.len() > limit {
        bail!("{label} must contain 1 to {limit} UTF-8 bytes");
    }
    Ok(password)
}
