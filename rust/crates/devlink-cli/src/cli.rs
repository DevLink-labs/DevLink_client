use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use devlink_broker::{
    broker, broker_control,
    config::{self, Config, state_directory},
};
use devlink_cli::setup::terminal::{self as setup_terminal, SetupArgs};
use std::path::PathBuf;

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
    /// Run guided setup or inspect setup readiness
    Setup(Box<SetupArgs>),
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
    /// Show retained requester-side status/result notifications (central details remain authoritative)
    Notifications {
        #[arg(long, default_value_t = 20, value_parser=clap::value_parser!(u16).range(1..=1024))]
        limit: u16,
        /// Clear only local advisory retention while the broker is stopped
        #[arg(long)]
        clear: bool,
    },
    /// Host hook helper: print new local task updates as hook context JSON.
    /// Reads local stores only, never stdin; always exits 0 so it cannot block a prompt.
    UpdatesHook {
        /// codex | claude-code (each keeps its own cursor)
        #[arg(long)]
        host: String,
        /// session-start | user-prompt-submit
        #[arg(long)]
        event: String,
    },
    /// Install, inspect or remove the user-level Host hooks that run updates-hook
    Hooks {
        #[command(subcommand)]
        command: HooksCommand,
    },
    /// Internal command: run the broker loop in foreground (spawned by start)
    #[command(hide = true)]
    Broker,
}

#[derive(Subcommand)]
pub enum HooksCommand {
    /// Show whether this Host's user-level hooks run updates-hook
    Status {
        #[arg(long, value_parser = ["codex", "claude-code"])]
        host: String,
    },
    /// Plan adding SessionStart/UserPromptSubmit hooks; writes only with --yes
    Install {
        #[arg(long, value_parser = ["codex", "claude-code"])]
        host: String,
        /// Apply the planned change (without it, only the plan is printed)
        #[arg(long)]
        yes: bool,
        /// Replace existing DevLink updates-hook entries that use another path or profile
        #[arg(long)]
        replace: bool,
    },
    /// Plan removing only DevLink's updates-hook entries; writes only with --yes
    Uninstall {
        #[arg(long, value_parser = ["codex", "claude-code"])]
        host: String,
        #[arg(long)]
        yes: bool,
    },
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

/// Host hooks treat a non-zero exit as an error (exit 2 blocks the prompt), so every
/// outcome here exits 0. Silence means nothing new; failures are reported only at
/// session start to avoid repeating the same notice on every prompt.
fn updates_hook(config_path: Option<PathBuf>, host: &str, event: &str) {
    let event = match event {
        "session-start" => "SessionStart",
        "user-prompt-submit" => "UserPromptSubmit",
        _ => {
            eprintln!("devlink updates-hook: --event must be session-start or user-prompt-submit");
            return;
        }
    };
    if !matches!(host, "codex" | "claude-code") {
        eprintln!("devlink updates-hook: --host must be codex or claude-code");
        return;
    }
    let result = config::config_path(config_path).and_then(|path| {
        let cwd = std::env::current_dir().context("Cannot read the working directory")?;
        devlink_broker::update_hook::digest(&path, host, &cwd)
    });
    let context = match result {
        Ok(Some(digest)) => digest,
        Ok(None) => return,
        Err(error) if event == "SessionStart" => format!(
            "DevLink could not check local task updates ({error}). Use DevLink get_updates or get_tasks if task status matters."
        ),
        Err(_) => return,
    };
    let _ = print_json(&serde_json::json!({
        "hookSpecificOutput": {"hookEventName": event, "additionalContext": context}
    }));
}

/// Turn a hook provider error into a message that names the problem and the file.
fn hook_error(error: devlink_broker::mcp_config::ConfigError) -> anyhow::Error {
    let at = error
        .path
        .as_deref()
        .map(|p| format!(" ({})", p.display()))
        .unwrap_or_default();
    let hint = match error.code {
        "HOOKS_PATH_UNSUPPORTED" => {
            "the devlink or config path has spaces or shell characters and cannot be written unquoted; add the hook by hand as in the DevLink update-hook guide"
        }
        "HOOKS_CONFIG_INVALID" => {
            "the Host file is not valid JSON, has duplicate keys or an unexpected hooks shape, or the result would exceed 1 MiB; nothing was written"
        }
        "SETUP_MCP_CONFIG_CONFLICT" => {
            "the file changed while planning or is being changed by another process; rerun"
        }
        "SETUP_UNSAFE_FILE" => {
            "the Host file or its folder failed the safety check (link, owner or permissions)"
        }
        "SETUP_INPUT_INVALID" => "a path is not absolute",
        _ => "",
    };
    match error.backup_path.as_deref() {
        Some(backup) => {
            anyhow::anyhow!("{}{at}: {hint} (backup: {})", error.code, backup.display())
        }
        None => anyhow::anyhow!("{}{at}: {hint}", error.code),
    }
}

fn hooks_command(config_path: &std::path::Path, command: HooksCommand) -> Result<()> {
    use devlink_broker::mcp_config::hooks::{self, HookChange, HookTarget};
    use devlink_broker::sessions::ToolType;
    let tool = |host: &str| match host {
        "codex" => ToolType::Codex,
        _ => ToolType::ClaudeCode,
    };
    // Canonical paths keep the command identical however the profile was named.
    let wanted = |tool: ToolType| -> Result<_> {
        let config = config_path.canonicalize().with_context(|| {
            format!(
                "No DevLink configuration at {}. Run devlink setup (or pass --config) first.",
                config_path.display()
            )
        })?;
        let devlink = std::env::current_exe()
            .and_then(|p| p.canonicalize())
            .context("Cannot locate the devlink executable")?;
        hooks::wanted(tool, &devlink, &config).map_err(hook_error)
    };
    let (tool, plan, yes) = match command {
        HooksCommand::Status { host } => {
            let tool = tool(&host);
            let target = HookTarget::user(tool).map_err(hook_error)?;
            // Without a profile, report presence only; with one, also detect stale entries.
            // Paths that need a hand-made hook cannot be compared; check presence only.
            let expected = if config_path.is_file() {
                match wanted(tool) {
                    Ok(wanted) => Some(wanted),
                    Err(error) if error.to_string().starts_with("HOOKS_PATH_UNSUPPORTED") => None,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let status = hooks::status(&target, expected.as_ref()).map_err(hook_error)?;
            return print_json(&serde_json::to_value(status)?);
        }
        HooksCommand::Install { host, yes, replace } => {
            let tool = tool(&host);
            let wanted = wanted(tool)?;
            let target = HookTarget::user(tool).map_err(hook_error)?;
            let plan = hooks::plan_install(&target, wanted, replace).map_err(hook_error)?;
            (tool, plan, yes)
        }
        HooksCommand::Uninstall { host, yes } => {
            let tool = tool(&host);
            let target = HookTarget::user(tool).map_err(hook_error)?;
            (
                tool,
                hooks::plan_uninstall(&target).map_err(hook_error)?,
                yes,
            )
        }
    };
    if plan.change == HookChange::Conflict {
        print_json(&serde_json::json!({"plan": plan, "applied": false}))?;
        anyhow::bail!(
            "SETUP_MCP_CONFIG_CONFLICT: another DevLink updates-hook entry exists for this Host. Review it, then rerun with --replace."
        );
    }
    if !yes && plan.change != HookChange::Unchanged {
        print_json(&serde_json::json!({"plan": plan, "applied": false}))?;
        eprintln!("Review the plan, then rerun with --yes to write it.");
        return Ok(());
    }
    let target = HookTarget::user(tool).map_err(hook_error)?;
    let result = hooks::apply(&target, &plan).map_err(hook_error)?;
    let mut next = Vec::new();
    if result.status == "applied" {
        if tool == ToolType::Codex && plan.change != HookChange::Remove {
            next.push("trust_codex_hooks");
            eprintln!(
                "Codex runs new or changed hooks only after you trust them once: open Codex and approve them in /hooks."
            );
        }
        next.push("restart_host");
    }
    print_json(&serde_json::json!({"plan": plan, "result": result, "nextActions": next}))
}

fn password_input(from_stdin: bool) -> Result<String> {
    secret_input("Password", 72, from_stdin)
}

fn secret_input(label: &str, limit: usize, from_stdin: bool) -> Result<String> {
    devlink_cli::terminal_input::secret_input(label, limit, from_stdin)
}

/// CLI 진입점: 파싱된 CLI 명령어에 따라 적절한 로직을 라우팅하여 실행합니다.
pub async fn run(cli: Cli) -> Result<()> {
    if let Commands::UpdatesHook { host, event } = &cli.command {
        updates_hook(cli.config.clone(), host, event);
        return Ok(());
    }
    let path = config::config_path(cli.config)?;
    match cli.command {
        // Handled before config path resolution so it can never exit non-zero.
        Commands::UpdatesHook { .. } => unreachable!(),
        Commands::Setup(args) => setup_terminal::run(&path, *args).await?,
        Commands::Hooks { command } => hooks_command(&path, command)?,
        Commands::Notifications { limit, clear } => {
            let config = Config::load(&path)?;
            if clear {
                let directory = state_directory(&path);
                devlink_broker::credentials::ensure_safe_parent_directory(&directory)?;
                let _control = config::lock(&directory.join("control.lock"))?;
                let _broker = config::lock(&directory.join("broker.lock"))
                    .context("Stop the broker before clearing notifications")?;
                devlink_broker::notifications::Notifications::clear_config(&path)?;
                // Question/answer arrivals are local notices too; their central cursor is kept.
                devlink_broker::task_messages::TaskMessages::clear_config(&path)?;
                // Overdue advisories and their sent-task ledger are local only as well.
                devlink_broker::sent_tasks::clear_config(&path)?;
                print_json(&serde_json::json!({"cleared":true,"centralTasksChanged":false}))?;
                return Ok(());
            }
            let agent = devlink_broker::agent_profile::bound_agent_id(&path)?
                .context("Bind an owned Agent before reading notifications; use the bound MCP session to query sent tasks/results")?;
            match devlink_broker::notifications::Notifications::list_config(
                &path,
                &config.server_url,
                agent,
                limit as usize,
            ) {
                Ok(events) => print_json(
                    &serde_json::json!({"notifications":events,"authoritative":false,
                    "nextAction":"Use get_tasks(direction=sent) and get_task_result(taskId) in the bound MCP session to verify central results, including missed notifications."}),
                )?,
                Err(_) => print_json(
                    &serde_json::json!({"notifications":[],"authoritative":false,"error":"NOTIFICATION_STORE_UNAVAILABLE",
                    "nextAction":"Use get_tasks(direction=sent) and get_task_result(taskId) in the bound MCP session; local retention is unavailable."}),
                )?,
            }
        }
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
        Commands::Start => {
            let result = broker_control::start(&path, &std::env::current_exe()?).await?;
            if result.reused {
                println!("Broker already running");
            } else {
                println!("Broker started (PID: {})", result.health.pid);
            }
        }
        // 로컬 브로커 실행 여부 및 중앙 서버(WebSocket) 연결 상태 확인
        Commands::Status => {
            if let broker_control::Status::Running { health } =
                broker_control::status(&path).await?
            {
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
                if let Some(sync) = health.task_messages
                    && (sync.rejected_events > 0 || sync.failed_pulls > 0)
                {
                    println!(
                        "Question/answer sync: {} rejected, {} failed pulls (last: {}); verify with get_task_result",
                        sync.rejected_events,
                        sync.failed_pulls,
                        sync.last_error.as_deref().unwrap_or("none")
                    );
                }
                if let Some(count) = health.notification_rejections {
                    println!(
                        "Notification rejects: {count}; verify central details with get_task_result/get_tasks in the bound MCP session"
                    );
                }
            } else {
                println!("Local Broker  : Stopped\nCentral Server: Disconnected");
            }
        }
        // 실행 중인 브로커 정상 종료 (Graceful shutdown)
        Commands::Stop => match broker_control::stop(&path).await? {
            broker_control::StopResult::AlreadyStopped => println!("Broker already stopped"),
            broker_control::StopResult::Stopped => println!("Broker stopped"),
        },
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
