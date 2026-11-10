#![allow(clippy::result_large_err)]
use devlink_broker::{
    central::{self, CancellationToken, CentralIdentity, CentralState, ConnectionStatus},
    config::{self, Config},
    local::{self, Health, Runtime},
    protocol::{ClientMessage, HeartbeatPing, PresenceStatus, ServerMessage},
};
use futures_util::{SinkExt, StreamExt};
use std::{
    path::PathBuf,
    process::{Child, Command, Output},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Mutex, RwLock},
    task::{JoinHandle, JoinSet},
    time::{Instant, sleep},
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::{
        Message,
        handshake::server::{ErrorResponse, Request, Response},
        http::{StatusCode, header::AUTHORIZATION},
        protocol::CloseFrame,
    },
};

const TOKEN: &str = "test-only-secret-never-print";

async fn serve_context_request(stream: &mut tokio::net::TcpStream, agent: Option<u64>) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut head = [0; 4096];
    let count = stream.peek(&mut head).await.unwrap();
    // The broker also pulls question/answer arrivals on the same origin after READY.
    let messages = head[..count].starts_with(b"GET /api/v1/agents/me/task-messages?");
    // The broker refreshes the owner's Git comparison value from startup.
    let git_identity = head[..count].starts_with(b"GET /api/v1/agents/me/git-identity ");
    if !messages && !git_identity && !head[..count].starts_with(b"GET /api/v1/agents/me ") {
        return false;
    }
    let mut request = Vec::new();
    loop {
        let count = stream.read(&mut head).await.unwrap();
        if count == 0 {
            return true;
        }
        request.extend_from_slice(&head[..count]);
        if request.windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
        assert!(request.len() < 16384);
    }
    let headers = String::from_utf8(request).unwrap().to_lowercase();
    assert!(headers.contains(&format!("authorization: bearer {TOKEN}\r\n")));
    assert!(!headers.contains("cookie:") && !headers.contains("origin:"));
    if messages {
        let body = serde_json::json!({"success":true,"data":{"items":[],"nextCursor":"cli-fixture","hasNext":false}})
            .to_string();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        return true;
    }
    if git_identity {
        let body =
            serde_json::json!({"success":true,"data":{"registered":false,"gitEmailHash":null,
            "hashAlgorithm":"SHA-256","normalization":"TRIM_LOWERCASE"}})
            .to_string();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        return true;
    }
    let body = match agent {
        Some(id) => {
            serde_json::json!({"success":true,"data":{"agentId":id,"ownerUserId":3,"teamId":1,
                "name":"Existing Agent","roleDescription":null,"toolType":"CODEX","projectName":"CLI fixture",
                "status":"ONLINE","lastSeenAt":null,"createdAt":"2026-10-29T00:00:00"}})
                .to_string()
        }
        None => serde_json::json!({"success":false,"code":"AUTHENTICATION_REQUIRED"}).to_string(),
    };
    let status = if agent.is_some() {
        "200 OK"
    } else {
        "401 Unauthorized"
    };
    stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_creates_tasks_with_agent_bearer_and_reuses_request_key_after_restart() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    f.configure(|config| config.server_url = format!("ws://{}", listener.local_addr().unwrap()));
    let saved = Arc::new(Mutex::new(std::collections::HashMap::<String, u64>::new()));
    let saved_copy = saved.clone();
    let server = tokio::spawn(async move {
        let mut clients = JoinSet::new();
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let saved = saved_copy.clone();
            clients.spawn(async move {
                let mut method = [0;3];
                while stream.peek(&mut method).await.unwrap() < 3 { sleep(Duration::from_millis(1)).await; }
                if serve_context_request(&mut stream, Some(12)).await { return; }
                if &method == b"GET" {
                    let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                    socket.send(Message::Text(serde_json::json!({"type":"BROKER_READY","payload":{
                        "connection_id":"cli-task-connection","agent_id":12,"user_id":3,"team_id":1
                    }}).to_string().into())).await.unwrap();
                    while let Some(Ok(frame)) = socket.next().await {
                        match frame {
                            Message::Text(text) => if let Ok(ClientMessage::Heartbeat(HeartbeatPing::Ping{ping_id})) = serde_json::from_str(&text) {
                                socket.send(Message::Text(serde_json::to_string(&ServerMessage::heartbeat_pong(ping_id)).unwrap().into())).await.unwrap();
                            },
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                    return;
                }
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0;1024];
                    let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0 && bytes.len() < 16 * 1024);
                    bytes.extend_from_slice(&chunk[..count]);
                    if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") { break index + 4; }
                };
                let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap().to_lowercase();
                assert!(headers.starts_with("post /api/v1/tasks/1 http/1.1\r\n"));
                assert!(headers.contains(&format!("authorization: bearer {TOKEN}\r\n")));
                assert!(!headers.contains("cookie:") && !headers.contains("origin:"));
                let size: usize = headers.lines().find_map(|line| line.strip_prefix("content-length:")).unwrap().trim().parse().unwrap();
                assert!(size <= 128 * 1024);
                while bytes.len() - header_end < size {
                    let mut chunk = [0;1024]; let count = stream.read(&mut chunk).await.unwrap();
                    assert!(count > 0); bytes.extend_from_slice(&chunk[..count]);
                }
                let command: serde_json::Value = serde_json::from_slice(&bytes[header_end..header_end+size]).unwrap();
                assert!(command.get("requesterAgentId").is_none()); assert_eq!(command["receiverAgentId"],13);
                let key = command["requestKey"].as_str().unwrap().to_string();
                let mut tasks = saved.lock().await;
                let replayed = tasks.contains_key(&key);
                let id = *tasks.entry(key).or_insert(101);
                drop(tasks);
                let response = serde_json::json!({"success":true,"data":{"taskId":id,"receiverAgentId":13,
                    "status":"PENDING","version":1,"replayed":replayed,"delivery":{"status":"ACK_PENDING","reasonCode":null}},"code":null,"message":null}).to_string();
                let status = if replayed {"200 OK"} else {"201 Created"};
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
            });
            while let Some(result) = clients.try_join_next() {
                result.unwrap();
            }
        }
    });
    let mut original_key = None;
    for attempt in 0..2 {
        f.ok(&["start"]);
        f.wait_status(ConnectionStatus::Connected).await;
        let runtime = Runtime::load(&f.path).unwrap();
        let credentials = devlink_broker::credentials::load(&f.path).unwrap();
        // Runtime::client is a one-second health/control client. Task creation
        // includes durable request-key storage and central REST verification;
        // let the endpoint's own budget decide its result under parallel load.
        let client = runtime.client().unwrap();
        let request_timeout = devlink_broker::business_api::LOCAL_BUDGET + Duration::from_secs(1);
        let session: serde_json::Value = client.post(runtime.url("/v1/sessions")).bearer_auth(&credentials.local_token)
            .timeout(request_timeout)
            .json(&serde_json::json!({"requestId":"claim","instanceId":"creator","mode":"fresh","agentId":"12"}))
            .send().await.unwrap().json().await.unwrap();
        let mut context = session["data"]["principal"].clone();
        context["sessionId"] = session["data"]["sessionId"].clone();
        let response = client.post(runtime.url("/v1/tasks/create")).bearer_auth(&credentials.local_token)
            .timeout(request_timeout)
            .json(&serde_json::json!({"requestId":"stable-cli-task","operation":"create_task","context":context,
                "arguments":{"recipientAgentId":"13","title":"작업","description":"CLI 생성 검증"}}))
            .send().await.unwrap();
        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(
            status.as_u16(),
            200,
            "task create attempt {attempt}: {body}"
        );
        assert_eq!(body["data"]["taskId"], "101");
        assert_eq!(body["data"]["replayed"], attempt == 1);
        if let Some(key) = &original_key {
            assert_eq!(&body["requestKey"], key);
        } else {
            original_key = Some(body["requestKey"].clone());
        }
        assert!(
            !body.to_string().contains(TOKEN)
                && !body.to_string().contains(&credentials.local_token)
        );
        f.ok(&["stop"]);
    }
    assert_eq!(saved.lock().await.len(), 1);
    server.abort();
    let _ = server.await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_persists_tasks_before_ack_and_deduplicates_after_restart() {
    let f = Fixture::new();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("ws://{}", listener.local_addr().unwrap());
    f.configure(|config| config.server_url = origin.clone());
    let path = f.path.clone();
    let origin_copy = origin.clone();
    let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for attempt in 1..=2 {
            // Skip only the broker's question/answer pulls and Git comparison refreshes
            // (owner lookup included); any other request still has to be the WebSocket
            // upgrade this test expects.
            let stream = loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut head = [0; 64];
                while stream.peek(&mut head).await.unwrap() < 37 {
                    sleep(Duration::from_millis(1)).await;
                }
                if !serve_context_request(&mut stream, Some(12)).await {
                    break stream;
                }
            };
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let event_id = format!("cli-task-{attempt}");
            for message in [
                serde_json::json!({"type":"BROKER_READY","payload":{"connection_id":format!("cli-{attempt}"),"agent_id":12}}),
                serde_json::json!({"type":"TASK_CREATED","event_id":event_id,"payload":{
                    "task_id":101,"team_id":1,"requester_agent_id":10,"receiver_agent_id":12,
                    "prompt":"Synthetic CLI receipt test", "created_at":"2026-10-29T09:00:00"
                }}),
            ] {
                socket
                    .send(Message::Text(message.to_string().into()))
                    .await
                    .unwrap();
            }
            while let Some(Ok(frame)) = socket.next().await {
                match frame {
                    Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text)
                        .unwrap()
                    {
                        ClientMessage::Ack(ack) => {
                            assert_eq!(ack.event_id, event_id);
                            assert_eq!(
                                devlink_broker::inbox::Inbox::open(&path)
                                    .unwrap()
                                    .list_for(&origin_copy, 12)
                                    .unwrap()
                                    .len(),
                                1
                            );
                            events.send(ack.event_id).unwrap();
                        }
                        ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id }) => {
                            socket
                                .send(Message::Text(
                                    serde_json::to_string(&ServerMessage::heartbeat_pong(ping_id))
                                        .unwrap()
                                        .into(),
                                ))
                                .await
                                .unwrap();
                        }
                        _ => {}
                    },
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    });
    f.ok(&["start"]);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap(),
        "cli-task-1"
    );
    let snapshot = std::fs::read(devlink_broker::inbox::Inbox::path(&f.path)).unwrap();
    assert!(!String::from_utf8_lossy(&snapshot).contains(TOKEN));
    let first_instance = Runtime::load(&f.path).unwrap().instance_id;
    let list: serde_json::Value = serde_json::from_str(&f.ok(&["inbox", "list"])).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!f.run(&["inbox", "clear"]).status.success());
    f.ok(&["stop"]);
    f.ok(&["start"]);
    assert_ne!(Runtime::load(&f.path).unwrap().instance_id, first_instance);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap(),
        "cli-task-2"
    );
    let current: serde_json::Value = serde_json::from_slice(
        &std::fs::read(devlink_broker::inbox::Inbox::path(&f.path)).unwrap(),
    )
    .unwrap();
    let original: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
    assert_eq!(current["tasks"], original["tasks"]);
    assert_eq!(current["receipts"].as_array().unwrap().len(), 2);
    f.ok(&["stop"]);
    f.ok(&["inbox", "clear"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&f.ok(&["inbox", "list"])).unwrap(),
        serde_json::json!([])
    );
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbox_cli_reads_local_requests_safely_and_clear_requires_stopped_broker() {
    use devlink_broker::{
        inbox::Inbox,
        protocol::{TaskCreatedMessage, TaskCreatedPayload},
    };
    let f = Fixture::new();
    let before: Vec<_> = std::fs::read_dir(config::state_directory(&f.path))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&f.ok(&["inbox", "list"])).unwrap(),
        serde_json::json!([])
    );
    let after: Vec<_> = std::fs::read_dir(config::state_directory(&f.path))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
    assert!(!Inbox::path(&f.path).exists());
    let origin = Config::load(&f.path).unwrap().server_url;
    let inbox = Inbox::open(&f.path).unwrap();
    let prompt = "\u{001b}[31m\u{0085}\u{202e}\u{2067}\u{200f}실행하지 않은 요청";
    for id in [12, 13] {
        inbox
            .record(
                origin.clone(),
                id,
                TaskCreatedMessage {
                    event_id: format!("cli-inbox-{id}"),
                    payload: TaskCreatedPayload {
                        task_id: id,
                        team_id: 1,
                        requester_agent_id: 10,
                        receiver_agent_id: id,
                        prompt: prompt.into(),
                        created_at: "2026-10-30T09:00:00".into(),
                    },
                },
            )
            .await
            .unwrap();
    }
    let shown = f.ok(&["inbox", "list", "--agent-id", "12", "--limit", "1"]);
    for c in ['\u{001b}', '\u{0085}', '\u{202e}', '\u{2067}', '\u{200f}'] {
        assert!(!shown.contains(c));
    }
    let parsed: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(parsed.as_array().unwrap().len(), 1);
    assert_eq!(parsed[0]["message"]["payload"]["prompt"], prompt);
    assert_eq!(parsed[0]["message"]["payload"]["requester_agent_id"], 10);
    assert!(!f.run(&["inbox", "list", "--limit", "0"]).status.success());
    let before = std::fs::read(Inbox::path(&f.path)).unwrap();
    let guard = config::lock(&config::state_directory(&f.path).join("broker.lock")).unwrap();
    assert!(!f.run(&["inbox", "clear"]).status.success());
    assert_eq!(std::fs::read(Inbox::path(&f.path)).unwrap(), before);
    drop(guard);
    let credentials =
        devlink_broker::credentials::publish(&f.path, "http://127.0.0.1:43127").unwrap();
    f.ok(&["inbox", "clear"]);
    assert!(!Inbox::path(&f.path).exists());
    assert_eq!(
        devlink_broker::credentials::load(&f.path).unwrap(),
        credentials
    );
}
impl Fixture {
    fn new() -> Self {
        Self::with_config_name("config.json")
    }
    fn with_config_name(name: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("devlink test 한글 ")
            .tempdir()
            .unwrap();
        let path = dir.path().join(name);
        config::init(&path).unwrap();
        let mut config = Config::load(&path).unwrap();
        // Local lifecycle fixtures must never inherit the production default URL.
        config.server_url = "ws://127.0.0.1:9".into();
        config.local_port = 0;
        config.reconnect_initial_ms = 50;
        config.reconnect_max_ms = 100;
        config.auth_timeout_ms = 200;
        config::atomic_json(&path, &config).unwrap();
        Self { _dir: dir, path }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devlink"));
        command
            .arg("--config")
            .arg(&self.path)
            .args(args)
            .env("DEVLINK_TOKEN", TOKEN);
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(!text(&output).contains(TOKEN), "secret leaked");
        output
    }
    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(output.status.success(), "{:?}: {}", args, text(&output));
        text(&output)
    }
    fn configure(&self, update: impl FnOnce(&mut Config)) {
        let mut config = Config::load(&self.path).unwrap();
        update(&mut config);
        config::atomic_json(&self.path, &config).unwrap();
    }
    async fn health(&self) -> Health {
        Runtime::load(&self.path).unwrap().health().await.unwrap()
    }
    async fn wait_status(&self, expected: ConnectionStatus) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let health = self.health().await;
            if health.central_server == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Expected {expected:?}, got {:?}",
                health.central_server
            );
            sleep(Duration::from_millis(30)).await;
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.command(&["stop"]).output();
    }
}
fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

struct OwnedProcess(Child);
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn config_create_show_update_and_validation() {
    let f = Fixture::new();
    assert!(f.ok(&["init"]).contains("already exists"));
    assert!(f.ok(&["config"]).contains("serverUrl"));
    f.ok(&["config", "set", "agent", "test-agent"]);
    f.ok(&["config", "set", "autoReconnect", "false"]);
    f.ok(&[
        "config",
        "set",
        "serverUrl",
        "ws://localhost:8080/ws/broker",
    ]);
    assert_eq!(
        Config::load(&f.path).unwrap().server_url,
        "ws://localhost:8080/ws/broker"
    );
    f.ok(&["config", "set", "serverUrl", "ws://localhost:8080"]);
    assert_eq!(Config::load(&f.path).unwrap().agent, "test-agent");
    assert!(!Config::load(&f.path).unwrap().auto_reconnect);
    let before = std::fs::read(&f.path).unwrap();
    for args in [
        ["config", "set", "localPort", "80"],
        ["config", "set", "localPort", "65536"],
        ["config", "set", "autoReconnect", "123"],
        ["config", "set", "serverUrl", "https://example.com"],
        ["config", "set", "serverUrl", "ws://user:password@localhost"],
        ["config", "set", "serverUrl", "ws://localhost:8080/custom"],
        [
            "config",
            "set",
            "serverUrl",
            "ws://localhost:8080/ws/broker/",
        ],
        ["config", "set", "reconnectInitialMs", "0"],
        ["config", "set", "unknown", "value"],
    ] {
        assert!(!f.run(&args).status.success());
    }
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
    assert!(!f.run(&["not-a-command"]).status.success());
    assert!(f.ok(&["--help"]).contains("start"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_validates_existing_config_without_changing_a_running_broker() {
    let f = Fixture::new();
    devlink_broker::credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    devlink_broker::credentials::set_private_permissions(&f.path).unwrap();
    f.ok(&["start"]);
    let pid = f.health().await.pid;
    let before = std::fs::read(&f.path).unwrap();
    let credential_path = devlink_broker::credentials::path(&f.path);
    let credentials = std::fs::read(&credential_path).unwrap();
    let runtime = std::fs::read(Runtime::path(&f.path)).unwrap();
    let output = f.run(&["setup", "--output", "json"]);
    assert_eq!(output.status.code(), Some(4));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["stages"][0]["status"], "succeeded");
    assert_eq!(value["stages"][0]["reused"], true);
    assert_eq!(std::fs::read(&f.path).unwrap(), before);
    assert_eq!(std::fs::read(credential_path).unwrap(), credentials);
    assert_eq!(std::fs::read(Runtime::path(&f.path)).unwrap(), runtime);
    assert_eq!(f.health().await.pid, pid);
    f.ok(&["stop"]);
}

#[test]
fn config_names_matching_internal_files_are_preserved() {
    for name in [
        "runtime.json",
        "Runtime.JSON",
        "broker.lock",
        "Broker.LOCK",
        "control.lock",
        "Control.LOCK",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let f = Fixture {
            path: dir.path().join(name),
            _dir: dir,
        };
        assert!(f.ok(&["init"]).contains("Configuration created"));
        assert!(f.ok(&["init"]).contains("already exists"));
        f.ok(&["config", "set", "agent", "custom-agent"]);
        let before = std::fs::read(&f.path).unwrap();
        let runtime = Runtime {
            api_version: local::API_VERSION,
            pid: std::process::id(),
            port: 43127,
            instance_id: "stale-instance".into(),
            control_token: "test-control-token".into(),
        };
        local::publish(&f.path, &runtime).unwrap();
        assert!(f.ok(&["status"]).contains("Stopped"));
        assert!(!Runtime::path(&f.path).exists());
        assert_eq!(std::fs::read(&f.path).unwrap(), before, "{name}: status");

        local::publish(&f.path, &runtime).unwrap();
        assert!(f.ok(&["stop"]).contains("already stopped"));
        assert!(!Runtime::path(&f.path).exists());
        assert_eq!(std::fs::read(&f.path).unwrap(), before, "{name}: stop");

        local::publish(&f.path, &runtime).unwrap();
        let output = f
            .command(&["start"])
            .env_remove("DEVLINK_TOKEN")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(text(&output).contains("DEVLINK_TOKEN"));
        assert!(!Runtime::path(&f.path).exists());
        assert_eq!(std::fs::read(&f.path).unwrap(), before, "{name}: start");
        assert!(
            config::state_directory(&f.path)
                .join("broker.lock")
                .exists()
        );
        assert!(
            config::state_directory(&f.path)
                .join("control.lock")
                .exists()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_names_matching_internal_files_survive_broker_lifecycle() {
    for name in [
        "runtime.json",
        "Runtime.JSON",
        "broker.lock",
        "control.lock",
    ] {
        let f = Fixture::with_config_name(name);
        let before = std::fs::read(&f.path).unwrap();
        f.ok(&["start"]);
        assert!(f.ok(&["status"]).contains("Running"));
        assert_eq!(std::fs::read(&f.path).unwrap(), before, "{name}: running");
        f.ok(&["stop"]);
        assert!(!Runtime::path(&f.path).exists());
        assert_eq!(std::fs::read(&f.path).unwrap(), before, "{name}: stopped");
    }
}

#[test]
fn malformed_config_does_not_echo_secrets() {
    let f = Fixture::new();
    std::fs::write(&f.path, format!(r#"{{"token":"{TOKEN}"}}"#)).unwrap();
    assert!(!f.run(&["config"]).status.success());
    std::fs::write(
        &f.path,
        format!(r#"{{"serverUrl":"ws://{TOKEN}@localhost","agent":"codex","autoReconnect":true}}"#),
    )
    .unwrap();
    assert!(!f.run(&["config"]).status.success());
    std::fs::write(
        &f.path,
        format!(r#"{{"serverUrl":"ws://localhost","agent":true,"autoReconnect":"{TOKEN}"}}"#),
    )
    .unwrap();
    assert!(!f.run(&["config"]).status.success());
}

#[test]
fn typescript_config_shape_is_accepted() {
    let config: Config = serde_json::from_str(
        r#"{"serverUrl":"ws://localhost:8080","agent":"codex","autoReconnect":true}"#,
    )
    .unwrap();
    config.validate().unwrap();
    assert_eq!(config.local_port, 43127);
}

#[test]
fn missing_token_is_reported_without_spawning() {
    let f = Fixture::new();
    let output = f
        .command(&["start"])
        .env_remove("DEVLINK_TOKEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(text(&output).contains("DEVLINK_TOKEN"));
    assert!(!Runtime::path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_lifecycle_duplicate_and_local_authentication() {
    let f = Fixture::new();
    f.ok(&["start"]); // Parent CLI has exited, broker must remain alive.
    let pid = f.health().await.pid;
    assert!(f.ok(&["status"]).contains("Running"));
    assert!(f.ok(&["start"]).contains("already running"));
    assert_eq!(f.health().await.pid, pid);
    let runtime = Runtime::load(&f.path).unwrap();
    let client = runtime.client().unwrap();
    assert_eq!(
        client
            .get(runtime.url("/health"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .post(runtime.url("/shutdown"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .post(runtime.url("/shutdown"))
            .bearer_auth(&runtime.control_token)
            .header("Origin", "https://example.org")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert!(!f.ok(&["status"]).contains(&runtime.control_token));
    f.ok(&["stop"]);
    assert!(!Runtime::path(&f.path).exists());
    assert!(f.ok(&["status"]).contains("Stopped"));
    assert!(f.ok(&["stop"]).contains("already stopped"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capture_pipes_reach_eof_after_start_cli_exits_while_broker_remains_running() {
    use std::io::Read;
    let f = Fixture::new();
    let mut parent = f
        .command(&["start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let mut stdout = parent.stdout.take().unwrap();
    let mut stderr = parent.stderr.take().unwrap();
    let output_send = send.clone();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).unwrap();
        let _ = output_send.send(bytes);
    });
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).unwrap();
        let _ = send.send(bytes);
    });
    let deadline = Instant::now()
        + devlink_broker::broker_control::startup_timeout(&f.path)
        + Duration::from_secs(10);
    let exit = loop {
        if let Some(exit) = parent.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() >= deadline {
            let _ = parent.kill();
            let _ = parent.wait();
            panic!("start CLI failed to exit");
        }
        sleep(Duration::from_millis(20)).await;
    };
    assert!(exit.success());
    for _ in 0..2 {
        let bytes = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("CLI capture pipe stayed open after parent exit");
        assert!(!String::from_utf8_lossy(&bytes).contains(TOKEN));
    }
    assert!(local::running(&f.path).unwrap());
    Runtime::load(&f.path).unwrap().health().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_start_cannot_spawn_two_brokers() {
    let f = Fixture::new();
    let mut first = f
        .command(&["start"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let second = f.run(&["start"]);
    let first_exit = first.wait().unwrap();
    assert!(first_exit.success() || second.status.success());
    let pid = f.health().await.pid;
    f.ok(&["start"]);
    assert_eq!(f.health().await.pid, pid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broker_child_tolerates_a_brief_probe_but_rejects_a_persistently_held_lock() {
    let f = Fixture::new();
    let lock_path = config::state_directory(&f.path).join("broker.lock");
    let probe = config::lock(&lock_path).unwrap();
    let mut child = OwnedProcess(
        f.command(&["broker"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    sleep(Duration::from_millis(50)).await;
    assert!(child.0.try_wait().unwrap().is_none());
    drop(probe);
    let deadline = Instant::now() + devlink_broker::broker_control::startup_timeout(&f.path);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "broker exited after a brief probe"
        );
        if let Ok(runtime) = Runtime::load(&f.path)
            && runtime.health().await.is_ok()
        {
            break;
        }
        assert!(Instant::now() < deadline, "broker failed to become ready");
        sleep(Duration::from_millis(20)).await;
    }
    f.ok(&["stop"]);
    assert!(child.0.wait().unwrap().success());
    let _probe = config::lock(&lock_path).unwrap();
    let mut blocked = OwnedProcess(
        f.command(&["broker"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let exit = loop {
        if let Some(exit) = blocked.0.try_wait().unwrap() {
            break exit;
        }
        assert!(
            Instant::now() < deadline,
            "persistent lock did not fail within the bounded retry"
        );
        sleep(Duration::from_millis(20)).await;
    };
    assert!(!exit.success());
    let mut diagnostic = String::new();
    use std::io::Read;
    blocked
        .0
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut diagnostic)
        .unwrap();
    assert!(diagnostic.contains("Another DevLink operation is running"));
    assert!(!diagnostic.contains(TOKEN));
    assert!(!Runtime::path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crash_releases_os_lock_and_stale_runtime_is_recovered() {
    let f = Fixture::new();
    let mut child = OwnedProcess(f.command(&["broker"]).spawn().unwrap());
    let startup_timeout =
        Duration::from_secs(8) + devlink_broker::credentials::startup_validation_allowance();
    let deadline = Instant::now() + startup_timeout;
    loop {
        if let Ok(runtime) = Runtime::load(&f.path)
            && runtime.health().await.is_ok()
        {
            break;
        }
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(30)).await;
    }
    let old_instance = Runtime::load(&f.path).unwrap().instance_id;
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    assert!(Runtime::path(&f.path).exists());
    assert!(f.ok(&["status"]).contains("Stopped"));
    assert!(!Runtime::path(&f.path).exists());
    f.ok(&["start"]);
    assert_ne!(f.health().await.instance_id, old_instance);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn occupied_port_fails_and_releases_lock() {
    let other = Fixture::new();
    other.ok(&["start"]);
    let first = other.health().await;
    let f = Fixture::new();
    let occupied = Runtime::load(&other.path).unwrap().port;
    f.configure(|config| config.local_port = occupied);
    let output = f.run(&["start"]);
    assert!(!output.status.success());
    assert!(
        text(&output).contains("port is in use or the OS denied the bind"),
        "{}",
        text(&output)
    );
    assert!(text(&output).contains("config set localPort <FREE_PORT>"));
    assert!(!devlink_broker::local::running(&f.path).unwrap());
    f.ok(&["config", "set", "localPort", "0"]);
    f.ok(&["start"]);
    assert_ne!(Runtime::load(&f.path).unwrap().port, occupied);
    assert_eq!(Config::load(&f.path).unwrap().local_port, 0);
    assert_eq!(other.health().await.instance_id, first.instance_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unhealthy_live_broker_is_not_reported_as_stopped() {
    let f = Fixture::new();
    f.ok(&["start"]);
    let mut runtime = Runtime::load(&f.path).unwrap();
    let original = runtime.control_token.clone();
    runtime.control_token = "wrong-token".into();
    config::atomic_json(&Runtime::path(&f.path), &runtime).unwrap();
    let output = f.run(&["status"]);
    runtime.control_token = original;
    config::atomic_json(&Runtime::path(&f.path), &runtime).unwrap();
    assert!(!output.status.success());
    assert!(!text(&output).contains("Stopped"));
}

#[derive(Clone, Copy)]
enum Behavior {
    Ready { agent_id: u64 },
    VerifiedReady,
    Deny401,
    Deny403,
    Silent,
    InvalidJson,
    EmptyConnectionId,
    ZeroAgentId,
    LegacyAuthOk,
    CloseHeartbeatTimeout4001,
    CloseReplaced4002,
    ClosePolicyViolation1008,
}

struct Mock {
    address: std::net::SocketAddr,
    attempts: Arc<AtomicUsize>,
    received: Arc<Mutex<Vec<String>>>,
    heartbeat_pings: Arc<Mutex<Vec<(u64, String, Instant)>>>,
    expirations: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Mock {
    async fn start(address: &str, behavior: Behavior) -> Self {
        let listener = TcpListener::bind(address).await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let received = Arc::new(Mutex::new(Vec::new()));
        let heartbeat_pings = Arc::new(Mutex::new(Vec::new()));
        let expirations = Arc::new(AtomicUsize::new(0));
        let attempts_clone = attempts.clone();
        let received_clone = received.clone();
        let heartbeat_pings_clone = heartbeat_pings.clone();
        let expirations_clone = expirations.clone();
        let next_seq = Arc::new(AtomicU64::new(1));

        let task = tokio::spawn(async move {
            let mut clients = JoinSet::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let attempts = attempts_clone.clone();
                let received = received_clone.clone();
                let heartbeat_pings = heartbeat_pings_clone.clone();
                let expirations = expirations_clone.clone();
                let next_seq = next_seq.clone();

                clients.spawn(async move {
                    if serve_context_request(&mut stream, if matches!(behavior, Behavior::VerifiedReady) {Some(11)} else {None}).await {return;}
                    // Number only WebSocket attempts; HTTP lookups may arrive first.
                    let seq = next_seq.fetch_add(1, Ordering::SeqCst);
                    attempts.fetch_add(1, Ordering::SeqCst);
                    let handshake = accept_hdr_async(
                        stream,
                        move |req: &Request,
                              response: Response|
                              -> Result<Response, ErrorResponse> {
                            assert_eq!(
                                req.uri().path(),
                                "/ws/broker",
                                "Upgrade request path must be /ws/broker"
                            );
                            assert!(
                                req.uri().query().is_none(),
                                "Upgrade request must not carry query parameters"
                            );
                            assert!(
                                req.headers().get("origin").is_none(),
                                "Upgrade request must not include Origin header"
                            );
                            assert!(
                                req.headers().get("cookie").is_none(),
                                "Upgrade request must not include Cookie header"
                            );
                            let auth = req
                                .headers()
                                .get(AUTHORIZATION)
                                .and_then(|v| v.to_str().ok());
                            assert!(
                                auth == Some(&format!("Bearer {TOKEN}")),
                                "Upgrade request must carry Authorization: Bearer token"
                            );

                            match behavior {
                                Behavior::Deny401 => {
                                    let mut err = ErrorResponse::new(None);
                                    *err.status_mut() = StatusCode::UNAUTHORIZED;
                                    Err(err)
                                }
                                Behavior::Deny403 => {
                                    let mut err = ErrorResponse::new(None);
                                    *err.status_mut() = StatusCode::FORBIDDEN;
                                    Err(err)
                                }
                                _ => Ok(response),
                            }
                        },
                    )
                    .await;

                    let Ok(mut socket) = handshake else {
                        return;
                    };

                    let ready_text = match behavior {
                        Behavior::VerifiedReady => format!(
                            r#"{{"type":"BROKER_READY","payload":{{"connection_id":"conn-{seq}","agent_id":11,"user_id":3,"team_id":1}}}}"#
                        ),
                        Behavior::Ready { agent_id } => {
                            format!(
                                r#"{{"type":"BROKER_READY","payload":{{"connection_id":"conn-{seq}","agent_id":{agent_id}}}}}"#
                            )
                        }
                        Behavior::CloseHeartbeatTimeout4001 | Behavior::CloseReplaced4002 | Behavior::ClosePolicyViolation1008 => {
                            format!(
                                r#"{{"type":"BROKER_READY","payload":{{"connection_id":"conn-{seq}","agent_id":11}}}}"#
                            )
                        }
                        Behavior::Silent => {
                            std::future::pending::<()>().await;
                            return;
                        }
                        Behavior::InvalidJson => "malformed-json".to_string(),
                        Behavior::EmptyConnectionId => {
                            r#"{"type":"BROKER_READY","payload":{"connection_id":"","agent_id":11}}"#
                                .to_string()
                        }
                        Behavior::ZeroAgentId => {
                            r#"{"type":"BROKER_READY","payload":{"connection_id":"conn-0","agent_id":0}}"#
                                .to_string()
                        }
                        Behavior::LegacyAuthOk => r#"{"type":"auth_ok"}"#.to_string(),
                        Behavior::Deny401 | Behavior::Deny403 => unreachable!(),
                    };

                    if socket.send(Message::Text(ready_text.into())).await.is_err() {
                        return;
                    }

                    match behavior {
                        Behavior::CloseReplaced4002 => {
                            let _ = socket
                                .close(Some(CloseFrame {
                                    code: 4002.into(),
                                    reason: "Replaced by newer connection".into(),
                                }))
                                .await;
                            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                                while let Some(Ok(msg)) = socket.next().await {
                                    if msg.is_close() {
                                        break;
                                    }
                                }
                            })
                            .await;
                            return;
                        }
                        Behavior::ClosePolicyViolation1008 => {
                            let _ = socket
                                .close(Some(CloseFrame {
                                    code: 1008.into(),
                                    reason: "Invalid message".into(),
                                }))
                                .await;
                            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                                while let Some(Ok(msg)) = socket.next().await {
                                    if msg.is_close() {
                                        break;
                                    }
                                }
                            })
                            .await;
                            return;
                        }
                        _ => {}
                    }

                    if matches!(behavior, Behavior::CloseHeartbeatTimeout4001) && seq == 1 {
                        let _ = socket.close(Some(CloseFrame {
                            code: 4001.into(),
                            reason: "Heartbeat timeout".into(),
                        })).await;
                        return;
                    }

                    // 실제 중앙처럼 JSON PING만 40초 무수신 만료를 연장합니다.
                    let heartbeat_expiration = sleep(Duration::from_secs(40));
                    tokio::pin!(heartbeat_expiration);
                    loop {
                        let message = tokio::select! {
                            _ = &mut heartbeat_expiration => {
                                expirations.fetch_add(1, Ordering::SeqCst);
                                let _ = socket.close(Some(CloseFrame {
                                    code: 4001.into(),
                                    reason: "Heartbeat timeout".into(),
                                })).await;
                                break;
                            }
                            message = socket.next() => message,
                        };
                        let Some(Ok(message)) = message else { break };
                        match message {
                            Message::Text(text) => {
                                if let Ok(ClientMessage::Heartbeat(HeartbeatPing::Ping { ping_id })) =
                                    serde_json::from_str::<ClientMessage>(&text)
                                {
                                    if ping_id.trim().is_empty() || ping_id.encode_utf16().count() > 128 {
                                        let _ = socket.close(Some(CloseFrame {
                                            code: 1008.into(),
                                            reason: "Invalid HEARTBEAT PING".into(),
                                        })).await;
                                        break;
                                    }
                                    let received_at = Instant::now();
                                    heartbeat_expiration.as_mut().reset(received_at + Duration::from_secs(40));
                                    heartbeat_pings.lock().await.push((seq, ping_id.clone(), received_at));
                                    let pong = serde_json::to_string(&ServerMessage::heartbeat_pong(ping_id)).unwrap();
                                    if socket.send(Message::Text(pong.into())).await.is_err() {
                                        break;
                                    }
                                    continue;
                                }
                                received.lock().await.push(text.to_string());
                            }
                            Message::Ping(payload) => {
                                if socket.send(Message::Pong(payload)).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                });
                while clients.try_join_next().is_some() {}
            }
        });
        Self {
            address,
            attempts,
            received,
            heartbeat_pings,
            expirations,
            task,
        }
    }
    async fn stop(self) {
        self.task.abort();
        // Await cancellation so both listening socket and accepted sockets have been dropped.
        let _ = self.task.await;
    }

    async fn wait_messages(&self, count: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let messages = self.received.lock().await;
            if messages.len() >= count {
                return messages
                    .iter()
                    .map(|message| serde_json::from_str(message).unwrap())
                    .collect();
            }
            drop(messages);
            assert!(Instant::now() < deadline, "Timed out waiting for presence");
            sleep(Duration::from_millis(10)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_auth_disconnect_restart_and_reconnect() {
    let server = Mock::start("127.0.0.1:0", Behavior::Ready { agent_id: 11 }).await;
    let f = Fixture::new();
    // 1. Root URL (`ws://<addr>`) is normalized to `/ws/broker`
    f.configure(|config| config.server_url = format!("ws://{}", server.address));
    f.ok(&["start"]);
    f.wait_status(ConnectionStatus::Connected).await;
    assert!(f.ok(&["status"]).contains("Connected"));

    // In default CLI broker start, local Agent state is unconfirmed (`None`),
    // so the broker must NOT guess or send `AGENT_PRESENCE` (`ONLINE` or `OFFLINE`),
    // and must never send legacy `auth`/`hello`.
    sleep(Duration::from_millis(120)).await;
    assert!(
        server.received.lock().await.is_empty(),
        "Broker must not send guessed AGENT_PRESENCE when local Agent status is unknown"
    );
    assert_eq!(
        f.health().await.central_server,
        ConnectionStatus::Connected,
        "Central WebSocket must remain Connected after BROKER_READY when local status is unknown"
    );

    // Ensure agent_id is NOT persisted to broker.json in Stage 2
    let cred_raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(devlink_broker::credentials::path(&f.path)).unwrap())
            .unwrap();
    assert!(
        cred_raw.get("agentId").is_none() && cred_raw.get("agent_id").is_none(),
        "broker.json must not persist agent_id in Stage 2"
    );

    let address = server.address;
    server.stop().await;
    f.wait_status(ConnectionStatus::Reconnecting).await;

    let server = Mock::start(&address.to_string(), Behavior::Ready { agent_id: 11 }).await;
    f.wait_status(ConnectionStatus::Connected).await;
    assert!(
        server.received.lock().await.is_empty(),
        "Reconnect with unknown local Agent status must not send guessed AGENT_PRESENCE"
    );
    f.ok(&["stop"]);

    // 2. Explicit `/ws/broker` URL (`ws://<addr>/ws/broker`) also connects to `/ws/broker`
    f.configure(|config| config.server_url = format!("ws://{}/ws/broker", server.address));
    f.ok(&["start"]);
    f.wait_status(ConnectionStatus::Connected).await;
    f.ok(&["stop"]);

    // 3. Disallowed custom path (`ws://<addr>/other`) is rejected before opening a socket;
    //    the Mock server receives 0 additional attempts so the Bearer token is never sent to `/other`.
    let attempts_before = server.attempts.load(Ordering::SeqCst);
    let bad_config = Config {
        server_url: format!("ws://{}/other", server.address),
        ..Config::default()
    };
    let bad_status = Arc::new(RwLock::new(ConnectionStatus::Connecting));
    let bad_state = CentralState::new(bad_status);
    let bad_cancel = CancellationToken::new();
    central::run_with_state(bad_config, TOKEN.to_string(), bad_state.clone(), bad_cancel).await;
    assert_eq!(
        bad_state.status().await,
        ConnectionStatus::ProtocolViolation
    );
    assert_eq!(
        server.attempts.load(Ordering::SeqCst),
        attempts_before,
        "Bearer token must never be transmitted to a disallowed serverUrl path"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_heartbeat_keeps_cli_connected_for_45_seconds_without_reconnect() {
    let server = Mock::start("127.0.0.1:0", Behavior::Ready { agent_id: 11 }).await;
    let f = Fixture::new();
    f.configure(|config| config.server_url = format!("ws://{}", server.address));
    f.ok(&["start"]);
    f.wait_status(ConnectionStatus::Connected).await;
    let initial = f.health().await;
    let started = Instant::now();

    // 시간을 단축하지 않고 실제 CLI의 JSON PING 20초·PONG 대기 20초와
    // Mock의 40초 무수신 만료를 사용합니다. 재접속으로 연결이 바뀌어도 실패합니다.
    loop {
        let health = f.health().await;
        assert_eq!(health.central_server, ConnectionStatus::Connected);
        assert_eq!(health.pid, initial.pid);
        assert_eq!(health.instance_id, initial.instance_id);
        assert_eq!(
            server.attempts.load(Ordering::SeqCst),
            1,
            "Must keep the original WebSocket connection"
        );
        assert_eq!(server.expirations.load(Ordering::SeqCst), 0);
        if started.elapsed() >= Duration::from_secs(45) {
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }

    {
        let pings = server.heartbeat_pings.lock().await;
        assert!(
            pings.len() >= 2,
            "Expected production JSON PINGs around 20 and 40 seconds"
        );
        assert!(pings.iter().all(|(seq, _, _)| *seq == 1));
        assert_ne!(
            pings[0].1, pings[1].1,
            "Each heartbeat must use a new ping_id"
        );
        assert!(pings[1].2.duration_since(pings[0].2) >= Duration::from_secs(19));
    }
    assert!(
        server.received.lock().await.is_empty(),
        "Heartbeat must not guess local Agent presence"
    );
    f.ok(&["stop"]);
    assert!(!local::running(&f.path).unwrap());
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_routes_require_verified_identity_and_reject_cli_control_token() {
    let server = Mock::start("127.0.0.1:0", Behavior::Ready { agent_id: 11 }).await;
    let f = Fixture::new();
    f.configure(|config| config.server_url = format!("ws://{}", server.address));
    f.ok(&["start"]);
    f.wait_status(ConnectionStatus::Connected).await;
    let runtime = Runtime::load(&f.path).unwrap();
    let credentials = devlink_broker::credentials::load(&f.path).unwrap();
    let client = runtime.client().unwrap();
    let payload = serde_json::json!({"requestId":"production-claim","instanceId":"mcp-instance","mode":"fresh"});
    let response = client
        .post(runtime.url("/v1/sessions"))
        .bearer_auth(&credentials.local_token)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 503);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"requestId":"production-claim","error":{"code":"CENTRAL_UNAUTHENTICATED","scope":"domain","outcome":"rejected"}})
    );
    assert!(!body.to_string().contains(&credentials.local_token));
    let response = client
        .post(runtime.url("/v1/sessions"))
        .bearer_auth(&runtime.control_token)
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);
    // The earlier context REST denial invalidated the central principal;
    // rejecting a CLI control token must not restore it to Connected.
    assert_eq!(
        f.health().await.central_server,
        ConnectionStatus::AuthFailed
    );
    assert!(
        server.received.lock().await.is_empty(),
        "Rejected claims must not activate presence"
    );
    f.ok(&["stop"]);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_uses_ready_identity_for_real_local_sessions() {
    let server = Mock::start("127.0.0.1:0", Behavior::VerifiedReady).await;
    let f = Fixture::new();
    f.configure(|config| config.server_url = format!("ws://{}", server.address));
    f.ok(&["start"]);
    f.wait_status(ConnectionStatus::Connected).await;
    let runtime = Runtime::load(&f.path).unwrap();
    let credentials = devlink_broker::credentials::load(&f.path).unwrap();
    let client = runtime.client().unwrap();
    let response = client.post(runtime.url("/v1/sessions"))
        .bearer_auth(&credentials.local_token)
        .json(&serde_json::json!({"requestId":"verified-claim","instanceId":"verified-host","mode":"fresh","agentId":"11"}))
        .send().await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        body["data"]["principal"],
        serde_json::json!({"userId":"3","teamId":"1","agentId":"11"})
    );
    let session = body["data"]["sessionId"].as_str().unwrap();
    assert!(!body.to_string().contains(&credentials.local_token));
    let messages = server.wait_messages(1).await;
    assert_eq!(
        messages[0],
        serde_json::json!({"type":"AGENT_PRESENCE","payload":{"status":"ONLINE"}})
    );
    let response = client.post(runtime.url("/v1/sessions"))
        .bearer_auth(&credentials.local_token)
        .json(&serde_json::json!({"requestId":"wrong-agent","instanceId":"other-host","mode":"fresh","agentId":"12"}))
        .send().await.unwrap();
    assert_eq!(response.status().as_u16(), 422);
    let response = client
        .post(runtime.url(&format!("/v1/sessions/{session}/heartbeat")))
        .bearer_auth(&credentials.local_token)
        .json(&serde_json::json!({"requestId":"verified-renew","instanceId":"verified-host"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let response = client
        .delete(runtime.url(&format!("/v1/sessions/{session}")))
        .bearer_auth(&credentials.local_token)
        .json(&serde_json::json!({"requestId":"verified-release","instanceId":"verified-host"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    let messages = server.wait_messages(2).await;
    assert_eq!(
        messages[1],
        serde_json::json!({"type":"AGENT_PRESENCE","payload":{"status":"OFFLINE"}})
    );
    f.ok(&["stop"]);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heartbeat_timeout_4001_reconnects_only_when_enabled() {
    for auto_reconnect in [true, false] {
        let server = Mock::start("127.0.0.1:0", Behavior::CloseHeartbeatTimeout4001).await;
        let f = Fixture::new();
        f.configure(|config| {
            config.server_url = format!("ws://{}", server.address);
            config.auto_reconnect = auto_reconnect;
        });
        f.ok(&["start"]);
        if auto_reconnect {
            let deadline = Instant::now() + Duration::from_secs(5);
            while server.attempts.load(Ordering::SeqCst) < 2 {
                assert!(Instant::now() < deadline, "4001 must reconnect");
                sleep(Duration::from_millis(10)).await;
            }
            f.wait_status(ConnectionStatus::Connected).await;
            assert_eq!(server.attempts.load(Ordering::SeqCst), 2);
        } else {
            f.wait_status(ConnectionStatus::Disconnected).await;
            sleep(Duration::from_millis(200)).await;
            assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
        }
        f.ok(&["stop"]);
        server.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authentication_rejection_401_and_403_stop_retries() {
    for behavior in [Behavior::Deny401, Behavior::Deny403] {
        let server = Mock::start("127.0.0.1:0", behavior).await;
        let f = Fixture::new();
        f.configure(|config| config.server_url = format!("ws://{}", server.address));
        f.ok(&["start"]);
        f.wait_status(ConnectionStatus::AuthFailed).await;
        assert_eq!(
            server.attempts.load(Ordering::SeqCst),
            1,
            "Auth failure must not auto-retry"
        );
        assert!(
            server.received.lock().await.is_empty(),
            "Client must not send messages on auth failure"
        );

        let address = server.address;
        server.stop().await;
        let server = Mock::start(&address.to_string(), Behavior::Ready { agent_id: 11 }).await;
        sleep(Duration::from_millis(300)).await;
        assert_eq!(
            f.health().await.central_server,
            ConnectionStatus::AuthFailed
        );
        assert_eq!(
            server.attempts.load(Ordering::SeqCst),
            0,
            "Broker must remain stopped from retrying after AuthFailed"
        );
        f.ok(&["stop"]);
        server.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_4002_and_policy_violation_1008_stop_retries_with_distinct_status() {
    for (behavior, expected_status) in [
        (Behavior::CloseReplaced4002, ConnectionStatus::Replaced),
        (
            Behavior::ClosePolicyViolation1008,
            ConnectionStatus::ProtocolViolation,
        ),
    ] {
        let server = Mock::start("127.0.0.1:0", behavior).await;
        let f = Fixture::new();
        f.configure(|config| config.server_url = format!("ws://{}", server.address));
        f.ok(&["start"]);
        f.wait_status(expected_status).await;
        assert!(
            f.ok(&["status"]).contains(&format!("{expected_status:?}")),
            "CLI status must report {expected_status:?} distinctly"
        );
        assert_eq!(
            server.attempts.load(Ordering::SeqCst),
            1,
            "{expected_status:?} must stop auto-retries"
        );

        let address = server.address;
        server.stop().await;
        let server = Mock::start(&address.to_string(), Behavior::Ready { agent_id: 11 }).await;
        sleep(Duration::from_millis(300)).await;
        assert_eq!(f.health().await.central_server, expected_status);
        assert_eq!(
            server.attempts.load(Ordering::SeqCst),
            0,
            "No reconnect attempt must occur after {expected_status:?}"
        );
        f.ok(&["stop"]);
        server.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_broker_ready_is_rejected_without_connected_presence_or_retry() {
    for behavior in [
        Behavior::InvalidJson,
        Behavior::EmptyConnectionId,
        Behavior::ZeroAgentId,
        Behavior::LegacyAuthOk,
    ] {
        let server = Mock::start("127.0.0.1:0", behavior).await;
        let f = Fixture::new();
        f.configure(|config| {
            config.server_url = format!("ws://{}", server.address);
            config.auto_reconnect = true;
        });
        f.ok(&["start"]);
        f.wait_status(ConnectionStatus::ProtocolViolation).await;
        assert!(
            server.received.lock().await.is_empty(),
            "AGENT_PRESENCE must never be sent when BROKER_READY is invalid"
        );
        sleep(Duration::from_millis(250)).await;
        assert_eq!(
            server.attempts.load(Ordering::SeqCst),
            1,
            "Invalid BROKER_READY must stop auto-retry"
        );
        f.ok(&["stop"]);
        server.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn silent_broker_ready_waits_in_authenticating_then_retries_or_disconnects() {
    // 1. auto_reconnect = false: transitions to Disconnected on BROKER_READY timeout without retry
    let server = Mock::start("127.0.0.1:0", Behavior::Silent).await;
    let f = Fixture::new();
    f.configure(|config| {
        config.server_url = format!("ws://{}", server.address);
        config.auto_reconnect = false;
        config.auth_timeout_ms = 200;
    });
    f.ok(&["start"]);
    f.wait_status(ConnectionStatus::Disconnected).await;
    assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
    f.ok(&["stop"]);
    server.stop().await;

    // 2. auto_reconnect = true: retries after BROKER_READY timeout
    let server = Mock::start("127.0.0.1:0", Behavior::Silent).await;
    let f2 = Fixture::new();
    f2.configure(|config| {
        config.server_url = format!("ws://{}", server.address);
        config.auto_reconnect = true;
        config.auth_timeout_ms = 150;
    });
    f2.ok(&["start"]);
    let retry_deadline = Instant::now() + Duration::from_secs(5);
    while server.attempts.load(Ordering::SeqCst) < 2 {
        assert!(
            Instant::now() < retry_deadline,
            "Expected retry after BROKER_READY timeout"
        );
        sleep(Duration::from_millis(30)).await;
    }
    f2.ok(&["stop"]);
    server.stop().await;

    // 3. Upgrade alone stays in Authenticating (not Connected) and stops promptly during BROKER_READY wait
    let server = Mock::start("127.0.0.1:0", Behavior::Silent).await;
    let f3 = Fixture::new();
    f3.configure(|config| {
        config.server_url = format!("ws://{}", server.address);
        config.auto_reconnect = true;
        config.auth_timeout_ms = 5000;
    });
    f3.ok(&["start"]);
    f3.wait_status(ConnectionStatus::Authenticating).await;
    let stop_started = Instant::now();
    f3.ok(&["stop"]);
    assert!(
        stop_started.elapsed() < Duration::from_secs(5),
        "Broker must stop promptly while waiting for BROKER_READY"
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn central_state_distinguishes_unknown_confirmed_offline_and_confirmed_online_presence() {
    let server = Mock::start("127.0.0.1:0", Behavior::Ready { agent_id: 42 }).await;
    let config = Config {
        server_url: format!("ws://{}", server.address),
        reconnect_initial_ms: 50,
        reconnect_max_ms: 100,
        auth_timeout_ms: 500,
        ..Config::default()
    };

    let status = Arc::new(RwLock::new(ConnectionStatus::Connecting));
    let identity = Arc::new(RwLock::new(None));
    // Case 1: Local Agent status is unknown (`None`) -> must NOT send guessed AGENT_PRESENCE
    let presence = tokio::sync::watch::channel(None).0;
    let state = CentralState::with_details(status.clone(), identity.clone(), presence.clone());
    let cancel = CancellationToken::new();

    let runner = tokio::spawn(central::run_with_state(
        config,
        TOKEN.to_string(),
        state.clone(),
        cancel.clone(),
    ));

    let deadline = Instant::now() + Duration::from_secs(5);
    while state.status().await != ConnectionStatus::Connected {
        assert!(Instant::now() < deadline, "Timed out waiting for Connected");
        sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        state.identity().await,
        Some(CentralIdentity {
            connection_id: "conn-1".into(),
            agent_id: 42,
            user_id: None,
            team_id: None,
        })
    );
    sleep(Duration::from_millis(80)).await;
    assert!(
        server.received.lock().await.is_empty(),
        "Unknown local Agent status (None) must not send guessed AGENT_PRESENCE"
    );

    // Case 2: Local Agent status is confirmed OFFLINE (`Some(PresenceStatus::Offline)`)
    let address = server.address;
    server.stop().await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while state.status().await != ConnectionStatus::Reconnecting {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for Reconnecting"
        );
        sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        state.identity().await,
        None,
        "Previous connection_id and agent_id must be invalidated on disconnect"
    );
    // 재접속 대기 중에도 최신 상태만 보존하고 다음 READY 뒤에 보고합니다.
    state.set_presence(PresenceStatus::Online);
    state.set_presence(PresenceStatus::Offline);

    let server = Mock::start(&address.to_string(), Behavior::Ready { agent_id: 42 }).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.status().await != ConnectionStatus::Connected {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for reconnect Connected (Offline)"
        );
        sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        state.identity().await,
        Some(CentralIdentity {
            connection_id: "conn-1".into(),
            agent_id: 42,
            user_id: None,
            team_id: None,
        })
    );
    {
        let msgs = server.wait_messages(1).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0],
            serde_json::json!({
                "type": "AGENT_PRESENCE",
                "payload": {
                    "status": "OFFLINE"
                }
            })
        );
    }

    // Case 3: Local Agent status is confirmed ONLINE (`Some(PresenceStatus::Online)`)
    server.stop().await;

    let deadline = Instant::now() + Duration::from_secs(5);
    while state.status().await != ConnectionStatus::Reconnecting {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for second Reconnecting"
        );
        sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(state.identity().await, None);
    state.set_presence(PresenceStatus::Online);

    let server = Mock::start(&address.to_string(), Behavior::Ready { agent_id: 99 }).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.status().await != ConnectionStatus::Connected {
        assert!(
            Instant::now() < deadline,
            "Timed out waiting for reconnect Connected (Online)"
        );
        sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        state.identity().await,
        Some(CentralIdentity {
            connection_id: "conn-1".into(),
            agent_id: 99,
            user_id: None,
            team_id: None,
        })
    );
    {
        let msgs = server.wait_messages(1).await;
        assert_eq!(msgs.len(), 1);
        assert_eq!(
            msgs[0],
            serde_json::json!({
                "type": "AGENT_PRESENCE",
                "payload": {
                    "status": "ONLINE"
                }
            })
        );
    }

    cancel.cancel();
    runner.await.unwrap();
    assert_eq!(state.status().await, ConnectionStatus::Disconnected);
    assert_eq!(state.identity().await, None);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_changes_use_the_current_connection_without_duplicates_or_ack() {
    let server = Mock::start("127.0.0.1:0", Behavior::Ready { agent_id: 42 }).await;
    let config = Config {
        server_url: format!("ws://{}", server.address),
        ..Config::default()
    };
    let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(central::run_with_state(
        config,
        TOKEN.into(),
        state.clone(),
        cancel.clone(),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.status().await != ConnectionStatus::Connected {
        assert!(Instant::now() < deadline, "Timed out waiting for Connected");
        sleep(Duration::from_millis(10)).await;
    }
    // 로컬 상태 미확인 시에는 READY만으로 ONLINE/OFFLINE을 만들지 않습니다.
    assert!(server.received.lock().await.is_empty());
    state.set_presence(PresenceStatus::Online);
    server.wait_messages(1).await;
    state.set_presence(PresenceStatus::Online);
    state.set_presence(PresenceStatus::Offline);
    let messages = server.wait_messages(2).await;
    assert_eq!(
        messages,
        vec![
            serde_json::json!({"type":"AGENT_PRESENCE","payload":{"status":"ONLINE"}}),
            serde_json::json!({"type":"AGENT_PRESENCE","payload":{"status":"OFFLINE"}}),
        ]
    );
    assert_eq!(server.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(state.status().await, ConnectionStatus::Connected);

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
    state.set_presence(PresenceStatus::Online);
    assert_eq!(server.received.lock().await.len(), 2);
    assert_eq!(state.identity().await, None);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_waits_for_ready_and_reports_only_the_latest_confirmed_state() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (messages_tx, mut messages_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        accepted_tx.send(()).unwrap();
        tokio::select! {
            _ = release_rx => {},
            message = socket.next() => panic!("Unexpected frame before READY: {message:?}"),
        }
        socket
            .send(Message::Text(
                r#"{"type":"BROKER_READY","payload":{"connection_id":"gated","agent_id":42}}"#
                    .into(),
            ))
            .await
            .unwrap();
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Text(text) => messages_tx
                    .send(serde_json::from_str::<serde_json::Value>(&text).unwrap())
                    .unwrap(),
                Message::Ping(payload) => socket.send(Message::Pong(payload)).await.unwrap(),
                Message::Close(_) => break,
                _ => {}
            }
        }
    });
    let config = Config {
        server_url: format!("ws://{address}"),
        ..Config::default()
    };
    let state = CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)));
    // 수신자가 생기기 전 확인한 상태도 다음 READY까지 유지해야 합니다.
    state.set_presence(PresenceStatus::Online);
    let cancel = CancellationToken::new();
    let runner = tokio::spawn(central::run_with_state(
        config,
        TOKEN.into(),
        state.clone(),
        cancel.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(3), accepted_rx)
        .await
        .unwrap()
        .unwrap();
    state.set_presence(PresenceStatus::Offline);
    // READY가 보류된 동안에는 presence도 보류합니다.
    assert!(
        tokio::time::timeout(Duration::from_millis(80), messages_rx.recv())
            .await
            .is_err()
    );
    release_tx.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), messages_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        serde_json::json!({"type":"AGENT_PRESENCE","payload":{"status":"OFFLINE"}})
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(80), messages_rx.recv())
            .await
            .is_err()
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(3), runner)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreachable_server_and_stop_during_backoff() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let f = Fixture::new();
    f.configure(|config| {
        config.server_url = format!("ws://{address}");
        config.reconnect_initial_ms = 30000;
        config.reconnect_max_ms = 30000;
    });
    f.ok(&["start"]);
    f.wait_status(ConnectionStatus::Reconnecting).await;
    let started = Instant::now();
    f.ok(&["stop"]);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_setup_port_and_mcp_local_token_survive_normal_restart() {
    // 1. 기존 테스트 fixture를 활용해 임시 설정을 만든다.
    let f = Fixture::new();
    #[cfg(windows)]
    let _default_port = match std::net::TcpListener::bind(("127.0.0.1", 43127)) {
        Ok(listener) => Some(listener),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::AddrInUse | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            None
        }
        Err(error) => panic!("Cannot verify the preferred Windows port: {error}"),
    };

    // 2. 새 setup 프로필처럼 첫 기동에서 바인딩한 포트를 저장하도록 한다.
    devlink_broker::credentials::set_private_permissions(f.path.parent().unwrap()).unwrap();
    f.configure(|config| {
        config.local_port = 0;
        config.local_port_pending = true;
    });

    // 3. 실제 CLI start로 브로커를 시작한다.
    f.ok(&["start"]);

    // 4. Rust가 발행한 MCP 자격에서 brokerUrl/localToken을 메모리에 보관한다.
    //    runtime의 instanceId도 보관한다.
    let creds1 = devlink_broker::credentials::load(&f.path).unwrap();
    let token1 = creds1.local_token;
    let broker_url1 = creds1.broker_url;

    let runtime1 = Runtime::load(&f.path).unwrap();
    let saved = Config::load(&f.path).unwrap();
    assert_eq!(saved.local_port, runtime1.port);
    assert_ne!(saved.local_port, 0);
    #[cfg(windows)]
    {
        assert_ne!(saved.local_port, 43127);
        assert!((1024..=49151).contains(&saved.local_port));
    }
    #[cfg(target_os = "linux")]
    {
        let configured = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
        let mut endpoints = configured
            .split_whitespace()
            .map(|port| port.parse::<u16>().unwrap());
        let first = endpoints.next().unwrap();
        let last = endpoints.next().unwrap();
        assert!(!(first..=last).contains(&saved.local_port));
    }
    assert!(!saved.local_port_pending);
    devlink_broker::credentials::verify_private_permissions(&f.path).unwrap();
    let saved_bytes = std::fs::read(&f.path).unwrap();
    let instance_id1 = runtime1.instance_id;

    // 5. 실제 CLI stop으로 종료한다.
    f.ok(&["stop"]);

    // 6. MCP 자격 파일이 남아 있고 토큰이 유지되는지 확인한다.
    let cred_path = devlink_broker::credentials::path(&f.path);
    assert!(
        cred_path.exists(),
        "MCP credential file must remain after stop"
    );
    let creds_after_stop = devlink_broker::credentials::load(&f.path).unwrap();
    assert!(
        creds_after_stop.local_token == token1,
        "MCP token changed after stop"
    );

    // 7. 같은 설정으로 다시 start한다.
    f.ok(&["start"]);

    // 8. 다음을 확인한다.
    let creds2 = devlink_broker::credentials::load(&f.path).unwrap();
    // - localToken 동일 (비밀값 노출 방지를 위해 assert! 사용)
    assert!(
        creds2.local_token == token1,
        "MCP token changed after restart"
    );
    // - brokerUrl 동일
    assert!(
        creds2.broker_url == broker_url1,
        "MCP brokerUrl changed after restart"
    );
    // - runtime instanceId 변경
    let runtime2 = Runtime::load(&f.path).unwrap();
    assert_eq!(runtime2.port, runtime1.port);
    assert_eq!(std::fs::read(&f.path).unwrap(), saved_bytes);
    assert!(
        runtime2.instance_id != instance_id1,
        "Runtime instanceId must change after restart"
    );

    // 9. stop으로 종료하고 테스트 프로세스를 정리한다.
    f.ok(&["stop"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_local_token_survives_crash_restart() {
    // 1. 기존 Fixture와 OwnedProcess를 사용한다.
    let f = Fixture::new();

    // 2. 임시 설정에 사용 가능한 고정 포트를 지정한다.
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);

    f.configure(|config| {
        config.local_port = port;
    });

    // 3. broker를 직접 자식 프로세스로 실행하고 Health 준비를 기다린다.
    let mut child = OwnedProcess(f.command(&["broker"]).spawn().unwrap());
    let startup_timeout =
        Duration::from_secs(8) + devlink_broker::credentials::startup_validation_allowance();
    let deadline = Instant::now() + startup_timeout;
    loop {
        if let Ok(runtime) = Runtime::load(&f.path)
            && runtime.health().await.is_ok()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Broker failed to become healthy before deadline"
        );
        sleep(Duration::from_millis(30)).await;
    }

    // 4. MCP 자격의 localToken·brokerUrl과 runtime instanceId를 보관한다.
    let creds1 = devlink_broker::credentials::load(&f.path).unwrap();
    let token1 = creds1.local_token;
    let broker_url1 = creds1.broker_url;

    let runtime1 = Runtime::load(&f.path).unwrap();
    let instance_id1 = runtime1.instance_id;

    // 5. 자신이 생성한 자식 프로세스를 kill 후 wait한다. (stop 호출 없음)
    child.0.kill().unwrap();
    child.0.wait().unwrap();

    // 6. 강제 종료 직후 MCP 자격 파일과 토큰이 유지되는지 확인한다.
    let cred_path = devlink_broker::credentials::path(&f.path);
    assert!(
        cred_path.exists(),
        "MCP credential file must remain after crash"
    );
    let creds_after_crash = devlink_broker::credentials::load(&f.path).unwrap();
    assert!(
        creds_after_crash.local_token == token1,
        "MCP token changed after crash"
    );

    // 7. 같은 설정으로 실제 CLI start를 실행한다.
    f.ok(&["start"]);

    // 8. localToken·brokerUrl은 동일하고 instanceId는 변경됐는지 확인한다.
    let creds2 = devlink_broker::credentials::load(&f.path).unwrap();
    assert!(
        creds2.local_token == token1,
        "MCP token changed after crash restart"
    );
    assert!(
        creds2.broker_url == broker_url1,
        "MCP brokerUrl changed after crash restart"
    );
    let runtime2 = Runtime::load(&f.path).unwrap();
    assert!(
        runtime2.instance_id != instance_id1,
        "Runtime instanceId must change after crash restart"
    );

    // 9. CLI stop으로 종료한다.
    f.ok(&["stop"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_token_allows_health_but_denies_shutdown() {
    // 1. 기존 Fixture로 임시 브로커를 start한다.
    let f = Fixture::new();
    f.ok(&["start"]);

    // 2. 실제 MCP 자격 파일에서 localToken을 읽는다.
    let creds = devlink_broker::credentials::load(&f.path).unwrap();
    let local_token = creds.local_token;

    // 3. localToken과 runtime.controlToken이 서로 다른지 확인한다.
    let runtime = Runtime::load(&f.path).unwrap();
    assert!(
        local_token != runtime.control_token,
        "MCP token and control token must differ"
    );

    let client = runtime.client().unwrap();

    // 4. MCP 토큰으로 GET /health → 200 확인.
    let health_res = client
        .get(runtime.url("/health"))
        .bearer_auth(&local_token)
        .send()
        .await
        .unwrap();
    assert_eq!(health_res.status(), 200);

    // 5. MCP 토큰으로 POST /shutdown → 현재 구현 기준 401 확인.
    let shutdown_res = client
        .post(runtime.url("/shutdown"))
        .bearer_auth(&local_token)
        .send()
        .await
        .unwrap();
    assert_eq!(shutdown_res.status(), 401);

    // 6. 종료 거부 후 Health를 다시 확인해 같은 instanceId의 브로커가 계속 실행 중인지 확인한다.
    let current_health = f.health().await;
    assert_eq!(current_health.instance_id, runtime.instance_id);

    // 7. MCP 토큰과 Origin 헤더로 GET /health → 401 확인.
    let origin_health_res = client
        .get(runtime.url("/health"))
        .bearer_auth(&local_token)
        .header("Origin", "https://example.org")
        .send()
        .await
        .unwrap();
    assert_eq!(origin_health_res.status(), 401);

    // 8. 실제 CLI stop으로 정상 종료한다.
    f.ok(&["stop"]);
    assert!(!Runtime::path(&f.path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_token_rotation_requires_stopped_broker() {
    // 1. 임시 브로커를 start하고 MCP 토큰과 instanceId를 보관한다.
    let f = Fixture::new();
    f.ok(&["start"]);
    let creds1 = devlink_broker::credentials::load(&f.path).unwrap();
    let initial_token = creds1.local_token;
    let initial_instance_id = f.health().await.instance_id;

    // 2. 실행 중 CLI의 mcp rotate를 호출한다.
    //    실패 종료 코드, 기존 토큰 유지, 같은 브로커의 정상 Health를 확인한다.
    let rotate_running = f.run(&["mcp", "rotate"]);
    assert!(!rotate_running.status.success());
    let creds_after_failed_rotate = devlink_broker::credentials::load(&f.path).unwrap();
    assert!(
        creds_after_failed_rotate.local_token == initial_token,
        "Token must remain unchanged when rotation is rejected"
    );
    let health_after_failed_rotate = f.health().await;
    assert_eq!(health_after_failed_rotate.instance_id, initial_instance_id);

    // 3. CLI stop으로 종료한다.
    f.ok(&["stop"]);

    // 4. CLI mcp rotate를 호출해 성공과 토큰 변경을 확인한다.
    f.ok(&["mcp", "rotate"]);
    let creds_rotated = devlink_broker::credentials::load(&f.path).unwrap();
    let rotated_token = creds_rotated.local_token;
    assert!(
        rotated_token != initial_token,
        "Token must change after rotation"
    );

    // 5. 다시 start하고 회전한 토큰이 유지되는지 확인한다.
    f.ok(&["start"]);
    let creds_restarted = devlink_broker::credentials::load(&f.path).unwrap();
    assert!(
        creds_restarted.local_token == rotated_token,
        "Rotated token must survive restart"
    );

    // 6. /health에 회전한 토큰으로 요청하면 200,
    //    이전 토큰으로 요청하면 401인지 확인한다.
    let runtime = Runtime::load(&f.path).unwrap();
    let client = runtime.client().unwrap();

    let res_rotated = client
        .get(runtime.url("/health"))
        .bearer_auth(&rotated_token)
        .send()
        .await
        .unwrap();
    assert_eq!(res_rotated.status(), 200);

    let res_initial = client
        .get(runtime.url("/health"))
        .bearer_auth(&initial_token)
        .send()
        .await
        .unwrap();
    assert_eq!(res_initial.status(), 401);

    // 7. CLI stop으로 종료한다.
    f.ok(&["stop"]);
    assert!(!Runtime::path(&f.path).exists());
}

fn ensure_mcp_reader_verifier() -> PathBuf {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mcp_root = manifest_dir.join("../../../packages/mcp-server");
    let reader_dir = manifest_dir.join("../../target/mcp-reader-check");

    // Remove previous src, dist, and verify.mjs so stale sources or build outputs never mix in
    let _ = std::fs::remove_dir_all(reader_dir.join("src"));
    let _ = std::fs::remove_dir_all(reader_dir.join("dist"));
    let _ = std::fs::remove_file(reader_dir.join("verify.mjs"));

    let broker_src = reader_dir.join("src/broker");
    std::fs::create_dir_all(&broker_src).expect("Failed to create mcp-reader-check/src/broker");

    for file in ["package.json", "package-lock.json", "tsconfig.json"] {
        std::fs::copy(mcp_root.join(file), reader_dir.join(file))
            .unwrap_or_else(|e| panic!("Failed to copy {file} from packages/mcp-server: {e}"));
    }
    for entry in std::fs::read_dir(mcp_root.join("src/broker"))
        .expect("Failed to read packages/mcp-server/src/broker")
    {
        let entry = entry.expect("Failed to read directory entry in src/broker");
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("ts") {
            let file_name = entry.file_name();
            std::fs::copy(&path, broker_src.join(&file_name)).unwrap_or_else(|e| {
                panic!("Failed to copy src/broker/{}: {e}", file_name.display())
            });
        }
    }

    if !reader_dir.join("node_modules/zod").exists()
        || !reader_dir.join("node_modules/typescript/bin/tsc").exists()
    {
        let npm_bin = if cfg!(windows) { "npm.cmd" } else { "npm" };
        let out = Command::new(npm_bin)
            .args(["ci", "--prefer-offline", "--no-audit", "--no-fund"])
            .current_dir(&reader_dir)
            .output()
            .expect("Failed to execute npm ci for MCP reader verification");
        assert!(
            out.status.success(),
            "npm ci failed in mcp-reader-check: {}",
            text(&out)
        );
    }

    let out = Command::new("node")
        .arg(reader_dir.join("node_modules/typescript/bin/tsc"))
        .args([
            "--ignoreConfig",
            "--target",
            "ES2022",
            "--module",
            "NodeNext",
            "--moduleResolution",
            "NodeNext",
            "--rootDir",
            "src",
            "--outDir",
            "dist",
            "--strict",
            "--noUncheckedIndexedAccess",
            "--exactOptionalPropertyTypes",
            "--verbatimModuleSyntax",
            "--forceConsistentCasingInFileNames",
            "--types",
            "node",
            "src/broker/settings.ts",
        ])
        .current_dir(&reader_dir)
        .output()
        .expect("Failed to execute tsc for MCP reader verification");
    assert!(
        out.status.success(),
        "tsc compilation of MCP reader failed: {}",
        text(&out)
    );
    assert!(
        reader_dir.join("dist/broker/settings.js").exists(),
        "dist/broker/settings.js was not generated by tsc"
    );

    let verify_script = reader_dir.join("verify.mjs");
    const VERIFY_MJS: &str = r#"import { readBrokerSettings } from './dist/broker/settings.js';

const credPath = process.argv[2];
if (!credPath) {
  console.error('ERROR: Missing credential file path');
  process.exit(1);
}

try {
  const settings = await readBrokerSettings(credPath);
  if (settings.schemaVersion === 1) {
    console.log('MCP_CREDENTIAL_READ_OK');
    process.exit(0);
  } else {
    console.error('ERROR: Unexpected schemaVersion');
    process.exit(2);
  }
} catch (err) {
  const errCode = err?.code || err?.message || 'UNKNOWN_ERROR';
  console.error(`ERROR: ${errCode}`);
  process.exit(3);
}
"#;
    std::fs::write(&verify_script, VERIFY_MJS).expect("Failed to write verify.mjs");
    assert!(
        verify_script.exists(),
        "MCP reader verify.mjs was not prepared"
    );
    verify_script
}

#[test]
fn cli_and_mcp_reader_enforce_consistent_port_rules() {
    let f = Fixture::new();
    let mut raw: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&f.path).unwrap()).unwrap();
    raw["localPort"] = serde_json::json!(80);
    std::fs::write(&f.path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();

    // Existing port 80 config must be rejected by config show and start
    assert!(!f.run(&["config"]).status.success());
    assert!(!f.run(&["start"]).status.success());
    // Updating an unrelated field while localPort remains 80 must still be rejected
    assert!(
        !f.run(&["config", "set", "agent", "other-agent"])
            .status
            .success()
    );
    assert!(
        devlink_broker::credentials::publish(&f.path, "http://127.0.0.1:80").is_err(),
        "credentials::publish must reject port 80"
    );
    assert!(!devlink_broker::credentials::path(&f.path).exists());
    assert!(!Runtime::path(&f.path).exists());

    // CLI config set must be able to repair an existing localPort: 80 config without manual editing
    f.ok(&["config", "set", "localPort", "43127"]);
    assert_eq!(Config::load(&f.path).unwrap().local_port, 43127);
    assert!(f.ok(&["config"]).contains("43127"));

    let verify_script = ensure_mcp_reader_verifier();
    let cred_path = devlink_broker::credentials::path(&f.path);

    // Supported ports (1, 79, 81, 43127, 65535) must pass both Rust load and real MCP reader
    for port in [1u16, 79, 81, 43127, 65535] {
        let url = format!("http://127.0.0.1:{port}");
        let creds = devlink_broker::credentials::publish(&f.path, &url)
            .unwrap_or_else(|_| panic!("publish must succeed for port {port}"));
        let loaded = devlink_broker::credentials::load(&f.path)
            .unwrap_or_else(|_| panic!("load must succeed for port {port}"));
        assert!(
            loaded == creds,
            "loaded credentials must match for port {port}"
        );

        let out = Command::new("node")
            .arg(&verify_script)
            .arg(&cred_path)
            .output()
            .expect("Failed to run node verify.mjs");
        assert!(
            out.status.success() && text(&out).contains("MCP_CREDENTIAL_READ_OK"),
            "MCP reader rejected supported port {port}: {}",
            text(&out)
        );
    }

    let write_broker_url = |url: &str| {
        let mut cred_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&cred_path).unwrap()).unwrap();
        cred_json["brokerUrl"] = serde_json::json!(url);
        let mut bytes = serde_json::to_vec_pretty(&cred_json).unwrap();
        bytes.push(b'\n');
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&cred_path)
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    };

    // Malformed port strings ("+81", leading zero "081", "0", "65536") must be rejected by both
    // Rust load and the real MCP reader, and repaired by `devlink mcp rotate`
    for bad_url in [
        "http://127.0.0.1:+81",
        "http://127.0.0.1:081",
        "http://127.0.0.1:0",
        "http://127.0.0.1:65536",
    ] {
        write_broker_url(bad_url);

        assert!(
            devlink_broker::credentials::load(&f.path).is_err(),
            "credentials::load must reject malformed brokerUrl {bad_url}"
        );
        let bad_out = Command::new("node")
            .arg(&verify_script)
            .arg(&cred_path)
            .output()
            .expect("Failed to run node verify.mjs");
        assert!(
            !bad_out.status.success(),
            "MCP reader must reject malformed brokerUrl {bad_url}"
        );

        // Rotate via CLI must repair brokerUrl to the valid configured default (43127)
        f.ok(&["mcp", "rotate"]);
        let repaired = devlink_broker::credentials::load(&f.path)
            .expect("credentials::load must succeed after rotating corrupted file");
        assert_eq!(repaired.broker_url, "http://127.0.0.1:43127");

        let ok_out = Command::new("node")
            .arg(&verify_script)
            .arg(&cred_path)
            .output()
            .expect("Failed to run node verify.mjs after rotate");
        assert!(
            ok_out.status.success() && text(&ok_out).contains("MCP_CREDENTIAL_READ_OK"),
            "MCP reader rejected rotated credential after repairing {bad_url}: {}",
            text(&ok_out)
        );
    }

    // Explicit port 80 is accepted by current MCP credentialSchema, while Rust policy rejects it;
    // rotating a port 80 file via CLI must still recover it to the configured default (43127)
    write_broker_url("http://127.0.0.1:80");
    assert!(
        devlink_broker::credentials::load(&f.path).is_err(),
        "Rust credentials::load must reject port 80 per Rust policy"
    );
    let port80_reader_out = Command::new("node")
        .arg(&verify_script)
        .arg(&cred_path)
        .output()
        .expect("Failed to run node verify.mjs on port 80");
    assert!(
        port80_reader_out.status.success()
            && text(&port80_reader_out).contains("MCP_CREDENTIAL_READ_OK"),
        "Current MCP reader should accept explicit port 80 literal: {}",
        text(&port80_reader_out)
    );

    f.ok(&["mcp", "rotate"]);
    let repaired_from_80 = devlink_broker::credentials::load(&f.path)
        .expect("credentials::load must succeed after rotating port 80 file");
    assert_eq!(repaired_from_80.broker_url, "http://127.0.0.1:43127");
    let ok_out_80 = Command::new("node")
        .arg(&verify_script)
        .arg(&cred_path)
        .output()
        .expect("Failed to run node verify.mjs after rotating port 80 file");
    assert!(
        ok_out_80.status.success() && text(&ok_out_80).contains("MCP_CREDENTIAL_READ_OK"),
        "MCP reader rejected rotated credential after repairing port 80: {}",
        text(&ok_out_80)
    );
}

#[cfg(windows)]
#[test]
fn cli_rejects_junction_state_dir_and_preserves_target_files() {
    use std::os::windows::process::CommandExt;

    fn get_sddl(path: &std::path::Path) -> String {
        const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$p = $env:DEVLINK_SDDL_PATH
$acl = if ([System.IO.Directory]::Exists($p)) { [System.IO.Directory]::GetAccessControl($p) } else { [System.IO.File]::GetAccessControl($p) }
[Console]::Write($acl.GetSecurityDescriptorSddlForm('All'))
"#;
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
            .env("DEVLINK_SDDL_PATH", path)
            .stdin(std::process::Stdio::null())
            .creation_flags(0x08000000)
            .output()
            .expect("Failed to get SDDL");
        assert!(output.status.success(), "powershell SDDL query failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    let temp_root = tempfile::tempdir().unwrap();
    let target_dir = temp_root.path().join("target_dir");
    std::fs::create_dir_all(&target_dir).unwrap();

    let target_runtime = target_dir.join("runtime.json");
    let target_sentinel = target_dir.join("sentinel.txt");
    std::fs::write(&target_runtime, b"{\"existing\":\"runtime\"}").unwrap();
    std::fs::write(&target_sentinel, b"original-content").unwrap();

    let target_dir_sddl_before = get_sddl(&target_dir);
    let runtime_sddl_before = get_sddl(&target_runtime);
    let sentinel_sddl_before = get_sddl(&target_sentinel);

    let config_dir = temp_root.path().join("config_dir");
    std::fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join("config.json");
    config::atomic_json(&config_path, &Config::default()).unwrap();

    let junction_path = config::state_directory(&config_path);
    let status = std::process::Command::new("cmd")
        .args([
            "/c",
            "mklink",
            "/J",
            junction_path.to_str().unwrap(),
            target_dir.to_str().unwrap(),
        ])
        .stdin(std::process::Stdio::null())
        .creation_flags(0x08000000)
        .status()
        .expect("mklink /J execution failed");
    assert!(status.success(), "Failed to create directory junction");

    struct JunctionCleanup(PathBuf);
    impl Drop for JunctionCleanup {
        fn drop(&mut self) {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("cmd")
                .args(["/c", "rmdir", self.0.to_str().unwrap()])
                .stdin(std::process::Stdio::null())
                .creation_flags(0x08000000)
                .status();
        }
    }
    let _cleanup = JunctionCleanup(junction_path);

    let f = Fixture {
        _dir: temp_root,
        path: config_path,
    };

    for args in [
        &["start"][..],
        &["status"][..],
        &["stop"][..],
        &["mcp", "rotate"][..],
        &["broker"][..],
    ] {
        let out = f.run(args);
        assert!(
            !out.status.success(),
            "CLI command {args:?} must fail when .devlink-runtime is a junction"
        );
    }

    assert!(
        target_runtime.exists(),
        "existing runtime.json in junction target must not be deleted"
    );
    assert_eq!(
        std::fs::read(&target_runtime).unwrap(),
        b"{\"existing\":\"runtime\"}",
        "existing runtime.json content in junction target must be preserved"
    );
    assert_eq!(
        std::fs::read(&target_sentinel).unwrap(),
        b"original-content",
        "existing sentinel.txt content in junction target must be preserved"
    );
    assert!(
        !target_dir.join("control.lock").exists(),
        "control.lock must not be created in junction target"
    );
    assert!(
        !target_dir.join("broker.lock").exists(),
        "broker.lock must not be created in junction target"
    );
    assert!(
        !target_dir.join("broker.json").exists(),
        "broker.json must not be created in junction target"
    );

    assert_eq!(
        get_sddl(&target_dir),
        target_dir_sddl_before,
        "target directory permissions must remain untouched"
    );
    assert_eq!(
        get_sddl(&target_runtime),
        runtime_sddl_before,
        "existing runtime.json permissions must remain untouched"
    );
    assert_eq!(
        get_sddl(&target_sentinel),
        sentinel_sddl_before,
        "existing sentinel.txt permissions must remain untouched"
    );
}
