use super::{
    control::{Attempt, RunControl},
    model::*,
};
use std::{
    future::Future,
    sync::{Arc, atomic::AtomicBool},
};
use uuid::Uuid;

/// Stage adapters retain their typed input/state. Terminal I/O belongs to the caller.
pub trait StepExecutor {
    type Data;

    fn execute(
        &mut self,
        step: Step,
        context: &SetupContext,
    ) -> impl Future<Output = StepResult> + Send;

    /// Validate a full run and acquire any resources needed for its lifetime.
    fn validate_run(&mut self, context: &SetupContext) -> impl Future<Output = StepResult> + Send {
        self.execute(Step::Validate, context)
    }

    fn can_resolve_input(&self) -> bool {
        false
    }
    fn input_is_interactive(&self) -> bool {
        false
    }
    fn resolve_input(
        &mut self,
        required: &SetupError,
    ) -> impl Future<Output = Result<(), SetupError>> + Send {
        let error = required.clone();
        async { Err(error) }
    }

    /// Called only after every stage, including connection verification, succeeds.
    fn finish(self) -> Self::Data;

    fn timed_out(&self, step: Step) -> SetupError {
        SetupError::new(ErrorCode::Timeout, step, Outcome::Unknown)
    }

    /// True only while a submitted local write must be allowed to settle.
    fn local_write(&self) -> Option<Arc<AtomicBool>> {
        None
    }
}

/// A progress consumer has disconnected. Stop before performing another operation.
#[derive(Debug)]
pub struct ProgressUnavailable;

/// Execute stages and report progress through an injected, fallible observer.
/// No stdin/stdout, sleeps for user input, subprocesses, or implicit mutation retries.
pub async fn run<E: StepExecutor>(
    context: &SetupContext,
    executor: E,
    observe: impl FnMut(&ProgressEvent) -> Result<(), ProgressUnavailable>,
) -> RunResult<E::Data> {
    run_controlled(context, executor, observe, &mut RunControl::default()).await
}

fn initial_stages() -> Vec<Stage> {
    Step::ALL
        .iter()
        .map(|&step| Stage {
            step,
            status: StageStatus::Pending,
            outcome: Outcome::NotApplied,
            reused: false,
        })
        .collect()
}

pub(super) fn reject<D>(
    error: SetupError,
    mut observe: impl FnMut(&ProgressEvent) -> Result<(), ProgressUnavailable>,
) -> RunResult<D> {
    let run_id = Uuid::new_v4().to_string();
    let mut sequence = 0;
    let mut stages = initial_stages();
    let validate = &mut stages[0];
    validate.status = StageStatus::Running;
    let _ = emit(&run_id, &mut sequence, validate, &mut observe);
    validate.status = if error.code == ErrorCode::Cancelled {
        StageStatus::Cancelled
    } else {
        StageStatus::Failed
    };
    let _ = emit(&run_id, &mut sequence, validate, &mut observe);
    failure(run_id, sequence, stages, 0, error)
}

pub(super) async fn run_controlled<E: StepExecutor>(
    context: &SetupContext,
    mut executor: E,
    mut observe: impl FnMut(&ProgressEvent) -> Result<(), ProgressUnavailable>,
    control: &mut RunControl,
) -> RunResult<E::Data> {
    let run_id = Uuid::new_v4().to_string();
    let mut sequence = 0;
    let mut stages = initial_stages();

    for index in 0..stages.len() {
        let step = stages[index].step;
        stages[index].status = StageStatus::Running;
        if emit(&run_id, &mut sequence, &stages[index], &mut observe).is_err() {
            return failure(
                run_id,
                sequence,
                stages,
                index,
                SetupError::new(ErrorCode::InternalError, step, Outcome::NotApplied),
            );
        }

        let mut input_resolved = false;
        let error = loop {
            let local_write = executor.local_write();
            let result = match control
                .attempt(
                    async {
                        if step == Step::Validate {
                            executor.validate_run(context).await
                        } else {
                            executor.execute(step, context).await
                        }
                    },
                    local_write.as_deref(),
                )
                .await
            {
                Attempt::Done(result) => result,
                Attempt::Interrupted {
                    reason,
                    started,
                    completed,
                } => {
                    let mut error = executor.timed_out(step);
                    match completed {
                        Some(StepResult::Complete { outcome, reused }) => {
                            stages[index].reused = reused;
                            error.outcome = outcome;
                        }
                        Some(StepResult::Failed(settled) | StepResult::InputRequired(settled)) => {
                            error = settled;
                        }
                        None if !started => error.outcome = Outcome::NotApplied,
                        None => {}
                    }
                    StepResult::Failed(error.interrupted(reason, started))
                }
            };
            match result {
                StepResult::InputRequired(mut required) => {
                    required.step = step;
                    required.outcome = combine(stages[index].outcome, required.outcome);
                    stages[index].outcome = required.outcome;
                    let explicit_agent_recovery = required.step == Step::RegisterAgent
                        && matches!(
                            required.code,
                            ErrorCode::AgentAlreadyExists | ErrorCode::TokenRequired
                        )
                        && executor.input_is_interactive();
                    if (input_resolved && !explicit_agent_recovery)
                        || required.outcome == Outcome::Unknown
                        || !executor.can_resolve_input()
                    {
                        stages[index].status = StageStatus::Failed;
                        break Some(required);
                    }
                    if executor.input_is_interactive() {
                        stages[index].status = StageStatus::WaitingInput;
                        if emit_with_inputs(
                            &run_id,
                            &mut sequence,
                            &stages[index],
                            Some(&required),
                            &mut observe,
                        )
                        .is_err()
                        {
                            break Some(SetupError::new(
                                ErrorCode::InternalError,
                                step,
                                stages[index].outcome,
                            ));
                        }
                    }
                    let resolved = match control
                        .attempt(async { executor.resolve_input(&required).await }, None)
                        .await
                    {
                        Attempt::Done(result) => result,
                        Attempt::Interrupted { reason, .. } => {
                            let mut error = executor.timed_out(step);
                            error.outcome = Outcome::NotApplied;
                            Err(error.interrupted(reason, false))
                        }
                    };
                    if let Err(mut error) = resolved {
                        error.step = step;
                        error.outcome = combine(stages[index].outcome, error.outcome);
                        stages[index].outcome = error.outcome;
                        break Some(error);
                    }
                    input_resolved = true;
                    stages[index].status = StageStatus::Running;
                    if executor.input_is_interactive()
                        && emit(&run_id, &mut sequence, &stages[index], &mut observe).is_err()
                    {
                        break Some(SetupError::new(
                            ErrorCode::InternalError,
                            step,
                            stages[index].outcome,
                        ));
                    }
                    // Resume only this stage. Network failures and mutations are not retried.
                    continue;
                }
                StepResult::Complete {
                    outcome: Outcome::Unknown,
                    ..
                } => {
                    // A contradictory adapter result cannot authorize the next mutation.
                    stages[index].status = StageStatus::Failed;
                    stages[index].outcome = Outcome::Unknown;
                    break Some(SetupError::new(
                        ErrorCode::InternalError,
                        step,
                        Outcome::Unknown,
                    ));
                }
                StepResult::Complete { outcome, reused } => {
                    stages[index].status = StageStatus::Succeeded;
                    stages[index].outcome = combine(stages[index].outcome, outcome);
                    stages[index].reused = reused;
                    break None;
                }
                StepResult::Failed(mut error) => {
                    // The runner's current step is authoritative, even for a faulty adapter.
                    error.step = step;
                    error.outcome = combine(stages[index].outcome, error.outcome);
                    stages[index].status = StageStatus::Failed;
                    stages[index].outcome = error.outcome;
                    break Some(error);
                }
            }
        };
        if let Some(error) = &error {
            stages[index].status = if error.code == ErrorCode::Cancelled {
                StageStatus::Cancelled
            } else {
                StageStatus::Failed
            };
            stages[index].outcome = error.outcome;
        }
        let observed = emit(&run_id, &mut sequence, &stages[index], &mut observe);
        // Preserve the original domain error/known identity even if reporting failed.
        if let Some(error) = error {
            return failure(run_id, sequence, stages, index, error);
        }
        if observed.is_err() {
            let outcome = stages[index].outcome;
            return failure(
                run_id,
                sequence,
                stages,
                index,
                SetupError::new(ErrorCode::InternalError, step, outcome),
            );
        }
    }

    RunResult {
        schema_version: 1,
        event_type: "result",
        run_id,
        sequence: sequence + 1,
        status: RunStatus::Succeeded,
        exit_code: 0,
        stages,
        completion: Completion::Success {
            data: executor.finish(),
        },
    }
}

fn emit(
    run_id: &str,
    sequence: &mut u64,
    stage: &Stage,
    observe: &mut impl FnMut(&ProgressEvent) -> Result<(), ProgressUnavailable>,
) -> Result<(), ProgressUnavailable> {
    emit_with_inputs(run_id, sequence, stage, None, observe)
}

fn emit_with_inputs(
    run_id: &str,
    sequence: &mut u64,
    stage: &Stage,
    inputs: Option<&SetupError>,
    observe: &mut impl FnMut(&ProgressEvent) -> Result<(), ProgressUnavailable>,
) -> Result<(), ProgressUnavailable> {
    *sequence += 1;
    observe(&ProgressEvent {
        schema_version: 1,
        event_type: "progress",
        run_id: run_id.to_owned(),
        sequence: *sequence,
        status: match stage.status {
            StageStatus::WaitingInput => RunStatus::WaitingInput,
            StageStatus::Cancelled => RunStatus::Cancelled,
            _ => RunStatus::Running,
        },
        step: stage.step,
        step_status: stage.status,
        outcome: stage.outcome,
        required_inputs: inputs
            .map(|error| error.required_inputs.clone())
            .unwrap_or_default(),
        team_choices: inputs
            .map(|error| error.team_choices.clone())
            .unwrap_or_default(),
        agent_choices: inputs
            .map(|error| error.agent_choices.clone())
            .unwrap_or_default(),
        tools: inputs.map(|error| error.tools.clone()).unwrap_or_default(),
        mcp_plan: inputs.and_then(|error| error.mcp_plan.clone()),
    })
}

fn failure<D>(
    run_id: String,
    sequence: u64,
    mut stages: Vec<Stage>,
    index: usize,
    error: SetupError,
) -> RunResult<D> {
    stages[index].status = if error.code == ErrorCode::Cancelled {
        StageStatus::Cancelled
    } else {
        StageStatus::Failed
    };
    stages[index].outcome = error.outcome;
    for stage in &mut stages[index + 1..] {
        stage.status = StageStatus::Skipped;
    }
    let exit_code = error.code.exit_code();
    RunResult {
        schema_version: 1,
        event_type: "result",
        run_id,
        sequence: sequence + 1,
        status: if exit_code == 130 {
            RunStatus::Cancelled
        } else if exit_code == 3 {
            RunStatus::ActionRequired
        } else {
            RunStatus::Failed
        },
        exit_code,
        stages,
        completion: Completion::Failure {
            error: Box::new(error),
        },
    }
}

fn combine(previous: Outcome, current: Outcome) -> Outcome {
    if previous == Outcome::Unknown || current == Outcome::Unknown {
        Outcome::Unknown
    } else if previous == Outcome::Applied || current == Outcome::Applied {
        Outcome::Applied
    } else {
        Outcome::NotApplied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, future::pending, time::Duration};

    struct Stalled<'a>(&'a Cell<usize>, Option<tokio::sync::oneshot::Sender<()>>);

    impl StepExecutor for Stalled<'_> {
        type Data = ();

        fn execute(
            &mut self,
            step: Step,
            _: &SetupContext,
        ) -> impl Future<Output = StepResult> + Send {
            self.0.set(self.0.get() + 1);
            let signal = if step == Step::RegisterAgent {
                self.1.take()
            } else {
                None
            };
            async move {
                if step == Step::RegisterAgent {
                    if let Some(signal) = signal {
                        signal.send(()).unwrap();
                    }
                    pending().await
                } else {
                    StepResult::Complete {
                        outcome: Outcome::Applied,
                        reused: false,
                    }
                }
            }
        }

        fn timed_out(&self, step: Step) -> SetupError {
            let mut error = SetupError::new(ErrorCode::TokenRequired, step, Outcome::Unknown);
            error.known.agent_id = Some("201".into());
            error
        }

        fn finish(self) {}
    }

    #[tokio::test]
    async fn deadline_preserves_the_known_agent_and_stops_following_stages() {
        let calls = Cell::new(0);
        let context = SetupContext {
            config_path: "unused".into(),
        };
        let mut control = RunControl::default();
        control.deadline = Some(tokio::time::Instant::now() + Duration::from_millis(100));
        let result =
            run_controlled(&context, Stalled(&calls, None), |_| Ok(()), &mut control).await;
        let Completion::Failure { error } = result.completion else {
            panic!("expected deadline failure")
        };
        assert_eq!(error.code, ErrorCode::TokenRequired);
        assert_eq!(error.known.agent_id.as_deref(), Some("201"));
        assert_eq!(result.stages[0].outcome, Outcome::Applied);
        assert_eq!(result.stages[5].outcome, Outcome::Unknown);
        assert_eq!(result.stages[6].status, StageStatus::Skipped);
        assert_eq!(calls.get(), 6);
    }

    #[tokio::test]
    async fn cancellation_during_issuance_preserves_the_known_agent_and_recovery() {
        let calls = Cell::new(0);
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut control = RunControl::new(async move {
            receive.await.unwrap();
            ErrorCode::Cancelled
        });
        let result = run_controlled(
            &SetupContext {
                config_path: "unused".into(),
            },
            Stalled(&calls, Some(send)),
            |_| Ok(()),
            &mut control,
        )
        .await;
        let Completion::Failure { error } = result.completion else {
            panic!("expected cancellation")
        };
        assert_eq!(result.status, RunStatus::Cancelled);
        assert_eq!(result.exit_code, 130);
        assert_eq!(error.code, ErrorCode::Cancelled);
        assert_eq!(error.known.agent_id.as_deref(), Some("201"));
        assert_eq!(error.recovery_action(), "explicit_issue_token");
        assert_eq!(result.stages[0].outcome, Outcome::Applied);
        assert_eq!(result.stages[5].status, StageStatus::Cancelled);
        assert_eq!(result.stages[5].outcome, Outcome::Unknown);
        assert_eq!(result.stages[6].status, StageStatus::Skipped);
        assert_eq!(calls.get(), 6);
    }
}
