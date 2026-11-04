use super::model::*;
use std::future::Future;
use uuid::Uuid;

/// Stage adapters retain their typed input/state. Terminal I/O belongs to the caller.
pub trait StepExecutor {
    type Data;

    fn execute(
        &mut self,
        step: Step,
        context: &SetupContext,
    ) -> impl Future<Output = StepResult> + Send;

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
}

/// A progress consumer has disconnected. Stop before performing another operation.
#[derive(Debug)]
pub struct ProgressUnavailable;

/// Execute stages and report progress through an injected, fallible observer.
/// No stdin/stdout, sleeps for user input, subprocesses, or implicit mutation retries.
pub async fn run<E: StepExecutor>(
    context: &SetupContext,
    mut executor: E,
    mut observe: impl FnMut(&ProgressEvent) -> Result<(), ProgressUnavailable>,
) -> RunResult<E::Data> {
    let run_id = Uuid::new_v4().to_string();
    let mut sequence = 0;
    let mut stages: Vec<_> = Step::ALL
        .iter()
        .map(|&step| Stage {
            step,
            status: StageStatus::Pending,
            outcome: Outcome::NotApplied,
            reused: false,
        })
        .collect();

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
            match executor.execute(step, context).await {
                StepResult::InputRequired(mut required) => {
                    required.step = step;
                    required.outcome = combine(stages[index].outcome, required.outcome);
                    stages[index].outcome = required.outcome;
                    if input_resolved
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
                    if let Err(mut error) = executor.resolve_input(&required).await {
                        error.step = step;
                        error.outcome = combine(stages[index].outcome, error.outcome);
                        stages[index].status = StageStatus::Failed;
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
        status: if stage.status == StageStatus::WaitingInput {
            RunStatus::WaitingInput
        } else {
            RunStatus::Running
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
