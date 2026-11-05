// Synthetic Host: it cannot call a model. Every invocation is recorded for assertions.
use std::{fs, io::Write, path::PathBuf};
fn main() {
    let executable = std::env::current_exe().unwrap();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut log = fs::OpenOptions::new().create(true).append(true).open(executable.with_extension("calls")).unwrap();
    writeln!(log, "{}", args.join("|")).unwrap();
    let script = args.first().is_some_and(|s| s.ends_with(".js"));
    assert_eq!(args.last().map(String::as_str), Some("--version"));
    assert_eq!(args.len(), if script { 2 } else { 1 });
    let config = if script { PathBuf::from(&args[0]).with_extension("probe") } else { executable.with_extension("probe") };
    let mode = fs::read_to_string(config).unwrap();
    if mode == "timeout" { std::thread::sleep(std::time::Duration::from_secs(30)); }
    if mode == "overflow" { print!("{}", "x".repeat(9000)); return; }
    if mode == "failure" { eprintln!("private-version-error-marker"); std::process::exit(1); }
    if mode == "invalid" { println!("private-version-error-marker"); return; }
    println!("{mode}");
}
