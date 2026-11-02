use super::{
    ErrorCode, ErrorLocation, Outcome, SetupContext, SetupError, Step, StepExecutor, StepResult,
};
use devlink_broker::setup_config::{self, FailureKind, PreparedConfig};

/// Production setup stages. Each completed feature is connected here incrementally.
#[derive(Default)]
pub struct SetupStages {
    prepared: Option<PreparedConfig>,
}

impl SetupStages {
    pub fn configuration(&self) -> Option<&PreparedConfig> {
        self.prepared.as_ref()
    }
}

impl StepExecutor for SetupStages {
    type Data = ();

    async fn execute(&mut self, step: Step, context: &SetupContext) -> StepResult {
        if step != Step::Validate {
            return StepResult::Failed(SetupError::new(
                ErrorCode::StepUnavailable,
                step,
                Outcome::NotApplied,
            ));
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

    fn finish(self) {}
}
