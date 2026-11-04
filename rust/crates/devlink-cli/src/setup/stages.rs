use super::{
    AgentChoice, AgentInput, AgentInputError, ErrorCode, ErrorLocation, InputKind, InputSource,
    LoginCredentials, LoginInput, LoginInputError, McpInput, McpInputError, NoAgentInput,
    NoLoginInput, NoMcpInput, NoTeamInput, NoToolInput, Outcome, RequiredInput, SetupContext,
    SetupError, Step, StepExecutor, StepResult, TeamChoice, TeamInput, TeamInputError, ToolInput,
    ToolInputError,
};
use devlink_broker::{
    agent_profile::registration::{
        self, Registration, RegistrationContext, RegistrationState, Selection,
    },
    agent_profile::tokens,
    broker_control, mcp_config,
    sessions::ToolType,
    setup_config::{self, FailureKind, PreparedConfig},
    setup_connection, setup_mcp,
    setup_project::{self, PreparedProject, ProjectOptions},
    setup_tools::{self, SelectionBinding, SelectionSnapshot},
    teams::{self, SetupSelection, SetupTeamFault, SetupTeamFaultKind, Team},
    tool_discovery::{SelectedTool, ToolDetection, ToolSearch},
    user_auth::{self, AuthFault, AuthFaultKind, SessionCheck, User},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Production setup stages. Each completed feature is connected here incrementally.
/// Per-stage state lives in small structs, so swapping one input adapter moves a
/// handful of fields and a new stage adds its own struct instead of touching three
/// field lists.
pub struct SetupStages<
    I = NoLoginInput,
    T = NoTeamInput,
    U = NoToolInput,
    A = NoAgentInput,
    M = NoMcpInput,
> {
    input: I,
    team_input: T,
    tool_input: U,
    agent_input: A,
    mcp_input: M,
    prepared: Option<PreparedConfig>,
    // Held across stages and input waits for the lifetime of an orchestrated run.
    setup_lock: Option<std::fs::File>,
    auth: AuthState,
    teams: TeamState,
    project: ProjectState,
    tools: ToolState,
    agents: AgentState,
    broker: BrokerState,
    mcp: McpState,
    local_write: Arc<AtomicBool>,
}

struct LocalWrite(Arc<AtomicBool>);

impl LocalWrite {
    fn begin(flag: &Arc<AtomicBool>) -> Self {
        flag.store(true, Ordering::Release);
        Self(flag.clone())
    }
}

impl Drop for LocalWrite {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

#[derive(Default)]
struct AuthState {
    credentials: Option<LoginCredentials>,
    user: Option<User>,
    batch: Option<BatchAuth>,
}

struct BatchAuth {
    server_url: Option<String>,
    login: bool,
}

#[derive(Default)]
struct TeamState {
    requested: Option<u64>,
    choices: Vec<TeamChoice>,
    waiting: bool,
    selected: Option<Team>,
}

#[derive(Default)]
struct ProjectState {
    options: ProjectOptions,
    prepared: Option<PreparedProject>,
}

#[derive(Default)]
struct ToolState {
    requested: Option<ToolType>,
    path: Option<std::path::PathBuf>,
    search: ToolSearch,
    reports: Vec<ToolDetection>,
    snapshot: Option<SelectionSnapshot>,
    waiting: bool,
    selected: Option<SelectedTool>,
    batch_candidate: Option<SelectedTool>,
}

#[derive(Default)]
struct AgentState {
    issue_token: bool,
    selection: Option<Selection>,
    waiting: bool,
    registration: Option<Registration>,
    creation_pending: bool,
    issuance_pending: bool,
}

#[derive(Default)]
struct BrokerState {
    executable: Option<std::path::PathBuf>,
    result: Option<broker_control::StartResult>,
}

#[derive(Default)]
struct McpState {
    options: setup_mcp::Options,
    waiting: bool,
    confirmed: bool,
    prepared: Option<setup_mcp::Preparation>,
    result: Option<setup_mcp::ConfigurationResult>,
    binding: Option<setup_mcp::Context>,
    connection: Option<setup_connection::Report>,
}

impl Default for SetupStages<NoLoginInput> {
    fn default() -> Self {
        Self::new(NoLoginInput)
    }
}

impl<I> SetupStages<I, NoTeamInput> {
    pub fn new(input: I) -> Self {
        Self {
            input,
            team_input: NoTeamInput,
            tool_input: NoToolInput,
            agent_input: NoAgentInput,
            mcp_input: NoMcpInput,
            prepared: None,
            setup_lock: None,
            auth: AuthState::default(),
            teams: TeamState::default(),
            project: ProjectState::default(),
            tools: ToolState::default(),
            agents: AgentState::default(),
            broker: BrokerState::default(),
            mcp: McpState::default(),
            local_write: Arc::default(),
        }
    }
}

impl<I, T, U, A, M> SetupStages<I, T, U, A, M> {
    pub(super) fn with_batch(mut self, server_url: Option<String>, login: bool) -> Self {
        self.auth.batch = Some(BatchAuth { server_url, login });
        self
    }
    pub fn with_team_input<V>(self, input: V) -> SetupStages<I, V, U, A, M> {
        SetupStages {
            input: self.input,
            team_input: input,
            tool_input: self.tool_input,
            agent_input: self.agent_input,
            mcp_input: self.mcp_input,
            prepared: self.prepared,
            setup_lock: self.setup_lock,
            auth: self.auth,
            teams: self.teams,
            project: self.project,
            tools: self.tools,
            agents: self.agents,
            broker: self.broker,
            mcp: self.mcp,
            local_write: self.local_write,
        }
    }

    pub fn with_tool_input<V>(self, input: V) -> SetupStages<I, T, V, A, M> {
        SetupStages {
            input: self.input,
            team_input: self.team_input,
            tool_input: input,
            agent_input: self.agent_input,
            mcp_input: self.mcp_input,
            prepared: self.prepared,
            setup_lock: self.setup_lock,
            auth: self.auth,
            teams: self.teams,
            project: self.project,
            tools: self.tools,
            agents: self.agents,
            broker: self.broker,
            mcp: self.mcp,
            local_write: self.local_write,
        }
    }
    pub fn with_agent_input<V>(self, input: V) -> SetupStages<I, T, U, V, M> {
        SetupStages {
            input: self.input,
            team_input: self.team_input,
            tool_input: self.tool_input,
            agent_input: input,
            mcp_input: self.mcp_input,
            prepared: self.prepared,
            setup_lock: self.setup_lock,
            auth: self.auth,
            teams: self.teams,
            project: self.project,
            tools: self.tools,
            agents: self.agents,
            broker: self.broker,
            mcp: self.mcp,
            local_write: self.local_write,
        }
    }
    pub fn with_mcp_input<V>(self, input: V) -> SetupStages<I, T, U, A, V> {
        SetupStages {
            input: self.input,
            team_input: self.team_input,
            tool_input: self.tool_input,
            agent_input: self.agent_input,
            mcp_input: input,
            prepared: self.prepared,
            setup_lock: self.setup_lock,
            auth: self.auth,
            teams: self.teams,
            project: self.project,
            tools: self.tools,
            agents: self.agents,
            broker: self.broker,
            mcp: self.mcp,
            local_write: self.local_write,
        }
    }
    pub fn with_mcp(mut self, options: setup_mcp::Options) -> Self {
        self.mcp.options = options;
        self
    }
    pub fn mcp_configuration(&self) -> Option<&setup_mcp::ConfigurationResult> {
        self.mcp.result.as_ref()
    }
    pub fn with_agent(mut self, selection: Option<Selection>) -> Self {
        self.agents.selection = selection;
        self
    }
    pub fn with_issue_token(mut self, allow: bool) -> Self {
        self.agents.issue_token = allow;
        self
    }
    pub fn registration(&self) -> Option<&Registration> {
        self.agents.registration.as_ref()
    }

    pub fn with_tool(mut self, tool: Option<ToolType>, path: Option<std::path::PathBuf>) -> Self {
        self.tools.requested = tool;
        self.tools.path = path;
        self
    }
    pub fn with_tool_search(mut self, search: ToolSearch) -> Self {
        self.tools.search = search;
        self
    }
    pub fn tool(&self) -> Option<&SelectedTool> {
        self.tools.selected.as_ref()
    }

    /// Non-CLI callers must supply the actual devlink executable, not their host process.
    pub fn with_broker_executable(mut self, executable: std::path::PathBuf) -> Self {
        self.broker.executable = Some(executable);
        self
    }

    pub fn broker(&self) -> Option<&broker_control::StartResult> {
        self.broker.result.as_ref()
    }

    pub fn with_team_id(mut self, id: Option<u64>) -> Self {
        self.teams.requested = id;
        self
    }

    pub fn with_project(mut self, options: ProjectOptions) -> Self {
        self.project.options = options;
        self
    }

    pub fn project(&self) -> Option<&PreparedProject> {
        self.project.prepared.as_ref()
    }

    pub fn team(&self) -> Option<&Team> {
        self.teams.selected.as_ref()
    }

    pub fn configuration(&self) -> Option<&PreparedConfig> {
        self.prepared.as_ref()
    }

    pub fn user(&self) -> Option<&User> {
        self.auth.user.as_ref()
    }
}

impl<I: LoginInput, T: TeamInput, U: ToolInput, A: AgentInput, M: McpInput> StepExecutor
    for SetupStages<I, T, U, A, M>
{
    type Data = setup_connection::Report;

    async fn execute(&mut self, step: Step, context: &SetupContext) -> StepResult {
        if step == Step::Authenticate {
            return self.authenticate(context).await;
        }
        if step == Step::SelectTeam {
            return self.select_team(context).await;
        }
        if step == Step::SelectTool {
            return self.select_tool(context).await;
        }
        if step == Step::RegisterAgent {
            return self.register_agent(context).await;
        }
        if step == Step::ConfigureMcp {
            return self.configure_mcp(context).await;
        }
        if step == Step::VerifyConnection {
            return self.verify_connection().await;
        }
        if step == Step::StartBroker {
            let executable = self
                .broker
                .executable
                .clone()
                .map(Ok)
                .unwrap_or_else(std::env::current_exe);
            let result = match executable {
                Ok(executable) => broker_control::start(&context.config_path, &executable).await,
                Err(error) => Err(error.into()),
            };
            return match result {
                Ok(result) => {
                    let reused = result.reused;
                    self.broker.result = Some(result);
                    StepResult::Complete {
                        outcome: if reused {
                            Outcome::NotApplied
                        } else {
                            Outcome::Applied
                        },
                        reused,
                    }
                }
                Err(error) => {
                    let mut safe = broker_error(&context.config_path, &error);
                    safe.known.user_id = self.auth.user.as_ref().map(|u| u.user_id.to_string());
                    safe.known.team_id =
                        self.teams.selected.as_ref().map(|t| t.team_id.to_string());
                    safe.known.tool_type = self.tools.selected.as_ref().map(|t| t.tool_type);
                    safe.known.agent_id = self
                        .agents
                        .registration
                        .as_ref()
                        .and_then(|r| r.agent_id)
                        .map(|id| id.to_string());
                    StepResult::Failed(safe)
                }
            };
        }
        if step == Step::BindProject {
            let path = context.config_path.clone();
            let options = self.project.options.clone();
            return match tokio::task::spawn_blocking(move || {
                setup_project::prepare(&path, &options)
            })
            .await
            {
                Ok(Ok(project)) => {
                    let reused = project.reused;
                    self.project.prepared = Some(project);
                    StepResult::Complete {
                        outcome: Outcome::NotApplied,
                        reused,
                    }
                }
                Ok(Err(failure)) => {
                    let mut error = project_error(step, failure);
                    error.known.user_id = self.auth.user.as_ref().map(|u| u.user_id.to_string());
                    error.known.team_id =
                        self.teams.selected.as_ref().map(|t| t.team_id.to_string());
                    StepResult::Failed(error)
                }
                Err(_) => StepResult::Failed(SetupError::new(
                    ErrorCode::InternalError,
                    step,
                    Outcome::NotApplied,
                )),
            };
        }
        if step != Step::Validate {
            let mut error = SetupError::new(ErrorCode::StepUnavailable, step, Outcome::NotApplied);
            if let Some(user) = &self.auth.user {
                error.known.user_id = Some(user.user_id.to_string());
            }
            if let Some(team) = &self.teams.selected {
                error.known.team_id = Some(team.team_id.to_string());
            }
            error.known.agent_id = self
                .agents
                .registration
                .as_ref()
                .and_then(|r| r.agent_id)
                .map(|id| id.to_string());
            error.mcp_config = self.mcp.result.clone();
            error.tools = self.tools.reports.clone();
            error.known.tool_type = self.tools.selected.as_ref().map(|s| s.tool_type);
            return StepResult::Failed(error);
        }
        self.validate(context, false).await
    }

    async fn validate_run(&mut self, context: &SetupContext) -> StepResult {
        self.validate(context, true).await
    }

    fn can_resolve_input(&self) -> bool {
        if self.auth.batch.is_some() {
            return false;
        }
        if self.mcp.waiting {
            self.mcp_input.available()
        } else if self.agents.waiting {
            self.agent_input.available()
        } else if self.tools.waiting {
            self.tool_input.available()
        } else if self.teams.waiting {
            self.team_input.available()
        } else {
            self.input.available()
        }
    }
    fn input_is_interactive(&self) -> bool {
        if self.mcp.waiting {
            self.mcp_input.interactive()
        } else if self.agents.waiting {
            self.agent_input.interactive()
        } else if self.tools.waiting {
            self.tool_input.interactive()
        } else if self.teams.waiting {
            self.team_input.interactive()
        } else {
            self.input.interactive()
        }
    }

    async fn resolve_input(&mut self, required: &SetupError) -> Result<(), SetupError> {
        if required.step == Step::ConfigureMcp {
            let Some(plan) = self.mcp.prepared.as_ref().and_then(|p| p.plan()) else {
                return Err(SetupError::new(
                    ErrorCode::InternalError,
                    Step::ConfigureMcp,
                    Outcome::NotApplied,
                ));
            };
            match self.mcp_input.request(plan).await {
                Ok(true) => {
                    self.mcp.confirmed = true;
                    return Ok(());
                }
                Err(McpInputError::Unavailable) => return Err(required.clone()),
                Err(McpInputError::ReadFailed) => {
                    return Err(SetupError::new(
                        ErrorCode::InternalError,
                        Step::ConfigureMcp,
                        Outcome::NotApplied,
                    ));
                }
                Ok(false) | Err(McpInputError::Cancelled) => {
                    let mut cancelled = SetupError::new(
                        ErrorCode::Cancelled,
                        Step::ConfigureMcp,
                        Outcome::NotApplied,
                    );
                    cancelled.mcp_plan = required.mcp_plan.clone();
                    cancelled.known = required.known.clone();
                    return Err(cancelled);
                }
            }
        }
        if required.step == Step::RegisterAgent {
            let code = match self.agent_input.request(&required.agent_choices).await {
                Ok(selection) => {
                    self.agents.selection = Some(selection);
                    return Ok(());
                }
                Err(AgentInputError::Unavailable) => return Err(required.clone()),
                Err(AgentInputError::Invalid) => ErrorCode::InputInvalid,
                Err(AgentInputError::Cancelled) => ErrorCode::Cancelled,
                Err(AgentInputError::ReadFailed) => ErrorCode::InternalError,
            };
            let mut error = SetupError::new(code, Step::RegisterAgent, Outcome::NotApplied);
            error.known = required.known.clone();
            error.agent_choices = required.agent_choices.clone();
            error.agent_list_error = required.agent_list_error;
            return Err(error);
        }
        if required.step == Step::SelectTool {
            let code = match self.tool_input.request(&self.tools.reports).await {
                Ok(tool)
                    if self
                        .tools
                        .reports
                        .iter()
                        .any(|r| r.tool_type == tool && r.selected.is_some()) =>
                {
                    self.tools.requested = Some(tool);
                    return Ok(());
                }
                Ok(_) | Err(ToolInputError::Invalid) => ErrorCode::InputInvalid,
                Err(ToolInputError::Unavailable) => return Err(required.clone()),
                Err(ToolInputError::Cancelled) => ErrorCode::Cancelled,
                Err(ToolInputError::ReadFailed) => ErrorCode::InternalError,
            };
            let mut error = SetupError::new(code, Step::SelectTool, Outcome::NotApplied);
            error.known = required.known.clone();
            error.tools = self.tools.reports.clone();
            return Err(error);
        }
        if required.step == Step::SelectTeam {
            let code = match self.team_input.request(&self.teams.choices).await {
                Ok(id) => {
                    self.teams.requested = Some(id);
                    return Ok(());
                }
                Err(TeamInputError::Unavailable) => return Err(required.clone()),
                Err(TeamInputError::Cancelled) => ErrorCode::Cancelled,
                Err(TeamInputError::ReadFailed) => ErrorCode::InternalError,
                Err(TeamInputError::Invalid) => ErrorCode::InputInvalid,
            };
            let mut safe = SetupError::new(code, Step::SelectTeam, Outcome::NotApplied);
            safe.known = required.known.clone();
            return Err(safe);
        }
        let code = match self.input.request().await {
            Ok(credentials) => {
                self.auth.credentials = Some(credentials);
                return Ok(());
            }
            Err(LoginInputError::Unavailable) => return Err(required.clone()),
            Err(LoginInputError::Invalid) => ErrorCode::InputInvalid,
            Err(LoginInputError::Cancelled) => ErrorCode::Cancelled,
            Err(LoginInputError::ReadFailed) => ErrorCode::InternalError,
        };
        Err(SetupError::new(
            code,
            Step::Authenticate,
            Outcome::NotApplied,
        ))
    }

    fn finish(self) -> Self::Data {
        self.mcp
            .connection
            .expect("successful verification records the connection")
    }

    fn timed_out(&self, step: Step) -> SetupError {
        let (code, outcome) = match step {
            Step::RegisterAgent if self.agents.issuance_pending => {
                (ErrorCode::TokenRequired, Outcome::Unknown)
            }
            Step::RegisterAgent if self.agents.creation_pending => {
                (ErrorCode::RegistrationUnknown, Outcome::Unknown)
            }
            Step::RegisterAgent => (
                ErrorCode::Timeout,
                self.agents
                    .registration
                    .as_ref()
                    .map_or(Outcome::NotApplied, |registration| {
                        registration_outcome(registration.outcome)
                    }),
            ),
            Step::Validate
            | Step::BindProject
            | Step::SelectTool
            | Step::ConfigureMcp
            | Step::VerifyConnection => (ErrorCode::Timeout, Outcome::NotApplied),
            _ => (ErrorCode::Timeout, Outcome::Unknown),
        };
        let mut error = SetupError::new(code, step, outcome);
        self.mcp_identity(&mut error);
        error.mcp_config = self.mcp.result.clone();
        error
    }

    fn local_write(&self) -> Option<Arc<AtomicBool>> {
        Some(self.local_write.clone())
    }
}

fn config_error(failure: setup_config::Failure) -> SetupError {
    let code = match failure.kind {
        FailureKind::InvalidPath => ErrorCode::ConfigPathInvalid,
        FailureKind::InvalidConfig => ErrorCode::ConfigInvalid,
        FailureKind::ServerConflict => ErrorCode::ProfileConflict,
        FailureKind::UnsafeFile => ErrorCode::UnsafeFile,
        FailureKind::ConfigBusy => ErrorCode::ConfigBusy,
        FailureKind::BrokerBusy => ErrorCode::BrokerBusy,
        FailureKind::Storage => ErrorCode::ConfigStorageFailed,
    };
    let outcome = if failure.mutation_uncertain {
        Outcome::Unknown
    } else if failure.created {
        Outcome::Applied
    } else {
        Outcome::NotApplied
    };
    let mut error = SetupError::new(code, Step::Validate, outcome);
    error.location = Some(ErrorLocation {
        file: failure.file,
        field: failure.field,
    });
    error
}

fn project_error(step: Step, failure: setup_project::Failure) -> SetupError {
    let code = match failure.kind {
        setup_project::FailureKind::InvalidPath => ErrorCode::ProjectPathInvalid,
        setup_project::FailureKind::InvalidMetadata => ErrorCode::InputInvalid,
        setup_project::FailureKind::BindingConflict => ErrorCode::ProjectBindingConflict,
        setup_project::FailureKind::Storage => ErrorCode::ProjectStorageFailed,
        setup_project::FailureKind::Busy => ErrorCode::ConfigBusy,
    };
    let mut error = SetupError::new(code, step, Outcome::NotApplied);
    error.location = failure.file.map(|file| ErrorLocation {
        file,
        field: Some(failure.field),
    });
    if code == ErrorCode::InputInvalid {
        error.required_inputs.push(RequiredInput {
            field: failure.field,
            kind: InputKind::Value,
            secret: false,
            source: InputSource::Input,
        });
    }
    error
}

fn broker_error(path: &std::path::Path, error: &anyhow::Error) -> SetupError {
    use broker_control::{Failure, FailureKind};
    let failure = error.downcast_ref::<Failure>();
    let code = match failure.map(|f| f.kind) {
        Some(FailureKind::InvalidConfig) => ErrorCode::ConfigInvalid,
        Some(FailureKind::ConfigBusy) => ErrorCode::ConfigBusy,
        Some(FailureKind::BrokerBusy) => ErrorCode::BrokerBusy,
        // Lock held, runtime not yet published: retryable like a running broker.
        Some(FailureKind::BrokerStarting) => ErrorCode::BrokerBusy,
        Some(
            FailureKind::ControlLockFailed | FailureKind::BrokerLockFailed | FailureKind::Storage,
        ) => ErrorCode::UnsafeFile,
        Some(FailureKind::TokenUnavailable) => ErrorCode::TokenRequired,
        _ => ErrorCode::BrokerStartFailed,
    };
    let mut safe = SetupError::new(
        code,
        Step::StartBroker,
        if failure.is_some_and(|f| f.outcome == broker_control::Outcome::Unknown) {
            Outcome::Unknown
        } else {
            Outcome::NotApplied
        },
    );
    safe.location = Some(ErrorLocation {
        file: failure
            .map(|f| f.file.clone())
            .unwrap_or_else(|| path.to_path_buf()),
        field: None,
    });
    safe
}

impl<I: LoginInput, T: TeamInput, U: ToolInput, A: AgentInput, M: McpInput>
    SetupStages<I, T, U, A, M>
{
    async fn select_tool(&mut self, context: &SetupContext) -> StepResult {
        let (Some(config), Some(user), Some(team), Some(project)) = (
            &self.prepared,
            &self.auth.user,
            &self.teams.selected,
            &self.project.prepared,
        ) else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::InternalError,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        };
        let Ok(origin) = user_auth::origin(&config.config) else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::ConfigInvalid,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        };
        let binding = SelectionBinding {
            origin: origin.to_string(),
            user_id: user.user_id,
            team_id: team.team_id,
            project_root: project.root.clone(),
        };
        let path = context.config_path.clone();
        let snapshot =
            match tokio::task::spawn_blocking(move || setup_tools::read(&path, &binding)).await {
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(failure)) => return self.tool_failure(tool_storage_error(failure)),
                Err(_) => {
                    return self.tool_failure(SetupError::new(
                        ErrorCode::InternalError,
                        Step::SelectTool,
                        Outcome::NotApplied,
                    ));
                }
            };
        if self.tools.waiting && self.tools.snapshot.as_ref() != Some(&snapshot) {
            return self.tool_failure(SetupError::new(
                ErrorCode::ToolSelectionConflict,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        }
        if snapshot.bound_tool.is_some_and(|bound| {
            self.tools
                .requested
                .is_some_and(|requested| requested != bound)
        }) {
            return self.tool_failure(SetupError::new(
                ErrorCode::ToolBindingConflict,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        }
        let saved = snapshot.preferred().filter(|saved| {
            self.auth.batch.is_some() && self.tools.requested == Some(saved.tool_type)
        });
        let batch_candidate = self
            .tools
            .batch_candidate
            .clone()
            .filter(|_| saved.is_none());
        let preferred = batch_candidate.as_ref().or_else(|| snapshot.preferred());
        let pinned = self.tools.requested.or(snapshot.bound_tool);
        let requested = pinned.or_else(|| preferred.map(|s| s.tool_type));
        let explicit = requested.zip(self.tools.path.as_deref());
        // Probe only what is needed. A pinned tool (explicit, bound, or just chosen)
        // is probed alone, so a host removed during input is still caught while the
        // other host's earlier report stays in the choice list. A saved, unbound
        // choice is probed first; the full scan runs only when it is gone.
        self.tools.reports = match (pinned, requested) {
            (Some(tool), _) => {
                let report = match saved {
                    Some(saved) => self.tools.search.recheck_saved(saved).await,
                    None => {
                        self.tools
                            .search
                            .detect_one(tool, preferred, explicit)
                            .await
                    }
                };
                let mut reports = if self.tools.waiting {
                    std::mem::take(&mut self.tools.reports)
                } else {
                    Vec::new()
                };
                match reports.iter_mut().find(|r| r.tool_type == tool) {
                    Some(slot) => *slot = report,
                    None => reports.push(report),
                }
                reports
            }
            (None, Some(tool)) => {
                let report = self
                    .tools
                    .search
                    .detect_one(tool, preferred, explicit)
                    .await;
                if report.selected.is_some() {
                    vec![report]
                } else {
                    self.tools.search.detect(preferred, explicit).await
                }
            }
            (None, None) => self.tools.search.detect(preferred, explicit).await,
        };
        let mut selected = requested.and_then(|tool| {
            self.tools
                .reports
                .iter()
                .find(|r| r.tool_type == tool)
                .and_then(|r| r.selected.clone())
        });
        if let (Some(expected), Some(current)) = (&batch_candidate, &selected)
            && expected != current
        {
            return self.tool_failure(SetupError::new(
                ErrorCode::ToolSelectionConflict,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        }
        if let Some(saved) = saved
            && selected.is_none()
        {
            return self.tool_failure(SetupError::saved_tool_unavailable(
                saved
                    .executable
                    .args
                    .first()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| saved.executable.command.clone()),
            ));
        }
        if selected.is_none() && (self.tools.requested.is_some() || snapshot.bound_tool.is_some()) {
            return self.tool_failure(SetupError::new(
                ErrorCode::ToolUnavailable,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        }
        if selected.is_none() {
            let available: Vec<_> = self
                .tools
                .reports
                .iter()
                .filter_map(|r| r.selected.clone())
                .collect();
            match available.as_slice() {
                [only] => selected = Some(only.clone()),
                [] => {
                    return self.tool_failure(SetupError::new(
                        ErrorCode::ToolUnavailable,
                        Step::SelectTool,
                        Outcome::NotApplied,
                    ));
                }
                _ => {
                    self.tools.snapshot = Some(snapshot);
                    self.tools.waiting = true;
                    let mut error = SetupError::new(
                        ErrorCode::ToolRequired,
                        Step::SelectTool,
                        Outcome::NotApplied,
                    );
                    error.required_inputs.push(RequiredInput {
                        field: "tool.type",
                        kind: InputKind::Selection,
                        secret: false,
                        source: InputSource::DetectedToolList,
                    });
                    return StepResult::InputRequired(self.tool_error(error));
                }
            }
        }
        let Some(selected) = selected else {
            return self.tool_failure(SetupError::new(
                ErrorCode::InternalError,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        };
        if self.auth.batch.is_some()
            && batch_candidate.is_none()
            && let Err(error) = self
                .preflight_mcp(
                    context,
                    Step::SelectTool,
                    selected.tool_type,
                    Some(selected.clone()),
                )
                .await
        {
            return self.tool_failure(*error);
        }
        let path = context.config_path.clone();
        let publish = selected.clone();
        let reuse_bound = snapshot.bound_tool == Some(selected.tool_type);
        let _write = LocalWrite::begin(&self.local_write);
        match tokio::task::spawn_blocking(move || setup_tools::publish(&path, &snapshot, &publish))
            .await
        {
            Ok(Ok(reused)) => {
                self.tools.selected = Some(selected);
                self.tools.waiting = false;
                StepResult::Complete {
                    outcome: if reused {
                        Outcome::NotApplied
                    } else {
                        Outcome::Applied
                    },
                    reused: reused || reuse_bound,
                }
            }
            Ok(Err(failure)) => self.tool_failure(tool_storage_error(failure)),
            Err(_) => self.tool_failure(SetupError::new(
                ErrorCode::InternalError,
                Step::SelectTool,
                Outcome::Unknown,
            )),
        }
    }

    fn tool_failure(&self, error: SetupError) -> StepResult {
        StepResult::Failed(self.tool_error(error))
    }

    fn tool_error(&self, mut error: SetupError) -> SetupError {
        error.known.user_id = self.auth.user.as_ref().map(|u| u.user_id.to_string());
        error.known.team_id = self.teams.selected.as_ref().map(|t| t.team_id.to_string());
        error.tools = self.tools.reports.clone();
        error
    }

    async fn select_team(&mut self, context: &SetupContext) -> StepResult {
        let (Some(prepared), Some(user)) = (&self.prepared, &self.auth.user) else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::InternalError,
                Step::SelectTeam,
                Outcome::NotApplied,
            ));
        };
        let Ok(origin) = user_auth::origin(&prepared.config) else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::ConfigInvalid,
                Step::SelectTeam,
                Outcome::NotApplied,
            ));
        };
        let result = teams::select_for_setup(
            &context.config_path,
            origin.as_str(),
            user.user_id,
            self.teams.requested,
        )
        .await;
        let mut error = match result {
            Ok(SetupSelection::Selected { team, reused }) => {
                self.teams.selected = Some(team);
                self.teams.waiting = false;
                return StepResult::Complete {
                    outcome: if reused {
                        Outcome::NotApplied
                    } else {
                        Outcome::Applied
                    },
                    reused,
                };
            }
            Ok(SetupSelection::Choose(teams)) => {
                self.teams.choices = teams
                    .into_iter()
                    .map(|team| TeamChoice {
                        id: team.team_id.to_string(),
                        name: team.name,
                    })
                    .collect();
                self.teams.waiting = true;
                let mut required = SetupError::new(
                    ErrorCode::TeamRequired,
                    Step::SelectTeam,
                    Outcome::NotApplied,
                );
                required.required_inputs = vec![RequiredInput {
                    field: "team.id",
                    kind: InputKind::Selection,
                    secret: false,
                    source: InputSource::VerifiedTeamList,
                }];
                required.team_choices = self.teams.choices.clone();
                required.known.user_id = Some(user.user_id.to_string());
                return StepResult::InputRequired(required);
            }
            Ok(SetupSelection::NoTeams) => SetupError::new(
                ErrorCode::TeamMembershipRequired,
                Step::SelectTeam,
                Outcome::NotApplied,
            ),
            Err(error) => team_error(&context.config_path, &error),
        };
        error.known.user_id = Some(user.user_id.to_string());
        StepResult::Failed(error)
    }

    async fn authenticate(&mut self, context: &SetupContext) -> StepResult {
        let Some(prepared) = &self.prepared else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::ConfigInvalid,
                Step::Authenticate,
                Outcome::NotApplied,
            ));
        };
        let Ok(expected_origin) = user_auth::origin(&prepared.config) else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::ConfigInvalid,
                Step::Authenticate,
                Outcome::NotApplied,
            ));
        };
        if self.auth.batch.as_ref().is_some_and(|batch| batch.login) {
            let Some(credentials) = self.auth.credentials.take() else {
                return StepResult::Failed(SetupError::new(
                    ErrorCode::InternalError,
                    Step::Authenticate,
                    Outcome::NotApplied,
                ));
            };
            return match user_auth::login_for_setup_explicit(
                &context.config_path,
                credentials.email(),
                credentials.password(),
                expected_origin.as_str(),
            )
            .await
            {
                Ok(user) => {
                    self.auth.user = Some(user);
                    StepResult::Complete {
                        outcome: Outcome::Applied,
                        reused: false,
                    }
                }
                Err(error) => StepResult::Failed(auth_error(&context.config_path, &error, true)),
            };
        }
        let checked =
            user_auth::check_session(&context.config_path, expected_origin.as_str()).await;
        match checked {
            Ok(SessionCheck::Authenticated(user)) => {
                // Another process may have signed in while input was being collected.
                self.auth.credentials = None;
                self.auth.user = Some(user);
                StepResult::Complete {
                    outcome: Outcome::Applied,
                    reused: true,
                }
            }
            Ok(SessionCheck::LoginRequired { expired_now }) => {
                let Some(credentials) = self.auth.credentials.take() else {
                    let mut error = SetupError::new(
                        ErrorCode::AuthRequired,
                        Step::Authenticate,
                        if expired_now {
                            Outcome::Applied
                        } else {
                            Outcome::NotApplied
                        },
                    );
                    error.required_inputs = if self.auth.batch.is_some() {
                        vec![super::batch::required("user.mode")]
                    } else {
                        vec![
                            RequiredInput {
                                field: "user.email",
                                kind: InputKind::Value,
                                secret: false,
                                source: InputSource::Input,
                            },
                            RequiredInput {
                                field: "password",
                                kind: InputKind::Secret,
                                secret: true,
                                source: InputSource::HiddenPromptOrStdin,
                            },
                        ]
                    };
                    return StepResult::InputRequired(error);
                };
                match user_auth::login_for_setup(
                    &context.config_path,
                    credentials.email(),
                    credentials.password(),
                    expected_origin.as_str(),
                )
                .await
                {
                    Ok(login) => {
                        self.auth.user = Some(login.user);
                        StepResult::Complete {
                            outcome: Outcome::Applied,
                            reused: login.reused,
                        }
                    }
                    Err(error) => {
                        StepResult::Failed(auth_error(&context.config_path, &error, true))
                    }
                }
            }
            Ok(SessionCheck::OriginMismatch) => StepResult::Failed(local_session_error(
                &context.config_path,
                ErrorCode::AuthSessionConflict,
            )),
            Ok(SessionCheck::InvalidLocalSession) => StepResult::Failed(local_session_error(
                &context.config_path,
                ErrorCode::UnsafeFile,
            )),
            Err(error) => StepResult::Failed(auth_error(&context.config_path, &error, false)),
        }
    }
}

fn tool_storage_error(failure: setup_tools::Failure) -> SetupError {
    let code = match failure.kind {
        setup_tools::FailureKind::Storage => ErrorCode::ToolStorageFailed,
        setup_tools::FailureKind::Busy
            if failure
                .file
                .file_name()
                .is_some_and(|name| name == "account.lock") =>
        {
            ErrorCode::AuthBusy
        }
        setup_tools::FailureKind::Busy => ErrorCode::ConfigBusy,
        setup_tools::FailureKind::BindingConflict => ErrorCode::ToolBindingConflict,
        setup_tools::FailureKind::SessionConflict => ErrorCode::AuthSessionConflict,
        setup_tools::FailureKind::SelectionConflict => ErrorCode::ToolSelectionConflict,
    };
    let mut error = SetupError::new(
        code,
        Step::SelectTool,
        if failure.mutation_uncertain {
            Outcome::Unknown
        } else {
            Outcome::NotApplied
        },
    );
    error.location = Some(ErrorLocation {
        file: failure.file,
        field: None,
    });
    error
}

fn team_error(path: &std::path::Path, error: &anyhow::Error) -> SetupError {
    if let Some(fault) = error.downcast_ref::<SetupTeamFault>() {
        if fault.kind == SetupTeamFaultKind::ListResourceNotFound {
            return SetupError::new(
                ErrorCode::TeamListResourceNotFound,
                Step::SelectTeam,
                Outcome::NotApplied,
            );
        }
        let code = match fault.kind {
            SetupTeamFaultKind::Forbidden => ErrorCode::TeamForbidden,
            SetupTeamFaultKind::ListResourceNotFound => ErrorCode::TeamListResourceNotFound,
            SetupTeamFaultKind::Protocol => ErrorCode::ProtocolError,
            SetupTeamFaultKind::Storage => ErrorCode::TeamStorageFailed,
            SetupTeamFaultKind::BindingConflict => ErrorCode::TeamBindingConflict,
            SetupTeamFaultKind::Busy => ErrorCode::ConfigBusy,
            SetupTeamFaultKind::SessionConflict => ErrorCode::AuthSessionConflict,
        };
        let mut safe = SetupError::new(
            code,
            Step::SelectTeam,
            if fault.mutation_uncertain {
                Outcome::Unknown
            } else {
                Outcome::NotApplied
            },
        );
        safe.location = Some(ErrorLocation {
            file: fault.file.clone().unwrap_or_else(|| {
                if code == ErrorCode::TeamBindingConflict {
                    devlink_broker::agent_profile::path(path)
                } else if code == ErrorCode::ConfigBusy {
                    devlink_broker::config::state_directory(path).join("control.lock")
                } else {
                    teams::selection_path(path)
                }
            }),
            field: None,
        });
        return safe;
    }
    let mut safe = match user_auth::rejection_status(error) {
        Some(401) => SetupError::new(ErrorCode::AuthRequired, Step::SelectTeam, Outcome::Applied),
        Some(403 | 404) => SetupError::new(
            ErrorCode::TeamForbidden,
            Step::SelectTeam,
            Outcome::NotApplied,
        ),
        _ => auth_error(path, error, false),
    };
    safe.step = Step::SelectTeam;
    safe
}

fn local_session_error(path: &std::path::Path, code: ErrorCode) -> SetupError {
    let mut error = SetupError::new(code, Step::Authenticate, Outcome::NotApplied);
    error.location = Some(ErrorLocation {
        file: user_auth::session_path(path),
        field: None,
    });
    error
}

fn auth_error(path: &std::path::Path, error: &anyhow::Error, login_attempted: bool) -> SetupError {
    let status = user_auth::rejection_status(error);
    let kind = error.downcast_ref::<AuthFault>().map(|fault| fault.kind);
    let code = match kind {
        Some(AuthFaultKind::Unavailable) => ErrorCode::CentralUnavailable,
        Some(AuthFaultKind::Protocol | AuthFaultKind::IdentityMismatch) => ErrorCode::ProtocolError,
        Some(AuthFaultKind::Busy) => ErrorCode::AuthBusy,
        Some(AuthFaultKind::ConfigBusy) => ErrorCode::ConfigBusy,
        Some(AuthFaultKind::BrokerBusy) => ErrorCode::BrokerBusy,
        Some(AuthFaultKind::ConfigInvalid) => ErrorCode::ConfigInvalid,
        Some(AuthFaultKind::ConfigUnreadable) => ErrorCode::ConfigStorageFailed,
        Some(AuthFaultKind::ControlLockFailed | AuthFaultKind::BrokerLockFailed) => {
            ErrorCode::UnsafeFile
        }
        Some(AuthFaultKind::SessionConflict) => ErrorCode::AuthSessionConflict,
        None => match status {
            Some(408 | 429 | 500..=599) => ErrorCode::CentralUnavailable,
            Some(400..=499) => ErrorCode::AuthRejected,
            Some(_) => ErrorCode::ProtocolError,
            None => ErrorCode::AuthStorageFailed,
        },
    };
    let mut safe = local_session_error(path, code);
    let state = || devlink_broker::config::state_directory(path);
    let location = match kind {
        Some(AuthFaultKind::ConfigInvalid) => Some((
            path.to_path_buf(),
            Some(
                error
                    .downcast_ref::<devlink_broker::config::ConfigValidationError>()
                    .map_or("$", |invalid| invalid.field),
            ),
        )),
        Some(AuthFaultKind::ConfigUnreadable) => Some((path.to_path_buf(), None)),
        Some(AuthFaultKind::ConfigBusy | AuthFaultKind::ControlLockFailed) => {
            Some((state().join("control.lock"), None))
        }
        Some(AuthFaultKind::BrokerBusy | AuthFaultKind::BrokerLockFailed) => {
            Some((state().join("broker.lock"), None))
        }
        Some(AuthFaultKind::Busy) => Some((state().join("account.lock"), None)),
        _ => None,
    };
    if let Some((file, field)) = location {
        safe.location = Some(ErrorLocation { file, field });
    }
    safe.outcome = if code == ErrorCode::AuthStorageFailed
        || (login_attempted
            && matches!(
                code,
                ErrorCode::CentralUnavailable | ErrorCode::ProtocolError
            )) {
        Outcome::Unknown
    } else if kind == Some(AuthFaultKind::IdentityMismatch) {
        Outcome::Applied
    } else {
        Outcome::NotApplied
    };
    if user_auth::session_was_invalidated(error) && safe.outcome != Outcome::Unknown {
        safe.outcome = Outcome::Applied;
    }
    safe
}

impl<I: LoginInput, T: TeamInput, U: ToolInput, A: AgentInput, M: McpInput>
    SetupStages<I, T, U, A, M>
{
    async fn register_agent(&mut self, context: &SetupContext) -> StepResult {
        // A tool chosen after Validate, or a local change since that check, must
        // still be rejected before registration and token issuance can mutate central.
        if let Some(tool) = &self.tools.selected
            && let Err(error) = self
                .preflight_mcp(
                    context,
                    Step::RegisterAgent,
                    tool.tool_type,
                    Some(tool.clone()),
                )
                .await
        {
            return StepResult::Failed(*error);
        }
        let (Some(config), Some(user), Some(team), Some(project), Some(tool)) = (
            &self.prepared,
            &self.auth.user,
            &self.teams.selected,
            &self.project.prepared,
            &self.tools.selected,
        ) else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::InternalError,
                Step::RegisterAgent,
                Outcome::NotApplied,
            ));
        };
        let Ok(origin) = user_auth::origin(&config.config) else {
            return StepResult::Failed(SetupError::new(
                ErrorCode::ConfigInvalid,
                Step::RegisterAgent,
                Outcome::NotApplied,
            ));
        };
        let expected = RegistrationContext {
            origin: origin.to_string(),
            user_id: user.user_id,
            team_id: team.team_id,
            project: project.clone(),
            tool: tool.tool_type,
        };
        self.agents.creation_pending = false;
        self.agents.issuance_pending = false;
        let result = registration::ensure_tracked(
            &context.config_path,
            &expected,
            self.agents.selection,
            &mut self.agents.creation_pending,
        )
        .await;
        self.agents.creation_pending = matches!(
            &result, Ok(registration) if registration.state == RegistrationState::CreationUnknown
        );
        let result = match result {
            Ok(mut registered)
                if matches!(
                    registered.state,
                    RegistrationState::Ready | RegistrationState::TokenRequired
                ) =>
            {
                let mut token_context = expected.clone();
                if let Some(metadata) = &registered.metadata {
                    token_context.project.metadata = metadata.clone();
                }
                let id = registered.agent_id.unwrap_or(0);
                // A grant belongs to one attempt, including an unknown outcome.
                // Retrying this stage cannot silently reuse an issuance grant.
                let explicit_issue = std::mem::take(&mut self.agents.issue_token);
                let permission = registered.created || explicit_issue;
                self.agents.registration = Some(registered.clone());
                let issued = tokens::ensure_tracked(
                    &context.config_path,
                    &token_context,
                    id,
                    permission,
                    &mut self.agents.issuance_pending,
                )
                .await;
                // Only an unconfirmed POST stays pending; a definite rejection
                // must not be reported as an unknown issuance by a later interrupt.
                self.agents.issuance_pending = issuance_unconfirmed(&issued);
                match issued {
                    Ok(prepared) => {
                        registered.state = if prepared.verified {
                            RegistrationState::Ready
                        } else {
                            RegistrationState::TokenRequired
                        };
                        registered.outcome =
                            combine_registration_outcome(registered.outcome, prepared.outcome);
                        if prepared.metadata.is_some() {
                            registered.metadata = prepared.metadata;
                        }
                        Ok(registered)
                    }
                    Err(mut failure) => {
                        failure.outcome =
                            combine_registration_outcome(registered.outcome, failure.outcome);
                        self.agents.registration = Some(registered);
                        Err(failure)
                    }
                }
            }
            other => other,
        };
        let mut error = match result {
            Ok(registration) => {
                let outcome = registration_outcome(registration.outcome);
                let reused = registration.reused;
                let state = registration.state;
                if let Some(metadata) = &registration.metadata
                    && let Some(project) = &mut self.project.prepared
                {
                    project.metadata = metadata.clone();
                }
                self.agents.registration = Some(registration);
                match state {
                    RegistrationState::Ready => {
                        self.agents.waiting = false;
                        return StepResult::Complete { outcome, reused };
                    }
                    RegistrationState::Vacant => {
                        self.agents.waiting = true;
                        let mut error =
                            SetupError::new(ErrorCode::InputRequired, Step::RegisterAgent, outcome);
                        error.required_inputs.push(RequiredInput {
                            field: "agent.action",
                            kind: InputKind::Selection,
                            secret: false,
                            source: InputSource::Input,
                        });
                        error.known.user_id = Some(user.user_id.to_string());
                        error.known.team_id = Some(team.team_id.to_string());
                        error.known.tool_type = Some(tool.tool_type);
                        add_agent_choices(&mut error, &context.config_path, &expected).await;
                        if let Some(code) = error.agent_list_error {
                            self.agents.waiting = false;
                            let mut unavailable =
                                SetupError::new(code, Step::RegisterAgent, outcome);
                            unavailable.known = error.known;
                            unavailable.agent_list_error = Some(code);
                            return StepResult::Failed(unavailable);
                        }
                        return StepResult::InputRequired(error);
                    }
                    RegistrationState::CreationUnknown => {
                        let mut error = SetupError::new(
                            ErrorCode::RegistrationUnknown,
                            Step::RegisterAgent,
                            outcome,
                        );
                        error.required_inputs.push(RequiredInput {
                            field: "agent.id",
                            kind: InputKind::Selection,
                            secret: false,
                            source: InputSource::OwnedAgentList,
                        });
                        error
                    }
                    RegistrationState::TokenRequired => {
                        let mut error =
                            SetupError::new(ErrorCode::TokenRequired, Step::RegisterAgent, outcome);
                        error.required_inputs.push(RequiredInput {
                            field: "agent.issueToken",
                            kind: InputKind::Value,
                            secret: false,
                            source: InputSource::Input,
                        });
                        error
                    }
                }
            }
            Err(failure) => {
                let code = registration_error_code(failure.kind);
                let mut error = SetupError::new(
                    code,
                    Step::RegisterAgent,
                    registration_outcome(failure.outcome),
                );
                error.known.agent_id = failure.agent_id.map(|id| id.to_string());
                if code == ErrorCode::AgentForbidden {
                    error.required_inputs.push(RequiredInput {
                        field: "agent.id",
                        kind: InputKind::Selection,
                        secret: false,
                        source: InputSource::OwnedAgentList,
                    });
                }
                if code == ErrorCode::AgentAlreadyExists {
                    error.required_inputs.push(RequiredInput {
                        field: "agent.action",
                        kind: InputKind::Selection,
                        secret: false,
                        source: InputSource::Input,
                    });
                }
                if code == ErrorCode::AgentStorageFailed {
                    error.location = Some(ErrorLocation {
                        file: devlink_broker::agent_profile::path(&context.config_path),
                        field: None,
                    });
                }
                error
            }
        };
        error.known.user_id = Some(user.user_id.to_string());
        error.known.team_id = Some(team.team_id.to_string());
        error.known.tool_type = Some(tool.tool_type);
        if error.known.agent_id.is_none() {
            error.known.agent_id = self
                .agents
                .registration
                .as_ref()
                .and_then(|r| r.agent_id)
                .map(|id| id.to_string());
        }
        if matches!(
            error.code,
            ErrorCode::RegistrationUnknown
                | ErrorCode::AgentAlreadyExists
                | ErrorCode::AgentForbidden
        ) {
            add_agent_choices(&mut error, &context.config_path, &expected).await;
        }
        StepResult::Failed(error)
    }
}

fn registration_error_code(kind: registration::FailureKind) -> ErrorCode {
    match kind {
        registration::FailureKind::Storage => ErrorCode::AgentStorageFailed,
        registration::FailureKind::Busy => ErrorCode::AuthBusy,
        registration::FailureKind::ConfigBusy => ErrorCode::ConfigBusy,
        registration::FailureKind::BrokerBusy => ErrorCode::BrokerBusy,
        registration::FailureKind::BindingConflict => ErrorCode::AgentBindingConflict,
        registration::FailureKind::Forbidden => ErrorCode::AgentForbidden,
        registration::FailureKind::SessionConflict => ErrorCode::AuthSessionConflict,
        registration::FailureKind::Rejected => ErrorCode::AgentRejected,
        registration::FailureKind::AlreadyExists => ErrorCode::AgentAlreadyExists,
        registration::FailureKind::AuthRejected => ErrorCode::AuthRequired,
        registration::FailureKind::Unavailable => ErrorCode::CentralUnavailable,
        registration::FailureKind::Protocol => ErrorCode::ProtocolError,
    }
}

async fn add_agent_choices(
    error: &mut SetupError,
    path: &std::path::Path,
    expected: &RegistrationContext,
) {
    match registration::owned_agents(path, expected).await {
        Ok(agents) => {
            error.agent_choices = agents
                .into_iter()
                .map(|agent| AgentChoice {
                    id: agent.agent_id.to_string(),
                    name: agent.name,
                    project_name: agent.project_name,
                    tool_type: expected.tool,
                })
                .collect();
        }
        // The failed read must not replace a known duplicate or an unknown creation outcome.
        Err(failure) => error.agent_list_error = Some(registration_error_code(failure.kind)),
    }
}
fn combine_registration_outcome(
    left: registration::Mutation,
    right: registration::Mutation,
) -> registration::Mutation {
    use registration::Mutation::{Applied, NotApplied, Unknown};
    match (left, right) {
        (Unknown, _) | (_, Unknown) => Unknown,
        (Applied, _) | (_, Applied) => Applied,
        _ => NotApplied,
    }
}
fn issuance_unconfirmed(result: &Result<tokens::TokenPreparation, registration::Failure>) -> bool {
    matches!(
        result,
        Ok(prepared) if !prepared.verified && prepared.outcome == registration::Mutation::Unknown
    )
}

fn registration_outcome(outcome: registration::Mutation) -> Outcome {
    match outcome {
        registration::Mutation::NotApplied => Outcome::NotApplied,
        registration::Mutation::Applied => Outcome::Applied,
        registration::Mutation::Unknown => Outcome::Unknown,
    }
}

impl<I: LoginInput, T: TeamInput, U: ToolInput, A: AgentInput, M: McpInput>
    SetupStages<I, T, U, A, M>
{
    async fn validate(&mut self, context: &SetupContext, coordinated: bool) -> StepResult {
        let step = Step::Validate;
        if let Err(error) = self.mcp.options.validate() {
            return StepResult::Failed(mcp_error(step, error));
        }
        // Reject an invalid directory before initializing settings or contacting
        // central. Display names wait for BindProject, where a saved profile may
        // still supply them (a filesystem root has no folder name to default from).
        let options = self.project.options.clone();
        match tokio::task::spawn_blocking(move || setup_project::resolve_root(&options)).await {
            Ok(Ok(root)) => {
                // Freeze cwd now; later stages must not resolve a relative path again.
                self.project.options.path = Some(root);
            }
            Ok(Err(failure)) => return StepResult::Failed(project_error(step, failure)),
            Err(_) => {
                return StepResult::Failed(SetupError::new(
                    ErrorCode::InternalError,
                    step,
                    Outcome::NotApplied,
                ));
            }
        }
        // A saved launcher must be checked against the authenticated binding in
        // SelectTool. Defer its MCP preflight to RegisterAgent, before any Agent
        // or token mutation. An explicit path takes precedence over saved state.
        if self.auth.batch.is_some() {
            if let Err(error) = self.preflight_batch(context).await {
                return StepResult::Failed(*error);
            }
        } else if let Some(tool) = self.tools.requested
            && (self.tools.path.is_some()
                || matches!(
                    std::fs::symlink_metadata(setup_tools::path(&context.config_path)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound
                ))
        {
            // Resolve the Host before checking its runtime: an npm shim's Node may
            // live only beside the shim, and only a verified version is applied
            // automatically. select_tool probes again; a Host missing here fails there.
            let explicit = self.tools.path.as_deref().map(|path| (tool, path));
            let host = self
                .tools
                .search
                .detect_one(tool, None, explicit)
                .await
                .selected;
            if let Err(error) = self
                .preflight_mcp(context, Step::Validate, tool, host)
                .await
            {
                return StepResult::Failed(*error);
            }
        }
        let path = context.config_path.clone();
        let server_url = self
            .auth
            .batch
            .as_ref()
            .and_then(|batch| batch.server_url.clone());
        let _write = LocalWrite::begin(&self.local_write);
        match tokio::task::spawn_blocking(move || {
            if coordinated {
                setup_config::prepare_for_run(&path, server_url.as_deref())
                    .map(|(prepared, lock)| (prepared, Some(lock)))
            } else {
                setup_config::prepare_with_server(&path, server_url.as_deref())
                    .map(|prepared| (prepared, None))
            }
        })
        .await
        {
            Ok(Ok((prepared, lock))) => {
                let created = prepared.created;
                self.prepared = Some(prepared);
                self.setup_lock = lock;
                StepResult::Complete {
                    outcome: if created {
                        Outcome::Applied
                    } else {
                        Outcome::NotApplied
                    },
                    reused: !created,
                }
            }
            Ok(Err(failure)) => StepResult::Failed(config_error(failure)),
            Err(_) => StepResult::Failed(SetupError::new(
                ErrorCode::InternalError,
                step,
                Outcome::Unknown,
            )),
        }
    }

    async fn preflight_batch(&mut self, context: &SetupContext) -> Result<(), Box<SetupError>> {
        let (Some(batch), Some(tool)) = (&self.auth.batch, self.tools.requested) else {
            return Err(super::batch::invalid());
        };
        let login = batch.login;
        let server = batch.server_url.clone();
        let path = context.config_path.clone();
        tokio::task::spawn_blocking(move || {
            setup_config::preflight_server(&path, server.as_deref())
        })
        .await
        .map_err(|_| {
            Box::new(SetupError::new(
                ErrorCode::InternalError,
                Step::Validate,
                Outcome::NotApplied,
            ))
        })?
        .map_err(|error| Box::new(config_error(error)))?;
        // A saved executable can only be scoped after authentication and team
        // verification. If there is no saved file, keep the early PATH check.
        if matches!(
            std::fs::symlink_metadata(setup_tools::path(&context.config_path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ) {
            let report = self.tools.search.detect_one(tool, None, None).await;
            let Some(host) = report.selected.clone() else {
                let mut error = SetupError::new(
                    ErrorCode::ToolUnavailable,
                    Step::Validate,
                    Outcome::NotApplied,
                );
                error.tools = vec![report];
                return Err(Box::new(error));
            };
            self.preflight_mcp(context, Step::Validate, tool, Some(host.clone()))
                .await?;
            self.tools.batch_candidate = Some(host);
        }
        if login {
            self.auth.credentials =
                Some(self.input.request().await.map_err(|error| match error {
                    LoginInputError::Cancelled | LoginInputError::Unavailable => {
                        super::batch::missing(["password"])
                    }
                    LoginInputError::Invalid => super::batch::invalid(),
                    LoginInputError::ReadFailed => Box::new(SetupError::new(
                        ErrorCode::InternalError,
                        Step::Validate,
                        Outcome::NotApplied,
                    )),
                })?);
        }
        Ok(())
    }

    async fn preflight_mcp(
        &mut self,
        context: &SetupContext,
        step: Step,
        tool_type: ToolType,
        host: Option<SelectedTool>,
    ) -> Result<(), Box<SetupError>> {
        let project = self
            .project
            .prepared
            .as_ref()
            .map(|p| p.root.clone())
            .or_else(|| self.project.options.path.clone())
            .ok_or_else(|| {
                Box::new(SetupError::new(
                    ErrorCode::InternalError,
                    step,
                    Outcome::NotApplied,
                ))
            })?;
        let executable = self
            .broker
            .executable
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_exe)
            .map_err(|_| {
                Box::new(SetupError::new(
                    ErrorCode::ToolUnavailable,
                    step,
                    Outcome::NotApplied,
                ))
            })?;
        setup_mcp::preflight(
            setup_mcp::PreflightContext {
                config_path: context.config_path.clone(),
                project,
                tool_type,
                host_launcher: host.as_ref().map(|h| h.executable.command.clone()),
                mcp_automatic_supported: host.as_ref().is_some_and(|h| h.verified_host().is_ok()),
            },
            self.mcp.options.clone(),
            executable,
        )
        .await
        .map_err(|error| {
            let mut safe = mcp_error(step, error);
            self.mcp_identity(&mut safe);
            Box::new(safe)
        })
    }

    async fn configure_mcp(&mut self, context: &SetupContext) -> StepResult {
        if self.mcp.prepared.is_none() {
            let (
                Some(config),
                Some(user),
                Some(team),
                Some(project),
                Some(tool),
                Some(registration),
                Some(broker),
            ) = (
                &self.prepared,
                &self.auth.user,
                &self.teams.selected,
                &self.project.prepared,
                &self.tools.selected,
                &self.agents.registration,
                &self.broker.result,
            )
            else {
                return StepResult::Failed(SetupError::new(
                    ErrorCode::InternalError,
                    Step::ConfigureMcp,
                    Outcome::NotApplied,
                ));
            };
            let Ok(origin) = user_auth::origin(&config.config) else {
                return StepResult::Failed(SetupError::new(
                    ErrorCode::ConfigInvalid,
                    Step::ConfigureMcp,
                    Outcome::NotApplied,
                ));
            };
            let executable = self
                .broker
                .executable
                .clone()
                .map(Ok)
                .unwrap_or_else(std::env::current_exe);
            let Ok(executable) = executable else {
                return StepResult::Failed(SetupError::new(
                    ErrorCode::ToolUnavailable,
                    Step::ConfigureMcp,
                    Outcome::NotApplied,
                ));
            };
            let binding = setup_mcp::Context {
                config_path: context.config_path.clone(),
                origin: origin.to_string(),
                user_id: user.user_id,
                team_id: team.team_id,
                agent_id: registration.agent_id.unwrap_or(0),
                project: project.root.clone(),
                tool: tool.clone(),
                broker_pid: broker.health.pid,
                broker_instance: broker.health.instance_id.clone(),
            };
            match setup_mcp::prepare(binding.clone(), self.mcp.options.clone(), executable).await {
                Ok(prepared) => {
                    self.mcp.binding = Some(binding);
                    self.mcp.prepared = Some(prepared);
                }
                Err(error) => return self.mcp_failure(mcp_error(Step::ConfigureMcp, error)),
            }
        }
        let Some(prepared) = self.mcp.prepared.as_ref() else {
            return self.mcp_failure(SetupError::new(
                ErrorCode::InternalError,
                Step::ConfigureMcp,
                Outcome::NotApplied,
            ));
        };
        if prepared.requires_manual() {
            let mut error = SetupError::new(
                ErrorCode::ManualActionRequired,
                Step::ConfigureMcp,
                Outcome::NotApplied,
            );
            error.manual_config = Some(prepared.manual().clone());
            return self.mcp_failure(error);
        }
        let Some(plan) = prepared.plan() else {
            return self.mcp_failure(SetupError::new(
                ErrorCode::InternalError,
                Step::ConfigureMcp,
                Outcome::NotApplied,
            ));
        };
        if plan.change == mcp_config::Change::Conflict {
            let mut error = SetupError::new(
                ErrorCode::McpConfigConflict,
                Step::ConfigureMcp,
                Outcome::NotApplied,
            );
            error.mcp_plan = Some(plan.clone());
            return self.mcp_failure(error);
        }
        if plan.change != mcp_config::Change::Unchanged
            && !(self.mcp.options.apply || self.mcp.confirmed)
        {
            self.mcp.waiting = true;
            let mut error = SetupError::new(
                ErrorCode::InputRequired,
                Step::ConfigureMcp,
                Outcome::NotApplied,
            );
            error.required_inputs.push(RequiredInput {
                field: "mcp.apply",
                kind: InputKind::Value,
                secret: false,
                source: InputSource::Input,
            });
            error.mcp_plan = Some(plan.clone());
            self.mcp_identity(&mut error);
            return StepResult::InputRequired(error);
        }
        self.mcp.waiting = false;
        self.mcp.confirmed = false;
        self.mcp.options.apply = false;
        let Some(prepared) = self.mcp.prepared.take() else {
            return self.mcp_failure(SetupError::new(
                ErrorCode::InternalError,
                Step::ConfigureMcp,
                Outcome::Unknown,
            ));
        };
        let _write = LocalWrite::begin(&self.local_write);
        match prepared.apply().await {
            Ok(result) => {
                let reused = result.config.status == mcp_config::ApplyStatus::Unchanged;
                self.mcp.result = Some(result);
                StepResult::Complete {
                    outcome: if reused {
                        Outcome::NotApplied
                    } else {
                        Outcome::Applied
                    },
                    reused,
                }
            }
            Err(error) => self.mcp_failure(mcp_error(Step::ConfigureMcp, error)),
        }
    }
    async fn verify_connection(&mut self) -> StepResult {
        let executable = self
            .broker
            .executable
            .clone()
            .map(Ok)
            .unwrap_or_else(std::env::current_exe);
        let (Some(binding), Some(applied), Ok(executable)) =
            (&self.mcp.binding, &self.mcp.result, executable)
        else {
            return self.mcp_failure(SetupError::new(
                ErrorCode::InternalError,
                Step::VerifyConnection,
                Outcome::NotApplied,
            ));
        };
        match super::connection::verify(binding, &self.mcp.options, &executable).await {
            Ok(mut report) => {
                if applied.config.restart_required {
                    super::connection::merge_restart_actions(
                        &mut report.next_actions,
                        &applied.next_actions,
                    );
                }
                report.mcp_config = Some(applied.clone());
                self.mcp.connection = Some(report);
                StepResult::Complete {
                    outcome: Outcome::NotApplied,
                    reused: false,
                }
            }
            Err(mut error) => {
                error.mcp_config = Some(applied.clone());
                self.mcp_failure(*error)
            }
        }
    }

    fn mcp_identity(&self, error: &mut SetupError) {
        error.known.user_id = self.auth.user.as_ref().map(|u| u.user_id.to_string());
        error.known.team_id = self.teams.selected.as_ref().map(|t| t.team_id.to_string());
        error.known.agent_id = self
            .agents
            .registration
            .as_ref()
            .and_then(|r| r.agent_id)
            .map(|id| id.to_string());
        error.known.tool_type = self.tools.selected.as_ref().map(|t| t.tool_type);
    }
    fn mcp_failure(&self, mut error: SetupError) -> StepResult {
        self.mcp_identity(&mut error);
        StepResult::Failed(error)
    }
}
fn mcp_error(step: Step, error: mcp_config::ConfigError) -> SetupError {
    let code = match error.code {
        "SETUP_INPUT_INVALID" | "SETUP_PROJECT_INVALID" => ErrorCode::InputInvalid,
        "SETUP_TOOL_UNAVAILABLE" => ErrorCode::ToolUnavailable,
        "SETUP_UNSAFE_FILE" => ErrorCode::UnsafeFile,
        "SETUP_AUTH_BUSY" => ErrorCode::AuthBusy,
        "SETUP_CONFIG_BUSY" => ErrorCode::ConfigBusy,
        "SETUP_PROFILE_CONFLICT" => ErrorCode::ProfileConflict,
        "SETUP_BROKER_START_FAILED" => ErrorCode::BrokerStartFailed,
        "SETUP_MCP_CONFIG_CONFLICT" => ErrorCode::McpConfigConflict,
        "SETUP_MCP_CONFIG_WRITE_FAILED" => ErrorCode::McpConfigWriteFailed,
        "SETUP_MANUAL_ACTION_REQUIRED" => ErrorCode::ManualActionRequired,
        _ => ErrorCode::InternalError,
    };
    let mut safe = SetupError::new(
        code,
        step,
        match error.outcome {
            "applied" => Outcome::Applied,
            "not_applied" => Outcome::NotApplied,
            _ => Outcome::Unknown,
        },
    );
    if code == ErrorCode::ToolUnavailable {
        for field in ["mcp.nodePath", "mcp.entry"] {
            safe.required_inputs.push(RequiredInput {
                field,
                kind: InputKind::Value,
                secret: false,
                source: InputSource::Input,
            });
        }
    }
    if code != ErrorCode::InternalError {
        safe.mcp_error = Some(error);
    }
    safe
}

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_uncertain_errors_keep_backup_and_rollback_evidence() {
        for rollback in ["restored", "failed", "conflict", "not_needed"] {
            let backup = std::path::PathBuf::from("safe-private-backup");
            let provider = mcp_config::ConfigError {
                code: "SETUP_MCP_CONFIG_WRITE_FAILED",
                outcome: "unknown",
                rollback,
                backup_path: Some(backup.clone()),
                recovery_directory: None,
                conflict: None,
            };
            let public = mcp_error(Step::ConfigureMcp, provider);
            assert_eq!(public.code, ErrorCode::McpConfigWriteFailed);
            assert_eq!(public.outcome, Outcome::Unknown);
            let evidence = public.mcp_error.unwrap();
            assert_eq!(evidence.rollback, rollback);
            assert_eq!(evidence.backup_path, Some(backup));
        }
    }

    use super::*;

    struct BrokenInput;
    impl LoginInput for BrokenInput {
        fn available(&self) -> bool {
            true
        }
        async fn request(&mut self) -> Result<LoginCredentials, LoginInputError> {
            Err(LoginInputError::ReadFailed)
        }
    }
    impl TeamInput for BrokenInput {
        fn available(&self) -> bool {
            true
        }
        async fn request(&mut self, _: &[TeamChoice]) -> Result<u64, TeamInputError> {
            Err(TeamInputError::ReadFailed)
        }
    }

    #[test]
    fn only_an_unconfirmed_issuance_remains_pending_after_the_token_call() {
        let prepared = |verified, outcome| {
            Ok(tokens::TokenPreparation {
                agent_id: "201".into(),
                verified,
                reused: false,
                issued: verified,
                outcome,
                metadata: None,
            })
        };
        assert!(issuance_unconfirmed(&prepared(
            false,
            registration::Mutation::Unknown
        )));
        assert!(!issuance_unconfirmed(&prepared(
            true,
            registration::Mutation::Applied
        )));
        assert!(!issuance_unconfirmed(&prepared(
            false,
            registration::Mutation::NotApplied
        )));
        // A definite 4xx rejection already removed the old token (applied),
        // but the issuance itself is known to have failed.
        assert!(!issuance_unconfirmed(&Err(registration::Failure {
            kind: registration::FailureKind::Forbidden,
            outcome: registration::Mutation::Applied,
            agent_id: Some(201),
        })));

        let mut stages = SetupStages::default();
        stages.agents.issuance_pending = issuance_unconfirmed(&Err(registration::Failure {
            kind: registration::FailureKind::Forbidden,
            outcome: registration::Mutation::Applied,
            agent_id: Some(201),
        }));
        assert_eq!(
            stages.timed_out(Step::RegisterAgent).code,
            ErrorCode::Timeout
        );
    }

    #[test]
    fn registration_interrupt_distinguishes_reads_from_an_outstanding_post() {
        let mut stages = SetupStages::default();
        let before_creation = stages.timed_out(Step::RegisterAgent);
        assert_eq!(before_creation.code, ErrorCode::Timeout);
        assert_eq!(before_creation.outcome, Outcome::NotApplied);

        stages.agents.creation_pending = true;
        let pending = stages.timed_out(Step::RegisterAgent);
        assert_eq!(pending.code, ErrorCode::RegistrationUnknown);
        assert_eq!(pending.outcome, Outcome::Unknown);

        stages.agents.creation_pending = false;
        stages.agents.registration = Some(registration::Registration {
            state: RegistrationState::TokenRequired,
            agent_id: Some(201),
            created: true,
            reused: false,
            outcome: registration::Mutation::Applied,
            metadata: None,
        });
        let before_issuance = stages.timed_out(Step::RegisterAgent);
        assert_eq!(before_issuance.code, ErrorCode::Timeout);
        assert_eq!(before_issuance.outcome, Outcome::Applied);
        assert_eq!(before_issuance.known.agent_id.as_deref(), Some("201"));

        stages.agents.issuance_pending = true;
        let pending = stages.timed_out(Step::RegisterAgent);
        assert_eq!(pending.code, ErrorCode::TokenRequired);
        assert_eq!(pending.outcome, Outcome::Unknown);
        assert_eq!(pending.known.agent_id.as_deref(), Some("201"));
    }

    #[tokio::test]
    async fn input_io_failures_are_internal_errors_and_preserve_known_state() {
        let mut stages = SetupStages::new(BrokenInput).with_team_input(BrokenInput);
        for step in [Step::Authenticate, Step::SelectTeam] {
            let mut required = SetupError::new(ErrorCode::InputRequired, step, Outcome::NotApplied);
            required.known.user_id = Some("51".into());
            let error = stages.resolve_input(&required).await.unwrap_err();
            assert_eq!(error.code, ErrorCode::InternalError);
            assert_eq!(error.step, step);
            assert_eq!(error.outcome, Outcome::NotApplied);
            if step == Step::SelectTeam {
                assert_eq!(error.known.user_id.as_deref(), Some("51"));
            }
        }
    }
}
