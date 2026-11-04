// Synthetic Node/MCP runtime. No network, Host or model calls are possible.
use std::{fs, io::{BufRead, Read, Write}};
fn main() {
    let exe = std::env::current_exe().unwrap();
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mode = fs::read_to_string(exe.with_extension("mcp-mode")).unwrap_or_default();
    let mut log = fs::OpenOptions::new().create(true).append(true).open(exe.with_extension("mcp-calls")).unwrap();
    writeln!(log, "{}", args.join("|")).unwrap();
    assert!(std::env::var_os("DEVLINK_BROKER_FILE").is_none());
    assert!(std::env::var_os("NODE_OPTIONS").is_none());
    if args == ["--version"] {
        if mode == "old-node" { println!("v22.0.0"); } else { println!("v24.18.0"); }
        return;
    }
    if args.len() == 2 && args[0].ends_with(".js") && args[1] == "--version" {
        // Host probe through an npm shim: the package entry's .probe file holds the version text.
        let probe = std::path::Path::new(&args[0]).with_extension("probe");
        println!("{}", fs::read_to_string(probe).unwrap().trim_end());
        return;
    }
    assert_eq!(&args[1..], ["--transport", "stdio"]);
    if mode == "timeout" { std::thread::sleep(std::time::Duration::from_secs(30)); }
    if mode == "invalid" { println!("private-runtime-error-marker"); return; }
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("initialize") && !line.contains("tools/call"));
    let name = if mode == "wrong-server" { "another-server" } else { "devlink-mcp" };
    println!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{{\"tools\":{{}}}},\"serverInfo\":{{\"name\":\"{name}\",\"version\":\"0.1.0\"}}}}}}");
    std::io::stdout().flush().unwrap();
    let mut rest = String::new();
    reader.read_to_string(&mut rest).unwrap();
    assert!(rest.is_empty());
}
