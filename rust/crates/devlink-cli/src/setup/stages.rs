use super::{
    ErrorCode, ErrorLocation, InputKind, InputSource, LoginCredentials, LoginInput,
    LoginInputError, NoLoginInput, NoTeamInput, NoToolInput, Outcome, RequiredInput, SetupContext,
    SetupError, Step, StepExecutor, StepResult, TeamChoice, TeamInput, TeamInputError, ToolInput,
    ToolInputError,
};
use devlink_broker::{
    broker_control,
    sessions::ToolType,
    setup_config::{self, FailureKind, PreparedConfig},
    setup_project::{self, PreparedProject, ProjectOptions},
    setup_tools::{self, SelectionBinding, SelectionSnapshot},
    teams::{self, SetupSelection, SetupTeamFault, SetupTeamFaultKind, Team},
    tool_discovery::{SelectedTool, ToolDetection, ToolSearch},
    user_auth::{self, AuthFault, AuthFaultKind, SessionCheck, User},
};

/// Production setup stages. Each completed feature is connected here incrementally.
pub struct SetupStages<I = NoLoginInput, T = NoTeamInput, U = NoToolInput> {
    prepared: Option<PreparedConfig>,
    input: I,
    credentials: Option<LoginCredentials>,
    user: Option<User>,
    team_input: T,
    requested_team: Option<u64>,
    team_choices: Vec<TeamChoice>,
    waiting_team: bool,
    team: Option<Team>,
    project_options: ProjectOptions,
    project: Option<PreparedProject>,
    tool_input: U,
    requested_tool: Option<ToolType>,
    tool_path: Option<std::path::PathBuf>,
    tool_search: ToolSearch,
    tool_reports: Vec<ToolDetection>,
    tool_snapshot: Option<SelectionSnapshot>,
    waiting_tool: bool,
    tool: Option<SelectedTool>,
    broker_executable: Option<std::path::PathBuf>,
    broker: Option<broker_control::StartResult>,
}

impl Default for SetupStages<NoLoginInput> {
    fn default() -> Self {
        Self::new(NoLoginInput)
    }
}

impl<I> SetupStages<I, NoTeamInput> {
    pub fn new(input: I) -> Self {
        Self {
            prepared: None,
            input,
            credentials: None,
            user: None,
            team_input: NoTeamInput,
            requested_team: None,
            team_choices: Vec::new(),
            waiting_team: false,
            team: None,
            project_options: ProjectOptions::default(),
            project: None,
            tool_input: NoToolInput,
            requested_tool: None,
            tool_path: None,
            tool_search: ToolSearch::default(),
            tool_reports: Vec::new(),
            tool_snapshot: None,
            waiting_tool: false,
            tool: None,
            broker_executable: None,
            broker: None,
        }
    }
}

impl<I, T, U> SetupStages<I, T, U> {
    pub fn with_team_input<V>(self, input: V) -> SetupStages<I, V, U> {
        SetupStages {
            prepared: self.prepared,
            input: self.input,
            credentials: self.credentials,
            user: self.user,
            team_input: input,
            requested_team: self.requested_team,
            team_choices: self.team_choices,
            waiting_team: self.waiting_team,
            team: self.team,
            project_options: self.project_options,
            project: self.project,
            tool_input: self.tool_input,
            requested_tool: self.requested_tool,
            tool_path: self.tool_path,
            tool_search: self.tool_search,
            tool_reports: self.tool_reports,
            tool_snapshot: self.tool_snapshot,
            waiting_tool: self.waiting_tool,
            tool: self.tool,
            broker_executable: self.broker_executable,
            broker: self.broker,
        }
    }

    pub fn with_tool_input<V>(self, input: V) -> SetupStages<I, T, V> {
        SetupStages {
            prepared: self.prepared,
            input: self.input,
            credentials: self.credentials,
            user: self.user,
            team_input: self.team_input,
            requested_team: self.requested_team,
            team_choices: self.team_choices,
            waiting_team: self.waiting_team,
            team: self.team,
            project_options: self.project_options,
            project: self.project,
            tool_input: input,
            requested_tool: self.requested_tool,
            tool_path: self.tool_path,
            tool_search: self.tool_search,
            tool_reports: self.tool_reports,
            tool_snapshot: self.tool_snapshot,
            waiting_tool: self.waiting_tool,
            tool: self.tool,
            broker_executable: self.broker_executable,
            broker: self.broker,
        }
    }
    pub fn with_tool(mut self, tool: Option<ToolType>, path: Option<std::path::PathBuf>) -> Self {
        self.requested_tool = tool;
        self.tool_path = path;
        self
    }
    pub fn with_tool_search(mut self, search: ToolSearch) -> Self {
        self.tool_search = search;
        self
    }
    pub fn tool(&self) -> Option<&SelectedTool> {
        self.tool.as_ref()
    }

    /// Non-CLI callers must supply the actual devlink executable, not their host process.
    pub fn with_broker_executable(mut self, executable: std::path::PathBuf) -> Self {
        self.broker_executable = Some(executable);
        self
    }

    pub fn broker(&self) -> Option<&broker_control::StartResult> {
        self.broker.as_ref()
    }

    pub fn with_team_id(mut self, id: Option<u64>) -> Self {
        self.requested_team = id;
        self
    }

    pub fn with_project(mut self, options: ProjectOptions) -> Self {
        self.project_options = options;
        self
    }

    pub fn project(&self) -> Option<&PreparedProject> {
        self.project.as_ref()
    }

    pub fn team(&self) -> Option<&Team> {
        self.team.as_ref()
    }

    pub fn configuration(&self) -> Option<&PreparedConfig> {
        self.prepared.as_ref()
    }

    pub fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }
}

impl<I: LoginInput, T: TeamInput, U: ToolInput> StepExecutor for SetupStages<I, T, U> {
    type Data = ();

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
        if step == Step::StartBroker {
            let executable = self
                .broker_executable
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
                    self.broker = Some(result);
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
                    safe.known.user_id = self.user.as_ref().map(|u| u.user_id.to_string());
                    safe.known.team_id = self.team.as_ref().map(|t| t.team_id.to_string());
                    safe.known.tool_type = self.tool.as_ref().map(|t| t.tool_type);
                    StepResult::Failed(safe)
                }
            };
        }
        if step == Step::BindProject {
            let path = context.config_path.clone();
            let options = self.project_options.clone();
            return match tokio::task::spawn_blocking(move || {
                setup_project::prepare(&path, &options)
            })
            .await
            {
                Ok(Ok(project)) => {
                    let reused = project.reused;
                    self.project = Some(project);
                    StepResult::Complete {
                        outcome: Outcome::NotApplied,
                        reused,
                    }
                }
                Ok(Err(failure)) => {
                    let mut error = project_error(step, failure);
                    error.known.user_id = self.user.as_ref().map(|u| u.user_id.to_string());
                    error.known.team_id = self.team.as_ref().map(|t| t.team_id.to_string());
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
            if let Some(user) = &self.user {
                error.known.user_id = Some(user.user_id.to_string());
            }
            if let Some(team) = &self.team {
                error.known.team_id = Some(team.team_id.to_string());
            }
            error.tools = self.tool_reports.clone();
            error.known.tool_type = self.tool.as_ref().map(|s| s.tool_type);
            return StepResult::Failed(error);
        }
        // Reject invalid local input before initializing settings or contacting central.
        let options = self.project_options.clone();
        match tokio::task::spawn_blocking(move || setup_project::resolve(&options)).await {
            Ok(Ok(project)) => {
                // Freeze cwd now; later stages must not resolve a relative path again.
                self.project_options.path = Some(project.root.clone());
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
        let path = context.config_path.clone();
        match tokio::task::spawn_blocking(move || setup_config::prepare(&path)).await {
            Ok(Ok(prepared)) => {
                let created = prepared.created;
                self.prepared = Some(prepared);
                StepResult::Complete {
                    outcome: if created {
                        Outcome::Applied
                    } else {
                        Outcome::NotApplied
                    },
                    reused: !created,
                }
            }
            Ok(Err(failure)) => {
                let code = match failure.kind {
                    FailureKind::InvalidPath => ErrorCode::ConfigPathInvalid,
                    FailureKind::InvalidConfig => ErrorCode::ConfigInvalid,
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
                let mut error = SetupError::new(code, step, outcome);
                error.location = Some(ErrorLocation {
                    file: failure.file,
                    field: failure.field,
                });
                StepResult::Failed(error)
            }
            Err(_) => StepResult::Failed(SetupError::new(
                ErrorCode::InternalError,
                step,
                Outcome::Unknown,
            )),
        }
    }

    fn can_resolve_input(&self) -> bool {
        if self.waiting_tool {
            self.tool_input.available()
        } else if self.waiting_team {
            self.team_input.available()
        } else {
            self.input.available()
        }
    }
    fn input_is_interactive(&self) -> bool {
        if self.waiting_tool {
            self.tool_input.interactive()
        } else if self.waiting_team {
            self.team_input.interactive()
        } else {
            self.input.interactive()
        }
    }

    async fn resolve_input(&mut self, required: &SetupError) -> Result<(), SetupError> {
        if required.step == Step::SelectTool {
            let code = match self.tool_input.request(&self.tool_reports).await {
                Ok(tool)
                    if self
                        .tool_reports
                        .iter()
                        .any(|r| r.tool_type == tool && r.selected.is_some()) =>
                {
                    self.requested_tool = Some(tool);
                    return Ok(());
                }
                Ok(_) | Err(ToolInputError::Invalid) => ErrorCode::InputInvalid,
                Err(ToolInputError::Unavailable) => return Err(required.clone()),
                Err(ToolInputError::Cancelled) => ErrorCode::Cancelled,
                Err(ToolInputError::ReadFailed) => ErrorCode::InternalError,
            };
            let mut error = SetupError::new(code, Step::SelectTool, Outcome::NotApplied);
            error.known = required.known.clone();
            error.tools = self.tool_reports.clone();
            return Err(error);
        }
        if required.step == Step::SelectTeam {
            let code = match self.team_input.request(&self.team_choices).await {
                Ok(id) => {
                    self.requested_team = Some(id);
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
                self.credentials = Some(credentials);
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

    fn finish(self) {}
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

impl<I: LoginInput, T: TeamInput, U: ToolInput> SetupStages<I, T, U> {
    async fn select_tool(&mut self, context: &SetupContext) -> StepResult {
        let (Some(config), Some(user), Some(team), Some(project)) =
            (&self.prepared, &self.user, &self.team, &self.project)
        else {
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
        if self.waiting_tool && self.tool_snapshot.as_ref() != Some(&snapshot) {
            return self.tool_failure(SetupError::new(
                ErrorCode::ToolSelectionConflict,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        }
        if snapshot.bound_tool.is_some_and(|bound| {
            self.requested_tool
                .is_some_and(|requested| requested != bound)
        }) {
            return self.tool_failure(SetupError::new(
                ErrorCode::ToolBindingConflict,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        }
        let preferred = snapshot.preferred();
        let requested = self
            .requested_tool
            .or(snapshot.bound_tool)
            .or_else(|| preferred.map(|s| s.tool_type));
        self.tool_reports = self
            .tool_search
            .detect(preferred, requested.zip(self.tool_path.as_deref()))
            .await;
        let mut selected = requested.and_then(|tool| {
            self.tool_reports
                .iter()
                .find(|r| r.tool_type == tool)
                .and_then(|r| r.selected.clone())
        });
        if selected.is_none() && (self.requested_tool.is_some() || snapshot.bound_tool.is_some()) {
            return self.tool_failure(SetupError::new(
                ErrorCode::ToolUnavailable,
                Step::SelectTool,
                Outcome::NotApplied,
            ));
        }
        if selected.is_none() {
            let available: Vec<_> = self
                .tool_reports
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
                    self.tool_snapshot = Some(snapshot);
                    self.waiting_tool = true;
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
        let path = context.config_path.clone();
        let publish = selected.clone();
        let reuse_bound = snapshot.bound_tool == Some(selected.tool_type);
        match tokio::task::spawn_blocking(move || setup_tools::publish(&path, &snapshot, &publish))
            .await
        {
            Ok(Ok(reused)) => {
                self.tool = Some(selected);
                self.waiting_tool = false;
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
        error.known.user_id = self.user.as_ref().map(|u| u.user_id.to_string());
        error.known.team_id = self.team.as_ref().map(|t| t.team_id.to_string());
        error.tools = self.tool_reports.clone();
        error
    }

    async fn select_team(&mut self, context: &SetupContext) -> StepResult {
        let (Some(prepared), Some(user)) = (&self.prepared, &self.user) else {
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
            self.requested_team,
        )
        .await;
        let mut error = match result {
            Ok(SetupSelection::Selected { team, reused }) => {
                self.team = Some(team);
                self.waiting_team = false;
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
                self.team_choices = teams
                    .into_iter()
                    .map(|team| TeamChoice {
                        id: team.team_id.to_string(),
                        name: team.name,
                    })
                    .collect();
                self.waiting_team = true;
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
                required.team_choices = self.team_choices.clone();
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
        let checked =
            user_auth::check_session(&context.config_path, expected_origin.as_str()).await;
        match checked {
            Ok(SessionCheck::Authenticated(user)) => {
                // Another process may have signed in while input was being collected.
                self.credentials = None;
                self.user = Some(user);
                StepResult::Complete {
                    outcome: Outcome::Applied,
                    reused: true,
                }
            }
            Ok(SessionCheck::LoginRequired { expired_now }) => {
                let Some(credentials) = self.credentials.take() else {
                    let mut error = SetupError::new(
                        ErrorCode::AuthRequired,
                        Step::Authenticate,
                        if expired_now {
                            Outcome::Applied
                        } else {
                            Outcome::NotApplied
                        },
                    );
                    error.required_inputs = vec![
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
                    ];
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
                        self.user = Some(login.user);
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

#[cfg(test)]
mod tests {
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
