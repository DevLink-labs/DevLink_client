//! Terminal adapter: CLI flags and rendering, separate from setup orchestration.
use super::{
    AgentInput, AgentInputError, Completion, LoginCredentials, LoginInput, LoginInputError,
    ProgressEvent, ProgressUnavailable, RunResult, SetupContext, SetupStages, TeamChoice,
    TeamInput, TeamInputError, ToolInput, ToolInputError, parse_team_id,
};
use clap::{Args, ValueEnum};
use serde::Serialize;
use std::{
    fmt,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

#[derive(Args)]
#[command(
    after_help = "Setup initializes settings, verifies your user session, selects a joined team, prepares the local project, and selects an installed tool.\nWhen sign-in is needed, use hidden terminal input or --email with --password-stdin.\nA valid saved team is reused; a single joined team is selected automatically.\nUse --team-id when several teams require selection.\nProject defaults to the current directory; display names default to its folder name.\nTools are checked with --version only. A valid saved tool is reused; use --tool when several tools are available.\nUse --create-agent for explicit creation or --agent-id for an owned existing Agent.\nCreation stores the Agent ID without issuing a token; token preparation and remaining stages follow."
)]
pub struct SetupArgs {
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
    let context = SetupContext {
        config_path: path.to_path_buf(),
    };
    let stdout = std::io::stdout();
    let mut output = Renderer::new(stdout.lock(), args.output);
    let input = TerminalLoginInput {
        email: args.email,
        password_stdin: args.password_stdin,
    };
    let stages = SetupStages::new(input)
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
    let result = super::run(&context, stages, |event| output.progress(event)).await;
    output.result(&result).map_err(|_| SetupExit(1))?;
    if result.exit_code != 0 {
        return Err(SetupExit(result.exit_code).into());
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
}

impl LoginInput for TerminalLoginInput {
    fn available(&self) -> bool {
        self.interactive() || (self.password_stdin && self.email.is_some())
    }

    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    async fn request(&mut self) -> Result<LoginCredentials, LoginInputError> {
        let email = self.email.clone();
        let password_stdin = self.password_stdin;
        let interactive = self.interactive();
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
                    Completion::Success { .. } => "Setup completed.",
                    Completion::Failure { error } => error.message(),
                };
                writeln!(self.writer, "{message}").map_err(|_| ProgressUnavailable)?;
                if let Completion::Failure { error } = &result.completion {
                    write!(self.writer, "Stage: ").map_err(|_| ProgressUnavailable)?;
                    self.json(&error.step)?;
                    if let Some(location) = &error.location {
                        write!(self.writer, "Location: ").map_err(|_| ProgressUnavailable)?;
                        self.json(location)?;
                    }
                    if !error.tools.is_empty() {
                        write!(self.writer, "Tools: ").map_err(|_| ProgressUnavailable)?;
                        self.json(&error.tools)?;
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
    ) -> Result<devlink_broker::agent_profile::registration::Selection, AgentInputError> {
        if !self.interactive() {
            return Err(AgentInputError::Unavailable);
        }
        tokio::task::spawn_blocking(|| {
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
