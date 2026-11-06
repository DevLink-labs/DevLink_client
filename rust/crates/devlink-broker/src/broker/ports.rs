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
            Self::AssignmentFailed => f.write_str(
                "Cannot assign a broker port outside the system's dynamic TCP port ranges",
            ),
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
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    if config.local_port_pending {
        return setup_port::bind().await;
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

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
mod setup_port {
    use super::{PortFailure, unavailable};
    use crate::config::DEFAULT_LOCAL_PORT;
    use anyhow::{Context, Result};
    use std::{io, net::Ipv4Addr, ops::RangeInclusive};
    use tokio::net::TcpListener;

    const LAST_REGISTERED_PORT: u16 = 49151;
    const FIRST_REGISTERED_PORT: u16 = 1024;

    fn candidates(dynamic: &[RangeInclusive<u16>]) -> impl Iterator<Item = u16> + '_ {
        (DEFAULT_LOCAL_PORT..=LAST_REGISTERED_PORT)
            .chain((FIRST_REGISTERED_PORT..DEFAULT_LOCAL_PORT).rev())
            .filter(move |port| !dynamic.iter().any(|range| range.contains(port)))
    }

    #[cfg(target_os = "linux")]
    fn parse_linux_range(value: &str) -> Option<RangeInclusive<u16>> {
        let mut ports = value.split_whitespace();
        let start = ports.next()?.parse::<u16>().ok()?;
        let end = ports.next()?.parse::<u16>().ok()?;
        (start > 0 && end >= start && ports.next().is_none()).then_some(start..=end)
    }

    #[cfg(target_os = "linux")]
    fn linux_ranges() -> Result<Vec<RangeInclusive<u16>>> {
        let value = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range")
            .context("Cannot read the Linux IPv4 dynamic port range")?;
        Ok(vec![
            parse_linux_range(&value).context("Unexpected Linux IPv4 dynamic port range")?,
        ])
    }

    #[cfg(target_os = "macos")]
    fn macos_ranges() -> Result<Vec<RangeInclusive<u16>>> {
        use std::ffi::CStr;

        fn port(name: &CStr) -> Result<u16> {
            let mut value: libc::c_int = 0;
            let mut len = std::mem::size_of_val(&value);
            // These sysctls return a C int. The buffer is writable for len bytes.
            let result = unsafe {
                libc::sysctlbyname(
                    name.as_ptr(),
                    std::ptr::from_mut(&mut value).cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error())
                    .context("Cannot query the macOS IPv4 port range");
            }
            anyhow::ensure!(
                len == std::mem::size_of_val(&value) && value > 0,
                "Unexpected macOS IPv4 port"
            );
            u16::try_from(value).context("Invalid macOS IPv4 port")
        }

        let first = port(c"net.inet.ip.portrange.first")?;
        let last = port(c"net.inet.ip.portrange.last")?;
        let high_first = port(c"net.inet.ip.portrange.hifirst")?;
        let high_last = port(c"net.inet.ip.portrange.hilast")?;
        // XNU supports both ascending and descending endpoints.
        Ok(vec![
            first.min(last)..=first.max(last),
            high_first.min(high_last)..=high_first.max(high_last),
        ])
    }

    pub(super) async fn bind() -> Result<TcpListener> {
        #[cfg(windows)]
        let ranges = super::windows::active_dynamic_range()
            .await
            .map(|range| vec![range]);
        #[cfg(target_os = "linux")]
        let ranges = linux_ranges();
        #[cfg(target_os = "macos")]
        let ranges = macos_ranges();

        let ranges = ranges.context(PortFailure::AssignmentFailed)?;
        for port in candidates(&ranges) {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
                Ok(listener) => return Ok(listener),
                Err(error)
                    if unavailable(&error) || error.kind() == io::ErrorKind::PermissionDenied => {}
                Err(error) => return Err(error).context("Cannot bind broker loopback port"),
            }
        }
        Err(PortFailure::AssignmentFailed.into())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn avoids_dynamic_ranges_and_prefers_ports_near_the_default() {
            for (dynamic, expected) in [
                (49152..=65535, Some(DEFAULT_LOCAL_PORT)),
                (32768..=60999, Some(32767)),
                (43126..=43128, Some(43129)),
                (43127..=65535, Some(43126)),
                (1024..=65535, None),
            ] {
                assert_eq!(candidates(std::slice::from_ref(&dynamic)).next(), expected);
            }
            assert_eq!(
                candidates(&[43126..=43128, 43129..=43131]).next(),
                Some(43132)
            );
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn parses_linux_kernel_port_range() {
            assert_eq!(parse_linux_range("44620\t48715\n"), Some(44620..=48715));
            assert_eq!(parse_linux_range("60999 32768"), None);
            assert_eq!(parse_linux_range("0 65535"), None);
            assert_eq!(parse_linux_range("32768 60999 1"), None);
        }
    }
}

#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result};
    use std::{
        ffi::OsString, ops::RangeInclusive, os::windows::ffi::OsStringExt, path::PathBuf,
        process::Stdio, time::Duration,
    };
    use tokio::{process::Command, time::timeout};
    use windows_sys::Win32::{
        Foundation::MAX_PATH, System::SystemInformation::GetSystemDirectoryW,
        System::Threading::CREATE_NO_WINDOW,
    };

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

    pub(super) async fn active_dynamic_range() -> Result<RangeInclusive<u16>> {
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
        fn windows_port_conflicts_include_excluded_ports() {
            use crate::broker::ports::unavailable;
            use std::io::Error;
            assert!(unavailable(&Error::from_raw_os_error(10013)));
            assert!(unavailable(&Error::from_raw_os_error(10048)));
        }
    }
}
