// Test-only powershell.exe shim: delay, then run the real ACL script unchanged.
use std::os::windows::process::CommandExt;
use std::{
    env, fs::OpenOptions, io::Write, path::PathBuf, process::Command, thread, time::Duration,
};

fn main() {
    // The marker contains no script, path, token, or credential data.
    let log = env::var_os("DEVLINK_ACL_PROXY_CALL_LOG").expect("test call log must exist");
    writeln!(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .unwrap(),
        "check"
    )
    .unwrap();
    thread::sleep(Duration::from_secs(3));
    let real_powershell = PathBuf::from(env::var_os("SystemRoot").expect("SystemRoot must exist"))
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let status = Command::new(real_powershell)
        .args(env::args_os().skip(1))
        .creation_flags(0x08000000)
        .status()
        .expect("real PowerShell must run");
    std::process::exit(status.code().unwrap_or(1));
}
