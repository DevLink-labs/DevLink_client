use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use devlink_broker::{
    broker,
    config::{self, Config, state_directory},
    local::{self, Runtime},
};
use devlink_cli::setup::terminal::{self as setup_terminal, SetupArgs};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::time::{Instant, sleep};

#[derive(Parser)]
#[command(name = "devlink", version, about = "DevLink CLI and local broker")]
pub struct Cli {
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,
    #[command(subcommand)]
    pub command: Commands,
}

pub fn parse() -> Cli {
    let arguments: Vec<_> = std::env::args_os().collect();
    let setup_requested = {
        let mut arguments = arguments.iter().skip(1);
        loop {
            match arguments.next() {
                Some(arg) if arg == "--config" => {
                    // A profile named "setup" is a value, not a subcommand.
                    arguments.next();
                }
                Some(arg) if arg.to_str().is_some_and(|s| s.starts_with("--config=")) => {}
                Some(arg) => break arg == "setup",
                None => break false,
            }
        }
    };
    match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            // Invalid-value errors omit command usage, so identify the requested
            // command before parsing. Never echo setup values; preserve legacy errors.
            if setup_requested && error.use_stderr() {
                eprintln!("Invalid setup arguments. Run devlink setup --help for usage.");
                std::process::exit(2);
            }
            error.exit()
        }
    }
}
#[derive(Subcommand)]
pub enum Commands {
    /// Run guided setup (execution foundation; stage integrations follow)
    Setup(SetupArgs),
    /// Create a configuration file without overwriting existing settings
    Init,
    /// Start an independent background broker
    Start,
    /// Gracefully stop the broker
    Stop,
    /// Show local broker and central WebSocket state
    Status,
    /// Read or update non-secret settings (changes apply after restart)
    Config {
        #[command(subcommand)]
        command: Option<ConfigCommand>,
    },
    /// MCP integration and credential management
    Mcp {
        #[command(subcommand)]
        command: Option<McpCommand>,
    },
    /// Sign in to the central user API (separate from Agent/MCP credentials)
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Create/join teams and choose a verified team for Agent setup
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Bind an owned Agent and keep its central token separate from MCP settings
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Read or clear locally retained requests; never accepts or executes tasks
    Inbox {
        #[command(subcommand)]
        command: Option<InboxCommand>,
    },
    /// Internal command: run the broker loop in foreground (spawned by start)
    #[command(hide = true)]
    Broker,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show the current configuration formatted as JSON
    Show,
    /// Update a configuration key-value pair
    Set {
        /// Configuration key to update (e.g. serverUrl, agent, localPort)
        key: String,
        /// New value for the configuration key
        value: String,
    },
}

#[derive(Subcommand)]
pub enum McpCommand {
    /// Print the absolute path of DEVLINK_BROKER_FILE for MCP Host configuration
    Path,
    /// Rotate the MCP local token (only allowed when broker is stopped)
    Rotate,
}

#[derive(Subcommand)]
pub enum InboxCommand {
    /// Read newest retained tasks for this server and bound Agent (or explicit local filter)
    List {
        #[arg(long, default_value_t = 20, value_parser=clap::value_parser!(u16).range(1..=1024))]
        limit: u16,
        #[arg(long)]
        agent_id: Option<u64>,
    },
    /// Delete this profile's entire local inbox and dedup history while broker is stopped
    Clear,
}

#[derive(Subcommand)]
pub enum AuthCommand {
    /// Sign in; password is never accepted as a command-line argument
    Login {
        #[arg(long)]
        email: String,
        /// Read one password line from stdin instead of a hidden terminal prompt
        #[arg(long)]
        password_stdin: bool,
    },
    /// Check the session against the central server
    Status,
    /// End the user session without revoking the separate Agent token
    Logout,
}

#[derive(Subcommand)]
pub enum TeamCommand {
    Create {
        #[arg(long)]
        name: String,
    },
    List,
    /// Read the invite code in a hidden prompt or from one stdin line
    Join {
        #[arg(long)]
        code_stdin: bool,
    },
    Select {
        #[arg(long)]
        id: u64,
    },
    Current,
    Members {
        #[arg(long)]
        id: u64,
    },
}

#[derive(Subcommand)]
pub enum AgentCommand {
    /// Explicitly create once; an uncertain creation is never retried automatically
    Create {
        #[arg(long)]
        name: String,
        #[arg(long,value_parser=["codex","claude-code"])]
        tool: String,
        #[arg(long)]
        project_name: String,
        #[arg(long)]
        project: PathBuf,
        #[arg(long)]
        role_description: Option<String>,
    },
    /// List the selected team's Agents; only your own ID can be bound
    List,
    /// Bind a server-verified existing ID; missing credentials require explicit token
    Use {
        #[arg(long)]
        id: u64,
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Explicitly issue/rotate the current Agent token while its broker is stopped
    Token,
    /// Show public profile metadata and verify a stored central token
    Status,
    /// Clear only an unknown creation after manual server inspection; never retries
    ResetPending {
        #[arg(long)]
        accept_duplicate_risk: bool,
    },
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    let json = serde_json::to_string(value)?;
    let safe: String = json
        .chars()
        .map(|c| {
            if matches!(c, '\u{7f}'..='\u{9f}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
            {
                format!("\\u{:04x}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect();
    println!("{safe}");
    Ok(())
}

fn password_input(from_stdin: bool) -> Result<String> {
    secret_input("Password", 72, from_stdin)
}

fn secret_input(label: &str, limit: usize, from_stdin: bool) -> Result<String> {
    use std::io::{BufRead, Read};
    let password = if from_stdin {
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .take(1025)
            .read_until(b'\n', &mut bytes)
            .map_err(|_| anyhow::anyhow!("Cannot read password from stdin"))?;
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("Password must be UTF-8"))?
    } else {
        rpassword::prompt_password(format!("{label}: ")).map_err(|_| {
            let flag = if label == "Password" {
                "--password-stdin"
            } else {
                "--code-stdin"
            };
            anyhow::anyhow!("Cannot read hidden {label}; use {flag} for pipes")
        })?
    };
    if password.is_empty() || password.len() > limit {
        bail!("{label} must contain 1 to {limit} UTF-8 bytes");
    }
    Ok(password)
}

/// CLI 진입점: 파싱된 CLI 명령어에 따라 적절한 로직을 라우팅하여 실행합니다.
pub async fn run(cli: Cli) -> Result<()> {
    let path = config::config_path(cli.config)?;
    match cli.command {
        Commands::Setup(args) => setup_terminal::run(&path, args).await?,
        Commands::Inbox { command } => {
            use devlink_broker::inbox::Inbox;
            match command.unwrap_or(InboxCommand::List {
                limit: 20,
                agent_id: None,
            }) {
                InboxCommand::List { limit, agent_id } => {
                    let config = Config::load(&path)?;
                    let agent = match agent_id {
                        Some(id) => {
                            anyhow::ensure!(
                                id > 0 && id <= i64::MAX as u64,
                                "Invalid local Agent filter"
                            );
                            Some(id)
                        }
                        None => devlink_broker::agent_profile::bound_agent_id(&path)?,
                    };
                    let tasks =
                        Inbox::list_config(&path, &config.server_url, agent, limit as usize)?;
                    print_json(&serde_json::to_value(tasks)?)?;
                }
                InboxCommand::Clear => {
                    let directory = state_directory(&path);
                    if !directory.try_exists()? {
                        println!("Inbox already empty");
                        return Ok(());
                    }
                    devlink_broker::credentials::verify_parent_safety(&directory)?;
                    let _control = config::lock(&directory.join("control.lock"))?;
                    let _broker = config::lock(&directory.join("broker.lock")).context(
                        "Stop the broker before clearing inbox retention and dedup history",
                    )?;
                    Inbox::clear_config(&path)?;
                    println!("Inbox data and dedup history cleared; no central task was changed");
                }
            }
        }
        Commands::Agent { command } => {
            use devlink_broker::agent_profile::{self, CreateOptions, Tool};
            let result = match command {
                AgentCommand::Create {
                    name,
                    tool,
                    project_name,
                    project,
                    role_description,
                } => {
                    agent_profile::create(
                        &path,
                        CreateOptions {
                            name,
                            tool: Tool::parse(&tool)?,
                            project_name,
                            project,
                            role_description,
                        },
                    )
                    .await?
                }
                AgentCommand::List => serde_json::to_value(agent_profile::list(&path).await?)?,
                AgentCommand::Use { id, project } => {
                    agent_profile::use_agent(&path, id, project.as_deref()).await?
                }
                AgentCommand::Token => agent_profile::token(&path).await?,
                AgentCommand::Status => agent_profile::status(&path).await?,
                AgentCommand::ResetPending {
                    accept_duplicate_risk,
                } => agent_profile::reset_pending(&path, accept_duplicate_risk).await?,
            };
            print_json(&result)?;
        }
        Commands::Team { command } => {
            use devlink_broker::teams;
            let value = match command {
                TeamCommand::Create { name } => {
                    serde_json::to_value(teams::create(&path, &name).await?)?
                }
                TeamCommand::List => serde_json::to_value(teams::list(&path).await?)?,
                TeamCommand::Join { code_stdin } => {
                    devlink_broker::user_auth::validate_config(&path)?;
                    let code = secret_input("Invite code", 256, code_stdin)?;
                    serde_json::to_value(teams::join(&path, &code).await?)?
                }
                TeamCommand::Select { id } => {
                    serde_json::json!({"selected":true,"team":teams::select(&path,id).await?,"agentBindingChanged":false})
                }
                TeamCommand::Current => match teams::current(&path).await? {
                    Some(team) => serde_json::json!({"selected":true,"team":team}),
                    None => serde_json::json!({"selected":false}),
                },
                TeamCommand::Members { id } => {
                    serde_json::to_value(teams::members(&path, id).await?)?
                }
            };
            print_json(&value)?;
        }
        Commands::Auth { command } => match command {
            AuthCommand::Login {
                email,
                password_stdin,
            } => {
                devlink_broker::user_auth::validate_config(&path)?;
                let password = password_input(password_stdin)?;
                let user = devlink_broker::user_auth::login(&path, &email, &password).await?;
                print_json(&serde_json::json!({"state":"authenticated","user":user}))?;
            }
            AuthCommand::Status => print_json(&devlink_broker::user_auth::status(&path).await?)?,
            AuthCommand::Logout => {
                let confirmed = devlink_broker::user_auth::logout(&path).await?;
                print_json(&serde_json::json!({"state":"logged_out","authenticated":false}))?;
                if !confirmed {
                    eprintln!(
                        "Local session cleared; no remote session was available to invalidate"
                    );
                }
            }
        },
        // 설정 파일 초기화 (기존 파일이 없을 때만 기본 생성)
        Commands::Init => {
            println!(
                "{}",
                if config::init(&path)? {
                    "Configuration created"
                } else {
                    "Configuration already exists"
                }
            );
        }
        // 설정 조회 및 업데이트
        Commands::Config { command } => match command {
            // 특정 설정 키-값 수정
            Some(ConfigCommand::Set { key, value }) => {
                config::set(&path, &key, &value)?;
                println!("Configuration updated; restart broker to apply");
            }
            // 전체 설정 JSON 형식 출력 (기본값)
            None | Some(ConfigCommand::Show) => {
                println!("{}", serde_json::to_string_pretty(&Config::load(&path)?)?)
            }
        },
        // 로컬 브로커 백그라운드 프로세스 시작
        Commands::Start => start(&path).await?,
        // 로컬 브로커 실행 여부 및 중앙 서버(WebSocket) 연결 상태 확인
        Commands::Status => {
            if !local::running(&path)? {
                println!("Local Broker  : Stopped\nCentral Server: Disconnected");
            } else {
                let health = Runtime::load(&path)?.health().await?;
                println!(
                    "Local Broker  : Running (PID: {})\nCentral Server: {:?}",
                    health.pid, health.central_server
                );
                if let Some(inbox) = health.inbox {
                    println!(
                        "Inbox rejects : {} (last: {:?})",
                        inbox.rejected_events, inbox.last_error
                    );
                }
            }
        }
        // 실행 중인 브로커 정상 종료 (Graceful shutdown)
        Commands::Stop => {
            if std::fs::symlink_metadata(state_directory(&path)).is_ok() {
                devlink_broker::credentials::verify_parent_safety(&state_directory(&path))?;
            }
            if !state_directory(&path).exists() {
                println!("Broker already stopped");
                return Ok(());
            }
            // 중복 제어 방지를 위한 파일 락 획득
            let _guard = config::lock(&state_directory(&path).join("control.lock"))?;
            if !local::running(&path)? {
                println!("Broker already stopped");
                return Ok(());
            }
            // 종료 요청 전송 후 최대 8초간 정상 종료 대기
            Runtime::load(&path)?.stop().await?;
            let deadline = Instant::now() + Duration::from_secs(8);
            while local::running(&path)? {
                if Instant::now() >= deadline {
                    bail!("Broker shutdown timed out; process was not force-killed");
                }
                sleep(Duration::from_millis(50)).await;
            }
            println!("Broker stopped");
        }
        // MCP 자격 경로 조회 및 토큰 회전
        Commands::Mcp { command } => match command.unwrap_or(McpCommand::Path) {
            McpCommand::Path => {
                println!("{}", devlink_broker::credentials::path(&path).display());
            }
            McpCommand::Rotate => {
                devlink_broker::credentials::rotate(&path)?;
                println!("MCP local token rotated; restart any running MCP server to apply");
            }
        },
        // 포그라운드에서 직접 브로커 통신 루프 실행 (start 명령어로 생성된 자식 프로세스가 실행)
        Commands::Broker => broker::run(&path).await?,
    }
    Ok(())
}

/// 브로커를 터미널과 분리된 백그라운드 독립 프로세스로 구동하고 정상 기동을 검증합니다.
async fn start(path: &Path) -> Result<()> {
    Config::load(path)?;
    let _guard = config::lock(&state_directory(path).join("control.lock"))?;
    if local::running(path)? {
        Runtime::load(path)?.health().await?;
        println!("Broker already running");
        return Ok(());
    }
    broker::central_token_for(path)?;
    let mut command = Command::new(std::env::current_exe()?);
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
        // Windows 파이프 핸들 상속 방지
        prevent_stdio_inheritance()?;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: 콘솔 창 없는 독립 프로세스 그룹으로 분리
        command.creation_flags(0x00000008 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("Cannot spawn broker process")?;
    // Credential publication precedes runtime discovery. On Windows its three ACL
    // checks can each succeed within 5 s and still exceed the former 8 s wait.
    let startup_timeout = Duration::from_secs(8)
        + devlink_broker::credentials::startup_validation_allowance()
        + devlink_broker::agent_profile::startup_allowance(path);
    let deadline = Instant::now() + startup_timeout;
    // Poll process exit and authenticated readiness within the combined budget.
    loop {
        if let Some(_exit) = child.try_wait()? {
            bail!(
                "Broker exited during startup; check configuration and local port, or run devlink broker in foreground"
            );
        }
        if let Ok(runtime) = Runtime::load(path)
            && runtime.pid == child.id()
            && runtime.health().await.is_ok()
        {
            println!("Broker started (PID: {})", child.id());
            return Ok(());
        }
        if Instant::now() >= deadline {
            // 타임아웃 발생 시 생성한 자식 프로세스 정리
            let _ = child.kill();
            let _ = child.wait();
            bail!("Broker startup timed out; child process cleaned up");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

/// Windows 환경에서 부모 CLI의 표준 핸들(stdin, stdout, stderr) 상속을 차단합니다.
/// 백그라운드 브로커가 파이프를 잡고 있어 CLI 종료 후에도 호출자가 대기(Hang)하는 문제를 방지합니다.
#[cfg(windows)]
fn prevent_stdio_inheritance() -> Result<()> {
    use windows_sys::Win32::{
        Foundation::{HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation},
        System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE},
    };
    // std::process on Windows inherits all inheritable handles, including the CLI's
    // original capture pipes even when the broker's own stdio is redirected to NUL.
    // Clear inheritance on those three existing handles so `devlink start` captured
    // by an Agent/test can reach EOF while its background broker stays alive.
    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle borrows a process-owned handle; it is never closed here.
        let handle = unsafe { GetStdHandle(stream) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        // SAFETY: This only changes the inheritance flag on a valid borrowed handle.
        if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
            bail!("Cannot prevent CLI standard handle inheritance");
        }
    }
    Ok(())
}
