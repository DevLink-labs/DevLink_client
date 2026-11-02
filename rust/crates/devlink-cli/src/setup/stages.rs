use super::{
    ErrorCode, ErrorLocation, InputKind, InputSource, LoginCredentials, LoginInput,
    LoginInputError, NoLoginInput, Outcome, RequiredInput, SetupContext, SetupError, Step,
    StepExecutor, StepResult,
};
use devlink_broker::{
    setup_config::{self, FailureKind, PreparedConfig},
    user_auth::{self, AuthFault, AuthFaultKind, SessionCheck, User},
};

/// Production setup stages. Each completed feature is connected here incrementally.
pub struct SetupStages<I = NoLoginInput> {
    prepared: Option<PreparedConfig>,
    input: I,
    credentials: Option<LoginCredentials>,
    user: Option<User>,
}

impl Default for SetupStages<NoLoginInput> {
    fn default() -> Self {
        Self::new(NoLoginInput)
    }
}

impl<I> SetupStages<I> {
    pub fn new(input: I) -> Self {
        Self {
            prepared: None,
            input,
            credentials: None,
            user: None,
        }
    }

    pub fn configuration(&self) -> Option<&PreparedConfig> {
        self.prepared.as_ref()
    }

    pub fn user(&self) -> Option<&User> {
        self.user.as_ref()
    }
}

impl<I: LoginInput> StepExecutor for SetupStages<I> {
    type Data = ();

    async fn execute(&mut self, step: Step, context: &SetupContext) -> StepResult {
        if step == Step::Authenticate {
            return self.authenticate(context).await;
        }
        if step != Step::Validate {
            let mut error = SetupError::new(ErrorCode::StepUnavailable, step, Outcome::NotApplied);
            if let Some(user) = &self.user {
                error.known.user_id = Some(user.user_id.to_string());
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
        self.input.available()
    }
    fn input_is_interactive(&self) -> bool {
        self.input.interactive()
    }

    async fn resolve_input(&mut self, required: &SetupError) -> Result<(), SetupError> {
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

impl<I: LoginInput> SetupStages<I> {
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
