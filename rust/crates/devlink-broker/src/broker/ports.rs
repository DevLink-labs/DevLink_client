use crate::config::Config;
use anyhow::Result;
use std::{io, net::Ipv4Addr};
use tokio::net::TcpListener;

/// Port failures the foreground broker can report to its detached startup caller.
#[derive(Debug)]
pub enum PortFailure {
    Unavailable(u16),
    AssignmentFailed,
}

impl PortFailure {
    pub(crate) const UNAVAILABLE_EXIT: i32 = 98;
    pub(crate) const ASSIGNMENT_FAILED_EXIT: i32 = 99;

    pub fn child_exit_code(&self) -> i32 {
        match self {
            Self::Unavailable(_) => Self::UNAVAILABLE_EXIT,
            Self::AssignmentFailed => Self::ASSIGNMENT_FAILED_EXIT,
        }
    }
}

impl std::fmt::Display for PortFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(port) => write!(
                f,
                "Broker loopback port {port} is in use or its bind was denied"
            ),
            Self::AssignmentFailed => {
                f.write_str("Cannot assign a broker port outside the Windows dynamic TCP range")
            }
        }
    }
}

impl std::error::Error for PortFailure {}

fn unavailable(error: &io::Error) -> bool {
    #[cfg(windows)]
    if error.raw_os_error() == Some(windows_sys::Win32::Networking::WinSock::WSAEACCES) {
        return true;
    }
    error.kind() == io::ErrorKind::AddrInUse
}

pub(super) async fn bind(config: &Config) -> Result<TcpListener> {
    #[cfg(windows)]
    if config.local_port_pending {
        return windows::bind_setup_port().await;
    }
    TcpListener::bind((Ipv4Addr::LOCALHOST, config.local_port))
        .await
        .map_err(|error| {
            if unavailable(&error) {
                anyhow::Error::new(error).context(PortFailure::Unavailable(config.local_port))
            } else {
                anyhow::Error::new(error).context("Cannot bind broker loopback port")
            }
        })
}

#[cfg(windows)]
mod windows {
    use super::*;
    use crate::config::DEFAULT_LOCAL_PORT;
    use anyhow::Context;
    use std::{
        ffi::OsString, ops::RangeInclusive, os::windows::ffi::OsStringExt, path::PathBuf,
        process::Stdio, time::Duration,
    };
    use tokio::{process::Command, time::timeout};
    use windows_sys::Win32::{
        Foundation::MAX_PATH, System::SystemInformation::GetSystemDirectoryW,
        System::Threading::CREATE_NO_WINDOW,
    };

    const LAST_REGISTERED_PORT: u16 = 49151;
    const FIRST_UNPRIVILEGED_PORT: u16 = 1024;

    fn candidates(dynamic: RangeInclusive<u16>) -> impl Iterator<Item = u16> {
        (DEFAULT_LOCAL_PORT..=LAST_REGISTERED_PORT)
            .chain((FIRST_UNPRIVILEGED_PORT..DEFAULT_LOCAL_PORT).rev())
            .filter(move |port| !dynamic.contains(port))
    }

    fn parse_dynamic_range(output: &str) -> Option<RangeInclusive<u16>> {
        // netsh localizes the labels. The start and count are the numeric values.
        let mut values = output.lines().filter_map(|line| {
            let (_, value) = line.split_once(':')?;
            value.trim().parse::<u32>().ok()
        });
        let start = values.next()?;
        let count = values.next()?;
        if start == 0 || count == 0 || values.next().is_some() {
            return None;
        }
        let end = start.checked_add(count - 1)?;
        Some(u16::try_from(start).ok()?..=u16::try_from(end).ok()?)
    }

    async fn active_dynamic_range() -> Result<RangeInclusive<u16>> {
        let mut system_dir = [0u16; MAX_PATH as usize];
        // The buffer is writable for the number of UTF-16 units passed to Windows.
        let length = unsafe { GetSystemDirectoryW(system_dir.as_mut_ptr(), MAX_PATH) } as usize;
        anyhow::ensure!(
            length > 0 && length < system_dir.len(),
            "Cannot locate the Windows system directory"
        );
        let system_dir = PathBuf::from(OsString::from_wide(&system_dir[..length]));
        let mut command = Command::new(system_dir.join("netsh.exe"));
        command
            .current_dir(system_dir)
            .args([
                "interface",
                "ipv4",
                "show",
                "dynamicportrange",
                "protocol=tcp",
                "store=active",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = timeout(Duration::from_secs(3), command.output()).await??;
        anyhow::ensure!(
            output.status.success(),
            "Cannot query the Windows IPv4 dynamic TCP range"
        );
        parse_dynamic_range(&String::from_utf8_lossy(&output.stdout))
            .context("Unexpected Windows IPv4 dynamic TCP range")
    }

    pub(super) async fn bind_setup_port() -> Result<TcpListener> {
        let dynamic = active_dynamic_range()
            .await
            .context(PortFailure::AssignmentFailed)?;
        for port in candidates(dynamic) {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                Ok(listener) => return Ok(listener),
                Err(error) if unavailable(&error) => {}
                Err(error) => return Err(error).context("Cannot bind broker loopback port"),
            }
        }
        Err(PortFailure::AssignmentFailed.into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_configured_range_independent_of_locale() {
            assert_eq!(
                parse_dynamic_range("IPv4 TCP\r\n시작 포트 : 40000\r\n포트 수 : 123\r\n"),
                Some(40000..=40122)
            );
            assert_eq!(
                parse_dynamic_range("Start Port : 65535\nNumber of Ports : 2"),
                None
            );
            assert_eq!(parse_dynamic_range("Start Port : 49152"), None);
        }

        #[test]
        fn avoids_custom_dynamic_range_and_prefers_ports_near_the_default() {
            assert_eq!(candidates(49152..=65535).next(), Some(DEFAULT_LOCAL_PORT));
            assert_eq!(candidates(43126..=43128).next(), Some(43129));
            assert_eq!(candidates(43127..=65535).next(), Some(43126));
            assert_eq!(candidates(1024..=65535).next(), None);
            assert!(unavailable(&io::Error::from_raw_os_error(10013)));
            assert!(unavailable(&io::Error::from_raw_os_error(10048)));
        }
    }
}
