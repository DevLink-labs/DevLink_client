//! Terminal adapter: CLI flags and rendering, separate from setup orchestration.
use super::control::{Attempt, RunControl, terminal_signal};
use super::{
    AgentChoice, AgentInput, AgentInputError, Completion, ErrorCode, LoginCredentials, LoginInput,
    LoginInputError, McpInput, McpInputError, Outcome, ProgressEvent, ProgressUnavailable,
    RunResult, RunStatus, SetupContext, SetupError, SetupStages, Step, TeamChoice, TeamInput,
    TeamInputError, ToolInput, ToolInputError, parse_team_id,
};
use clap::{Args, Subcommand, ValueEnum};
use devlink_broker::{setup_connection, setup_mcp};
use serde::Serialize;
use std::{
    fmt,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Args)]
#[command(
    args_conflicts_with_subcommands = true,
    after_help = "Setup initializes settings, verifies your user session, selects a joined team, prepares the local project, and selects an installed tool.\nWhen sign-in is needed, use hidden terminal input or --email with --password-stdin.\nA valid saved team is reused; a single joined team is selected automatically.\nUse --team-id when several teams require selection.\nProject defaults to the current directory; display names default to its folder name.\nTools are checked with --version only. A valid saved tool is reused; use --tool when several tools are available.\nUse --create-agent for explicit creation or --agent-id for an owned existing Agent.\nA newly created Agent receives its first token once. Existing Agents require --issue-token when their token is missing or rejected.\nValid tokens are reused. Stop this profile's broker before issuance; MCP config is planned for the selected project. Review it interactively or approve with --apply-mcp.\nUse --mcp-mode manual for a safe snippet. Reopen the trusted Host after changing settings. Final connection verification follows."
)]
pub struct SetupArgs {
    #[command(subcommand)]
    pub command: Option<SetupCommand>,
    /// Run without prompts using the v1 JSON input file
    #[arg(long)]
    pub non_interactive: bool,
    /// Read setup inputs from a v1 JSON file (up to 64 KiB)
    #[arg(long, value_name = "FILE")]
    pub input: Option<PathBuf>,
    /// Output format: final JSON only, JSONL progress, or human-readable messages
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub output: OutputFormat,
    /// Used only when the current session is missing or confirmed expired
    #[arg(long, value_name = "EMAIL")]
    pub email: Option<String>,
    /// Read one password line only when sign-in is required (never an argument/env secret)
    #[arg(long)]
    pub password_stdin: bool,
    /// Select a current joined team by ID; otherwise reuse or choose a team
    #[arg(long, value_name = "ID", value_parser = parse_team_id)]
    pub team_id: Option<u64>,
    /// Existing project directory (default: current working directory)
    #[arg(long, alias = "project-path", value_name = "DIRECTORY")]
    pub project: Option<PathBuf>,
    /// Central project display name (default: folder name, or saved profile name)
    #[arg(long, value_name = "NAME")]
    pub project_name: Option<String>,
    /// Agent display name (default: folder name, or saved profile name)
    #[arg(long, value_name = "NAME")]
    pub agent_name: Option<String>,
    /// Optional Agent role description
    #[arg(long, value_name = "DESCRIPTION")]
    pub role_description: Option<String>,
    /// Select codex or claude-code; otherwise reuse a valid selection or choose
    #[arg(long, value_name = "TYPE", value_parser = devlink_broker::tool_discovery::parse_tool)]
    pub tool: Option<devlink_broker::sessions::ToolType>,
    /// Explicit Host executable or npm .cmd shim outside PATH (requires --tool)
    #[arg(long, value_name = "FILE", requires = "tool")]
    pub tool_path: Option<PathBuf>,
    /// Explicitly request Agent creation; a saved matching Agent is reused
    #[arg(long, conflicts_with = "agent_id")]
    pub create_agent: bool,
    /// Explicit owned Agent ID, including recovery after an uncertain creation
    #[arg(long, value_name = "ID", value_parser = parse_team_id,
        conflicts_with_all = ["agent_name", "project_name", "role_description"])]
    pub agent_id: Option<u64>,
    /// Permit one issuance only when the existing Agent token is missing or rejected
    #[arg(long, conflicts_with = "create_agent")]
    pub issue_token: bool,
    /// Project MCP configuration mode: apply or manual
    #[arg(long, value_parser = devlink_broker::setup_mcp::parse_mode)]
    pub mcp_mode: Option<devlink_broker::setup_mcp::Mode>,
    /// Explicit project Host config target; otherwise the provider chooses its fixed project file
    #[arg(long, value_name = "FILE")]
    pub mcp_target: Option<PathBuf>,
    /// Absolute Node >=24 executable; otherwise reuse a Node Host launcher or search PATH
    #[arg(long, value_name = "FILE")]
    pub node_path: Option<PathBuf>,
    /// Absolute built DevLink MCP .js/.mjs entry; otherwise resolve beside the CLI or in its checkout
    #[arg(long, value_name = "FILE")]
    pub mcp_entry: Option<PathBuf>,
    /// Approve applying only the DevLink MCP entry to this project's Host settings
    #[arg(long)]
    pub apply_mcp: bool,
}

#[derive(Subcommand)]
pub enum SetupCommand {
    /// Inspect setup readiness without changing the profile, broker, or Host
    Status {
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        output: OutputFormat,
        /// Node executable override used when configuring MCP
        #[arg(long, value_name = "FILE")]
        node_path: Option<PathBuf>,
        /// MCP entry override used when configuring MCP
        #[arg(long, value_name = "FILE")]
        mcp_entry: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
    Jsonl,
}

/// Carries an already-rendered setup exit code without printing a second error.
#[derive(Debug)]
pub struct SetupExit(pub u8);

impl fmt::Display for SetupExit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Setup did not complete")
    }
}

impl std::error::Error for SetupExit {}

pub async fn run(path: &Path, args: SetupArgs) -> anyhow::Result<()> {
    if let Some(SetupCommand::Status {
        output,
        node_path,
        mcp_entry,
    }) = args.command
    {
        return status(
            path,
            output,
            setup_mcp::Options {
                node: node_path,
                entry: mcp_entry,
                ..Default::default()
            },
        )
        .await;
    }
    let context = SetupContext {
        config_path: path.to_path_buf(),
    };
    let stdout = std::io::stdout();
    let mut output = Renderer::new(stdout.lock(), args.output);
    let mut control = RunControl::new(terminal_signal());
    if args.non_interactive || args.input.is_some() {
        let result = run_batch(&context, args, &mut output, &mut control).await;
        return finish(&mut output, &result);
    }
    let input = TerminalLoginInput {
        email: args.email,
        password_stdin: args.password_stdin,
        unattended: false,
    };
    let stages = SetupStages::new(input)
        .with_mcp_input(TerminalMcpInput)
        .with_mcp(devlink_broker::setup_mcp::Options {
            mode: args.mcp_mode.unwrap_or_default(),
            target: args.mcp_target,
            node: args.node_path,
            entry: args.mcp_entry,
            apply: args.apply_mcp,
        })
        .with_issue_token(args.issue_token)
        .with_agent_input(TerminalAgentInput)
        .with_agent(if args.create_agent {
            Some(devlink_broker::agent_profile::registration::Selection::Create)
        } else {
            args.agent_id
                .map(devlink_broker::agent_profile::registration::Selection::Use)
        })
        .with_team_input(TerminalTeamInput)
        .with_tool_input(TerminalToolInput)
        .with_tool(args.tool, args.tool_path)
        .with_team_id(args.team_id)
        .with_project(devlink_broker::setup_project::ProjectOptions {
            path: args.project,
            project_name: args.project_name,
            agent_name: args.agent_name,
            role_description: args.role_description,
        });
    let result = super::flow::run_controlled(
        &context,
        stages,
        |event| output.progress(event),
        &mut control,
    )
    .await;
    finish(&mut output, &result)
}

fn finish<W: Write>(
    output: &mut Renderer<W>,
    result: &RunResult<setup_connection::Report>,
) -> anyhow::Result<()> {
    render_connection_result(output, result)?;
    if result.exit_code != 0 {
        return Err(SetupExit(result.exit_code).into());
    }
    Ok(())
}

async fn run_batch<W: Write>(
    context: &SetupContext,
    args: SetupArgs,
    output: &mut Renderer<W>,
    control: &mut RunControl,
) -> RunResult<setup_connection::Report> {
    use super::{batch, flow};
    let start = tokio::time::Instant::now();
    let invalid_flags = !args.non_interactive
        || args.email.is_some()
        || args.team_id.is_some()
        || args.project.is_some()
        || args.project_name.is_some()
        || args.agent_name.is_some()
        || args.role_description.is_some()
        || args.tool.is_some()
        || args.tool_path.is_some()
        || args.create_agent
        || args.agent_id.is_some()
        || args.issue_token
        || args.mcp_mode.is_some()
        || args.mcp_target.is_some()
        || args.apply_mcp;
    let plan = match (invalid_flags, args.input) {
        (true, _) => Err(batch::invalid()),
        (false, None) => Err(batch::missing(["input"])),
        (false, Some(path)) => {
            let password_stdin = args.password_stdin;
            control.deadline = Some(start + std::time::Duration::from_secs(30));
            match control
                .attempt(
                    async move {
                        tokio::task::spawn_blocking(move || batch::load(&path, password_stdin))
                            .await
                    },
                    None,
                )
                .await
            {
                Attempt::Done(Ok(result)) => result,
                Attempt::Done(Err(_)) => Err(Box::new(SetupError::new(
                    ErrorCode::InternalError,
                    Step::Validate,
                    Outcome::NotApplied,
                ))),
                Attempt::Interrupted { reason, .. } => Err(Box::new(SetupError::new(
                    reason,
                    Step::Validate,
                    Outcome::NotApplied,
                ))),
            }
        }
    };
    let plan = match plan {
        Ok(plan) => plan,
        Err(error) => return flow::reject(*error, |event| output.progress(event)),
    };
    let login = plan.login_email.is_some();
    let stages = SetupStages::new(TerminalLoginInput {
        email: plan.login_email,
        password_stdin: args.password_stdin,
        unattended: true,
    })
    .with_batch(plan.server_url, login)
    .with_team_id(Some(plan.team))
    .with_project(plan.project)
    .with_tool(Some(plan.tool), None)
    .with_agent(Some(plan.agent))
    .with_issue_token(plan.issue_token)
    .with_mcp(setup_mcp::Options {
        node: args.node_path,
        entry: args.mcp_entry,
        ..plan.mcp
    });
    control.deadline = Some(start + plan.timeout);
    flow::run_controlled(context, stages, |event| output.progress(event), control).await
}

async fn status(
    path: &Path,
    format: OutputFormat,
    options: setup_mcp::Options,
) -> anyhow::Result<()> {
    let observation = match (options.validate(), std::env::current_exe()) {
        (Ok(()), Ok(executable)) => setup_connection::inspect(path, &options, &executable, None)
            .await
            .map_err(super::connection::read_error),
        (validation, _) => {
            let code = match validation.err().map(|error| error.code) {
                Some("SETUP_TOOL_UNAVAILABLE") => ErrorCode::ToolUnavailable,
                Some(_) => ErrorCode::InputInvalid,
                None => ErrorCode::InternalError,
            };
            Err(Box::new(SetupError::new(
                code,
                Step::VerifyConnection,
                Outcome::NotApplied,
            )))
        }
    };
    let (status, exit_code, completion) = match observation {
        Ok(data) => (RunStatus::Succeeded, 0, Completion::Success { data }),
        Err(error) => (
            RunStatus::Failed,
            error.code.exit_code(),
            Completion::Failure { error },
        ),
    };
    let result = RunResult {
        schema_version: 1,
        event_type: "result",
        run_id: uuid::Uuid::new_v4().to_string(),
        sequence: 1,
        status,
        exit_code,
        stages: Vec::new(),
        completion,
    };
    render_connection_result(
        &mut Renderer::new(std::io::stdout().lock(), format),
        &result,
    )?;
    if exit_code == 0 {
        Ok(())
    } else {
        Err(SetupExit(exit_code).into())
    }
}

fn render_connection_result<W: Write>(
    output: &mut Renderer<W>,
    result: &RunResult<setup_connection::Report>,
) -> Result<(), SetupExit> {
    output.result(result).map_err(|_| SetupExit(1))?;
    if matches!(output.format, OutputFormat::Human)
        && let Completion::Success { data } = &result.completion
    {
        let state = &data.connection;
        writeln!(output.writer, "Setup ready: {}", state.setup_ready).map_err(|_| SetupExit(1))?;
        writeln!(
            output.writer,
            "Collaboration ready: {}",
            match state.collaboration_ready {
                Some(true) => "yes",
                Some(false) => "no",
                None => "unknown",
            }
        )
        .map_err(|_| SetupExit(1))?;
        let central = state
            .central
            .map_or_else(|| "unknown".into(), |value| format!("{value:?}"));
        writeln!(
            output.writer,
            "Broker: {:?} | Central: {central} | Identity: {:?}",
            state.broker, state.identity
        )
        .map_err(|_| SetupExit(1))?;
        writeln!(
            output.writer,
            "Credentials: {:?} | MCP config: {:?} | Host session: {:?}",
            state.credentials, state.mcp_config, state.host_session
        )
        .map_err(|_| SetupExit(1))?;
        if !data.next_actions.is_empty() {
            writeln!(
                output.writer,
                "Next actions: {}",
                data.next_actions.join(", ")
            )
            .map_err(|_| SetupExit(1))?;
        }
        output.writer.flush().map_err(|_| SetupExit(1))?;
    }
    Ok(())
}

struct TerminalTeamInput;
impl TeamInput for TerminalTeamInput {
    fn available(&self) -> bool {
        self.interactive()
    }
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    async fn request(&mut self, choices: &[TeamChoice]) -> Result<u64, TeamInputError> {
        if !self.interactive() {
            return Err(TeamInputError::Unavailable);
        }
        let choices = choices.to_vec();
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Read};
            let stderr = std::io::stderr();
            let mut renderer = Renderer::new(stderr.lock(), OutputFormat::Json);
            for choice in choices {
                write!(renderer.writer, "{}: ", choice.id)
                    .map_err(|_| TeamInputError::ReadFailed)?;
                renderer
                    .json(&choice.name)
                    .map_err(|_| TeamInputError::ReadFailed)?;
            }
            write!(renderer.writer, "Team ID: ")
                .and_then(|_| renderer.writer.flush())
                .map_err(|_| TeamInputError::ReadFailed)?;
            let mut bytes = Vec::new();
            if std::io::stdin()
                .lock()
                .take(128)
                .read_until(b'\n', &mut bytes)
                .map_err(|_| TeamInputError::ReadFailed)?
                == 0
            {
                return Err(TeamInputError::Cancelled);
            }
            let value = String::from_utf8(bytes).map_err(|_| TeamInputError::Invalid)?;
            parse_team_id(value.trim()).map_err(|_| TeamInputError::Invalid)
        })
        .await
        .map_err(|_| TeamInputError::ReadFailed)?
    }
}

struct TerminalToolInput;
impl ToolInput for TerminalToolInput {
    fn available(&self) -> bool {
        self.interactive()
    }
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    async fn request(
        &mut self,
        tools: &[devlink_broker::tool_discovery::ToolDetection],
    ) -> Result<devlink_broker::sessions::ToolType, ToolInputError> {
        if !self.interactive() {
            return Err(ToolInputError::Unavailable);
        }
        let tools = tools.to_vec();
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Read};
            let stderr = std::io::stderr();
            let mut renderer = Renderer::new(stderr.lock(), OutputFormat::Json);
            for tool in tools {
                renderer
                    .json(&tool)
                    .map_err(|_| ToolInputError::ReadFailed)?;
            }
            write!(renderer.writer, "Tool (codex or claude-code): ")
                .and_then(|_| renderer.writer.flush())
                .map_err(|_| ToolInputError::ReadFailed)?;
            let mut bytes = Vec::new();
            if std::io::stdin()
                .lock()
                .take(128)
                .read_until(b'\n', &mut bytes)
                .map_err(|_| ToolInputError::ReadFailed)?
                == 0
            {
                return Err(ToolInputError::Cancelled);
            }
            let value = String::from_utf8(bytes).map_err(|_| ToolInputError::Invalid)?;
            devlink_broker::tool_discovery::parse_tool(value.trim())
                .map_err(|_| ToolInputError::Invalid)
        })
        .await
        .map_err(|_| ToolInputError::ReadFailed)?
    }
}

struct TerminalLoginInput {
    email: Option<String>,
    password_stdin: bool,
    unattended: bool,
}

impl LoginInput for TerminalLoginInput {
    fn available(&self) -> bool {
        self.interactive() || (self.password_stdin && self.email.is_some())
    }

    fn interactive(&self) -> bool {
        !self.unattended && std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    async fn request(&mut self) -> Result<LoginCredentials, LoginInputError> {
        let email = self.email.clone();
        let password_stdin = self.password_stdin;
        let interactive = self.interactive();
        let _mode = if interactive && !password_stdin {
            crate::terminal_input::TerminalMode::capture()
        } else {
            None
        };
        tokio::task::spawn_blocking(move || {
            let email = match email {
                Some(email) => email,
                None if interactive => {
                    use std::io::{BufRead, Read};
                    eprint!("Email: ");
                    std::io::stderr()
                        .flush()
                        .map_err(|_| LoginInputError::ReadFailed)?;
                    let mut bytes = Vec::new();
                    let count = std::io::stdin()
                        .lock()
                        .take(1025)
                        .read_until(b'\n', &mut bytes)
                        .map_err(|_| LoginInputError::ReadFailed)?;
                    if count == 0 {
                        return Err(LoginInputError::Cancelled);
                    }
                    String::from_utf8(bytes)
                        .map_err(|_| LoginInputError::Invalid)?
                        .trim()
                        .to_owned()
                }
                None => return Err(LoginInputError::Unavailable),
            };
            if email.trim().is_empty() || email.len() > 320 {
                return Err(LoginInputError::Invalid);
            }
            if !password_stdin && !interactive {
                return Err(LoginInputError::Unavailable);
            }
            let password = crate::terminal_input::secret_input("Password", 72, password_stdin)
                .map_err(secret_input_error)?;
            LoginCredentials::new(email, password)
        })
        .await
        .map_err(|_| LoginInputError::ReadFailed)?
    }
}

fn secret_input_error(error: anyhow::Error) -> LoginInputError {
    match error
        .downcast_ref::<std::io::Error>()
        .map(std::io::Error::kind)
    {
        Some(std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::Interrupted) => {
            LoginInputError::Cancelled
        }
        Some(_) => LoginInputError::ReadFailed,
        None => LoginInputError::Invalid,
    }
}

/// Caller-supplied writer also supports tests and non-terminal embedding.
pub struct Renderer<W> {
    writer: W,
    format: OutputFormat,
}

impl<W: Write> Renderer<W> {
    pub fn new(writer: W, format: OutputFormat) -> Self {
        Self { writer, format }
    }

    pub fn progress(&mut self, event: &ProgressEvent) -> Result<(), ProgressUnavailable> {
        match self.format {
            OutputFormat::Json => Ok(()),
            OutputFormat::Jsonl => self.json(event),
            OutputFormat::Human => {
                let step = serde_json::to_value(event.step).map_err(|_| ProgressUnavailable)?;
                let status =
                    serde_json::to_value(event.step_status).map_err(|_| ProgressUnavailable)?;
                writeln!(
                    self.writer,
                    "{}: {}",
                    step.as_str().unwrap_or("setup"),
                    status.as_str().unwrap_or("unknown")
                )
                .and_then(|_| self.writer.flush())
                .map_err(|_| ProgressUnavailable)
            }
        }
    }

    pub fn result<D: Serialize>(
        &mut self,
        result: &RunResult<D>,
    ) -> Result<(), ProgressUnavailable> {
        match self.format {
            OutputFormat::Json | OutputFormat::Jsonl => self.json(result),
            OutputFormat::Human => {
                let message = match &result.completion {
                    Completion::Success { .. } if result.stages.is_empty() => "Setup status.",
                    Completion::Success { .. } => "Setup completed.",
                    Completion::Failure { error } => error.message(),
                };
                writeln!(self.writer, "{message}").map_err(|_| ProgressUnavailable)?;
                if let Completion::Failure { error } = &result.completion {
                    write!(self.writer, "Stage: ").map_err(|_| ProgressUnavailable)?;
                    self.json(&error.step)?;
                    if !error.required_inputs.is_empty() {
                        write!(self.writer, "Required inputs: ")
                            .map_err(|_| ProgressUnavailable)?;
                        self.json(&error.required_inputs)?;
                    }
                    if let Some(location) = &error.location {
                        write!(self.writer, "Location: ").map_err(|_| ProgressUnavailable)?;
                        self.json(location)?;
                    }
                    if !error.tools.is_empty() {
                        write!(self.writer, "Tools: ").map_err(|_| ProgressUnavailable)?;
                        self.json(&error.tools)?;
                    }
                    if !error.agent_choices.is_empty() {
                        write!(self.writer, "Owned Agents for the selected tool: ")
                            .map_err(|_| ProgressUnavailable)?;
                        self.json(&error.agent_choices)?;
                    }
                    if let Some(code) = error.agent_list_error {
                        write!(self.writer, "Owned Agent list unavailable: ")
                            .map_err(|_| ProgressUnavailable)?;
                        self.json(&code)?;
                    }
                    if let Some(plan) = &error.mcp_plan {
                        write!(self.writer, "DevLink MCP plan: ")
                            .map_err(|_| ProgressUnavailable)?;
                        self.json(plan)?;
                        writeln!(
                            self.writer,
                            "Review the project entry; approve with --apply-mcp when ready."
                        )
                        .map_err(|_| ProgressUnavailable)?;
                    }
                    if let Some(manual) = &error.manual_config {
                        write!(self.writer, "Manual DevLink MCP configuration: ")
                            .map_err(|_| ProgressUnavailable)?;
                        self.json(manual)?;
                    }
                    if let Some(result) = &error.mcp_config {
                        write!(self.writer, "MCP configuration result: ")
                            .map_err(|_| ProgressUnavailable)?;
                        self.json(result)?;
                    }
                    if let Some(connection) = &error.connection {
                        write!(self.writer, "Connection: ").map_err(|_| ProgressUnavailable)?;
                        self.json(connection)?;
                    }
                    if let Some(provider) = &error.mcp_error {
                        let label = if provider.conflict.is_some() {
                            "MCP configuration conflict"
                        } else {
                            "MCP apply/rollback evidence"
                        };
                        write!(self.writer, "{label}: ").map_err(|_| ProgressUnavailable)?;
                        self.json(provider)?;
                    }
                    writeln!(self.writer, "Recovery: {}", error.recovery_action())
                        .map_err(|_| ProgressUnavailable)?;
                }
                self.writer.flush().map_err(|_| ProgressUnavailable)
            }
        }
    }

    fn json(&mut self, value: &impl Serialize) -> Result<(), ProgressUnavailable> {
        let json = serde_json::to_string(value).map_err(|_| ProgressUnavailable)?;
        // Match the CLI's existing escaping of control/bidi characters in public values.
        for c in json.chars() {
            if matches!(c, '\u{7f}'..='\u{9f}' | '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
            {
                write!(self.writer, "\\u{:04x}", c as u32)
            } else {
                write!(self.writer, "{c}")
            }
            .map_err(|_| ProgressUnavailable)?;
        }
        writeln!(self.writer)
            .and_then(|_| self.writer.flush())
            .map_err(|_| ProgressUnavailable)
    }
}

struct TerminalAgentInput;
impl AgentInput for TerminalAgentInput {
    fn available(&self) -> bool {
        self.interactive()
    }
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    async fn request(
        &mut self,
        choices: &[AgentChoice],
    ) -> Result<devlink_broker::agent_profile::registration::Selection, AgentInputError> {
        if !self.interactive() {
            return Err(AgentInputError::Unavailable);
        }
        let choices = choices.to_vec();
        tokio::task::spawn_blocking(move || {
            use devlink_broker::agent_profile::registration::Selection;
            use std::io::{BufRead, Read};
            fn read(prompt: &str) -> Result<String, AgentInputError> {
                let mut stderr = std::io::stderr().lock();
                write!(stderr, "{prompt}")
                    .and_then(|_| stderr.flush())
                    .map_err(|_| AgentInputError::ReadFailed)?;
                let mut bytes = Vec::new();
                let n = std::io::stdin()
                    .lock()
                    .take(128)
                    .read_until(b'\n', &mut bytes)
                    .map_err(|_| AgentInputError::ReadFailed)?;
                if n == 0 {
                    return Err(AgentInputError::Cancelled);
                }
                if n >= 128 {
                    return Err(AgentInputError::Invalid);
                }
                String::from_utf8(bytes).map_err(|_| AgentInputError::Invalid)
            }
            if !choices.is_empty() {
                let stderr = std::io::stderr();
                let mut renderer = Renderer::new(stderr.lock(), OutputFormat::Json);
                writeln!(renderer.writer, "Owned Agents for the selected tool:")
                    .map_err(|_| AgentInputError::ReadFailed)?;
                renderer
                    .json(&choices)
                    .map_err(|_| AgentInputError::ReadFailed)?;
            }
            match read("Agent action (create/use): ")?.trim() {
                "create" => Ok(Selection::Create),
                "use" => parse_team_id(read("Owned Agent ID: ")?.trim())
                    .map(Selection::Use)
                    .map_err(|_| AgentInputError::Invalid),
                _ => Err(AgentInputError::Invalid),
            }
        })
        .await
        .map_err(|_| AgentInputError::ReadFailed)?
    }
}

struct TerminalMcpInput;
impl McpInput for TerminalMcpInput {
    fn available(&self) -> bool {
        self.interactive()
    }
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }
    async fn request(
        &mut self,
        plan: &devlink_broker::mcp_config::ConfigPlan,
    ) -> Result<bool, McpInputError> {
        if !self.interactive() {
            return Err(McpInputError::Unavailable);
        }
        let plan = plan.clone();
        tokio::task::spawn_blocking(move || {
            use std::io::{BufRead, Read};
            let stderr = std::io::stderr();
            let mut renderer = Renderer::new(stderr.lock(), OutputFormat::Json);
            renderer
                .json(&plan)
                .map_err(|_| McpInputError::ReadFailed)?;
            write!(
                renderer.writer,
                "Apply this DevLink entry to the project MCP file? [y/N]: "
            )
            .and_then(|_| renderer.writer.flush())
            .map_err(|_| McpInputError::ReadFailed)?;
            let mut bytes = Vec::new();
            let n = std::io::stdin()
                .lock()
                .take(16)
                .read_until(b'\n', &mut bytes)
                .map_err(|_| McpInputError::ReadFailed)?;
            if n == 0 {
                return Err(McpInputError::Cancelled);
            }
            if n >= 16 {
                return Ok(false);
            }
            let value = String::from_utf8(bytes).map_err(|_| McpInputError::ReadFailed)?;
            Ok(matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            ))
        })
        .await
        .map_err(|_| McpInputError::ReadFailed)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_input_distinguishes_cancellation_io_and_invalid_values() {
        for kind in [
            std::io::ErrorKind::UnexpectedEof,
            std::io::ErrorKind::Interrupted,
        ] {
            let error =
                anyhow::Error::from(std::io::Error::from(kind)).context("hidden input failed");
            assert!(matches!(
                secret_input_error(error),
                LoginInputError::Cancelled
            ));
        }
        assert!(matches!(
            secret_input_error(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into()),
            LoginInputError::ReadFailed
        ));
        assert!(matches!(
            secret_input_error(anyhow::anyhow!("invalid length")),
            LoginInputError::Invalid
        ));
    }
}
