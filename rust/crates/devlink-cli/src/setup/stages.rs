use super::{
    ErrorCode, ErrorLocation, InputKind, InputSource, LoginCredentials, LoginInput,
    LoginInputError, NoLoginInput, NoTeamInput, Outcome, RequiredInput, SetupContext, SetupError,
    Step, StepExecutor, StepResult, TeamChoice, TeamInput, TeamInputError,
};
use devlink_broker::{
    setup_config::{self, FailureKind, PreparedConfig},
    teams::{self, SetupSelection, SetupTeamFault, SetupTeamFaultKind, Team},
    user_auth::{self, AuthFault, AuthFaultKind, SessionCheck, User},
};

/// Production setup stages. Each completed feature is connected here incrementally.
pub struct SetupStages<I = NoLoginInput, T = NoTeamInput> {
    prepared: Option<PreparedConfig>,
    input: I,
    credentials: Option<LoginCredentials>,
    user: Option<User>,
    team_input: T,
    requested_team: Option<u64>,
    team_choices: Vec<TeamChoice>,
    waiting_team: bool,
    team: Option<Team>,
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
        }
    }
}

impl<I, T> SetupStages<I, T> {
    pub fn with_team_input<U>(self, input: U) -> SetupStages<I, U> {
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
        }
    }

    pub fn with_team_id(mut self, id: Option<u64>) -> Self {
        self.requested_team = id;
        self
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

impl<I: LoginInput, T: TeamInput> StepExecutor for SetupStages<I, T> {
    type Data = ();

    async fn execute(&mut self, step: Step, context: &SetupContext) -> StepResult {
        if step == Step::Authenticate {
            return self.authenticate(context).await;
        }
        if step == Step::SelectTeam {
            return self.select_team(context).await;
        }
        if step != Step::Validate {
            let mut error = SetupError::new(ErrorCode::StepUnavailable, step, Outcome::NotApplied);
            if let Some(user) = &self.user {
                error.known.user_id = Some(user.user_id.to_string());
            }
            if let Some(team) = &self.team {
                error.known.team_id = Some(team.team_id.to_string());
            }
            return StepResult::Failed(error);
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
        if self.waiting_team {
            self.team_input.available()
        } else {
            self.input.available()
        }
    }
    fn input_is_interactive(&self) -> bool {
        if self.waiting_team {
            self.team_input.interactive()
        } else {
            self.input.interactive()
        }
    }

    async fn resolve_input(&mut self, required: &SetupError) -> Result<(), SetupError> {
        if required.step == Step::SelectTeam {
            return match self.team_input.request(&self.team_choices).await {
                Ok(id) => {
                    self.requested_team = Some(id);
                    Ok(())
                }
                Err(TeamInputError::Unavailable) => Err(required.clone()),
                Err(error) => {
                    let mut safe = SetupError::new(
                        if matches!(error, TeamInputError::Cancelled) {
                            ErrorCode::Cancelled
                        } else {
                            ErrorCode::InputInvalid
                        },
                        Step::SelectTeam,
                        Outcome::NotApplied,
                    );
                    safe.known = required.known.clone();
                    Err(safe)
                }
            };
        }
        match self.input.request().await {
            Ok(credentials) => {
                self.credentials = Some(credentials);
                Ok(())
            }
            Err(LoginInputError::Unavailable) => Err(required.clone()),
            Err(failure) => Err(SetupError::new(
                match failure {
                    LoginInputError::Invalid => ErrorCode::InputInvalid,
                    LoginInputError::Cancelled => ErrorCode::Cancelled,
                    _ => ErrorCode::InputInvalid,
                },
                Step::Authenticate,
                Outcome::NotApplied,
            )),
        }
    }

    fn finish(self) {}
}

impl<I: LoginInput, T: TeamInput> SetupStages<I, T> {
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
    let code = match error.downcast_ref::<AuthFault>().map(|fault| fault.kind) {
        Some(AuthFaultKind::Unavailable) => ErrorCode::CentralUnavailable,
        Some(AuthFaultKind::Protocol | AuthFaultKind::IdentityMismatch) => ErrorCode::ProtocolError,
        Some(AuthFaultKind::Busy) => ErrorCode::AuthBusy,
        Some(AuthFaultKind::SessionConflict) => ErrorCode::AuthSessionConflict,
        None => match status {
            Some(408 | 429 | 500..=599) => ErrorCode::CentralUnavailable,
            Some(400..=499) => ErrorCode::AuthRejected,
            Some(_) => ErrorCode::ProtocolError,
            None => ErrorCode::AuthStorageFailed,
        },
    };
    let mut safe = local_session_error(path, code);
    if code == ErrorCode::AuthBusy {
        safe.location = Some(ErrorLocation {
            file: devlink_broker::config::state_directory(path).join("account.lock"),
            field: None,
        });
    }
    safe.outcome = if code == ErrorCode::AuthStorageFailed
        || (login_attempted
            && matches!(
                code,
                ErrorCode::CentralUnavailable | ErrorCode::ProtocolError
            )) {
        Outcome::Unknown
    } else if matches!(
        error.downcast_ref::<AuthFault>().map(|fault| fault.kind),
        Some(AuthFaultKind::IdentityMismatch)
    ) {
        Outcome::Applied
    } else {
        Outcome::NotApplied
    };
    if user_auth::session_was_invalidated(error) && safe.outcome != Outcome::Unknown {
        safe.outcome = Outcome::Applied;
    }
    safe
}
