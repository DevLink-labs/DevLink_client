use devlink_cli::setup::{
    Completion, ErrorCode, InputKind, InputSource, Outcome, ProgressUnavailable, RequiredInput,
    RunStatus, SetupContext, SetupError, StageStatus, Step, StepExecutor, StepResult, run,
};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Executor {
    calls: Arc<Mutex<Vec<Step>>>,
    expected_path: PathBuf,
    failure: Option<(Step, SetupError)>,
    unknown_at: Option<Step>,
}

impl StepExecutor for Executor {
    type Data = Vec<Step>;

    async fn execute(&mut self, step: Step, context: &SetupContext) -> StepResult {
        assert_eq!(context.config_path, self.expected_path);
        self.calls.lock().unwrap().push(step);
        if let Some((at, error)) = &self.failure
            && *at == step
        {
            return StepResult::Failed(error.clone());
        }
        StepResult::Complete {
            outcome: if self.unknown_at == Some(step) {
                Outcome::Unknown
            } else if step == Step::SelectTeam {
                Outcome::Applied
            } else {
                Outcome::NotApplied
            },
            reused: step == Step::Authenticate,
        }
    }

    fn finish(self) -> Self::Data {
        self.calls.lock().unwrap().clone()
    }
}

fn context() -> SetupContext {
    SetupContext {
        config_path: std::env::current_dir()
            .unwrap()
            .join("프로필 path/config.json"),
    }
}

fn executor(context: &SetupContext) -> Executor {
    Executor {
        calls: Arc::default(),
        expected_path: context.config_path.clone(),
        failure: None,
        unknown_at: None,
    }
}

#[tokio::test]
async fn completes_in_order_with_external_progress_and_the_selected_config() {
    let context = context();
    let mut events = Vec::new();
    let result = run(&context, executor(&context), |event| {
        events.push(event.clone());
        Ok(())
    })
    .await;
    assert_eq!(result.status, RunStatus::Succeeded);
    assert_eq!(result.exit_code, 0);
    assert_eq!(events.len(), 18);
    assert_eq!(result.sequence, 19);
    uuid::Uuid::parse_str(&result.run_id).unwrap();
    for (index, pair) in events.as_chunks::<2>().0.iter().enumerate() {
        assert_eq!(pair[0].step, Step::ALL[index]);
        assert_eq!(pair[0].step_status, StageStatus::Running);
        assert_eq!(pair[1].step_status, StageStatus::Succeeded);
        assert_eq!(pair[0].sequence, index as u64 * 2 + 1);
        assert_eq!(pair[1].sequence, index as u64 * 2 + 2);
        assert_eq!(pair[0].run_id, result.run_id);
        assert_eq!(pair[1].run_id, result.run_id);
    }
    assert!(result.stages[1].reused);
    assert_eq!(result.stages[2].outcome, Outcome::Applied);
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["type"], "result");
    assert_eq!(json["data"], serde_json::to_value(Step::ALL).unwrap());
    assert!(json.get("error").is_none());
}

#[tokio::test]
async fn uncertain_registration_stops_once_and_preserves_prior_changes_and_known_ids() {
    let context = context();
    let mut executor = executor(&context);
    let calls = executor.calls.clone();
    // The adapter's incorrect step must not corrupt the runner's stage attribution.
    let mut error = SetupError::new(
        ErrorCode::RegistrationUnknown,
        Step::Validate,
        Outcome::Unknown,
    );
    error.known.user_id = Some("3".into());
    error.known.team_id = Some("12".into());
    error.required_inputs.push(RequiredInput {
        field: "agent.id",
        kind: InputKind::Selection,
        secret: false,
        source: InputSource::OwnedAgentList,
    });
    executor.failure = Some((Step::RegisterAgent, error));
    let mut events = Vec::new();
    let result = run(&context, executor, |event| {
        events.push(event.clone());
        Ok(())
    })
    .await;
    assert_eq!(*calls.lock().unwrap(), Step::ALL[..6]);
    assert_eq!(result.exit_code, 3);
    assert_eq!(result.status, RunStatus::ActionRequired);
    assert_eq!(result.stages[2].outcome, Outcome::Applied);
    assert_eq!(result.stages[5].status, StageStatus::Failed);
    assert_eq!(result.stages[5].outcome, Outcome::Unknown);
    assert!(
        result.stages[6..]
            .iter()
            .all(|s| s.status == StageStatus::Skipped)
    );
    assert_eq!(events.last().unwrap().step_status, StageStatus::Failed);
    let json = serde_json::to_value(result).unwrap();
    assert!(json.get("data").is_none());
    assert_eq!(json["error"]["step"], "register_agent");
    assert_eq!(json["error"]["known"]["teamId"], "12");
    assert!(json["error"]["known"].get("agentId").is_none());
    assert_eq!(json["error"]["requiredInputs"][0]["field"], "agent.id");
    assert_eq!(json["error"]["recovery"]["automatic"], false);
}

#[tokio::test]
async fn missing_input_is_returned_without_waiting_for_a_terminal() {
    let context = context();
    let mut executor = executor(&context);
    let mut error = SetupError::new(
        ErrorCode::InputRequired,
        Step::Validate,
        Outcome::NotApplied,
    );
    error.required_inputs.push(RequiredInput {
        field: "password",
        kind: InputKind::Secret,
        secret: true,
        source: InputSource::HiddenPromptOrStdin,
    });
    executor.failure = Some((Step::Validate, error));
    let result = tokio::time::timeout(Duration::from_secs(1), run(&context, executor, |_| Ok(())))
        .await
        .expect("the flow must return input requirements rather than prompt");
    assert_eq!(result.exit_code, 2);
    let Completion::Failure { error } = result.completion else {
        panic!("expected error")
    };
    assert_eq!(error.required_inputs.len(), 1);
    assert!(error.required_inputs[0].secret);
}

#[tokio::test]
async fn observer_disconnect_prevents_starting_a_stage() {
    let context = context();
    let executor = executor(&context);
    let calls = executor.calls.clone();
    let result = run(&context, executor, |_| Err(ProgressUnavailable)).await;
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stages[0].outcome, Outcome::NotApplied);
    assert!(
        result.stages[1..]
            .iter()
            .all(|s| s.status == StageStatus::Skipped)
    );
}

#[tokio::test]
async fn observer_disconnect_after_a_mutation_preserves_its_outcome_and_stops_next_stage() {
    let context = context();
    let executor = executor(&context);
    let calls = executor.calls.clone();
    let result = run(&context, executor, |event| {
        if event.step == Step::SelectTeam && event.step_status == StageStatus::Succeeded {
            Err(ProgressUnavailable)
        } else {
            Ok(())
        }
    })
    .await;
    assert_eq!(*calls.lock().unwrap(), Step::ALL[..3]);
    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stages[2].outcome, Outcome::Applied);
    assert!(
        result.stages[3..]
            .iter()
            .all(|s| s.status == StageStatus::Skipped)
    );
}

#[tokio::test]
async fn domain_error_survives_a_simultaneous_observer_disconnect() {
    let context = context();
    let mut executor = executor(&context);
    executor.failure = Some((
        Step::RegisterAgent,
        SetupError::new(
            ErrorCode::RegistrationUnknown,
            Step::RegisterAgent,
            Outcome::Unknown,
        ),
    ));
    let result = run(&context, executor, |event| {
        if event.step_status == StageStatus::Failed {
            Err(ProgressUnavailable)
        } else {
            Ok(())
        }
    })
    .await;
    let Completion::Failure { error } = result.completion else {
        panic!("expected error")
    };
    assert_eq!(error.code, ErrorCode::RegistrationUnknown);
    assert_eq!(error.outcome, Outcome::Unknown);
    assert_eq!(result.exit_code, 3);
}

#[tokio::test]
async fn completed_step_with_unknown_outcome_cannot_advance_or_report_success() {
    let context = context();
    let mut executor = executor(&context);
    executor.unknown_at = Some(Step::RegisterAgent);
    let calls = executor.calls.clone();
    let mut events = Vec::new();
    let result = run(&context, executor, |event| {
        events.push(event.clone());
        Ok(())
    })
    .await;
    assert_eq!(*calls.lock().unwrap(), Step::ALL[..6]);
    assert_eq!(result.status, RunStatus::Failed);
    assert_eq!(result.exit_code, 1);
    assert_eq!(result.stages[5].status, StageStatus::Failed);
    assert_eq!(result.stages[5].outcome, Outcome::Unknown);
    assert!(
        result.stages[6..]
            .iter()
            .all(|stage| stage.status == StageStatus::Skipped)
    );
    assert_eq!(events.last().unwrap().step_status, StageStatus::Failed);
    let Completion::Failure { error } = result.completion else {
        panic!("expected error")
    };
    assert_eq!(error.code, ErrorCode::InternalError);
    assert_eq!(error.outcome, Outcome::Unknown);
}

struct ResumingExecutor {
    inner: Executor,
    supplied: bool,
    repeat_required: bool,
    requests: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

struct RoutingExecutor {
    inner: Executor,
    answers: usize,
}
impl StepExecutor for RoutingExecutor {
    type Data = Vec<Step>;
    async fn execute(&mut self, step: Step, context: &SetupContext) -> StepResult {
        if step == Step::RegisterAgent && self.answers < 2 {
            let mut required = SetupError::new(ErrorCode::InputRequired, step, Outcome::NotApplied);
            required.required_inputs.push(RequiredInput {
                field: if self.answers == 0 {
                    "agent.action"
                } else {
                    "agent.part"
                },
                kind: InputKind::Selection,
                secret: false,
                source: InputSource::Input,
            });
            return StepResult::InputRequired(required);
        }
        self.inner.execute(step, context).await
    }
    fn can_resolve_input(&self) -> bool {
        true
    }
    fn input_is_interactive(&self) -> bool {
        true
    }
    async fn resolve_input(&mut self, _: &SetupError) -> Result<(), SetupError> {
        self.answers += 1;
        Ok(())
    }
    fn finish(self) -> Self::Data {
        self.inner.finish()
    }
}

#[tokio::test]
async fn interactive_agent_action_then_metadata_each_emit_waiting_input_and_resume() {
    let context = context();
    let executor = RoutingExecutor {
        inner: executor(&context),
        answers: 0,
    };
    let mut events = Vec::new();
    let result = run(&context, executor, |event| {
        events.push(event.clone());
        Ok(())
    })
    .await;
    assert_eq!(result.exit_code, 0);
    let waiting: Vec<_> = events
        .iter()
        .filter(|e| e.step_status == StageStatus::WaitingInput)
        .collect();
    assert_eq!(waiting.len(), 2);
    assert_eq!(waiting[0].required_inputs[0].field, "agent.action");
    assert_eq!(waiting[1].required_inputs[0].field, "agent.part");
    assert_eq!(result.sequence, 23);
}

impl StepExecutor for ResumingExecutor {
    type Data = Vec<Step>;
    async fn execute(&mut self, step: Step, context: &SetupContext) -> StepResult {
        if step == Step::Authenticate && (!self.supplied || self.repeat_required) {
            self.inner.calls.lock().unwrap().push(step);
            let mut required = SetupError::new(ErrorCode::AuthRequired, step, Outcome::Applied);
            required.required_inputs.push(RequiredInput {
                field: "password",
                kind: InputKind::Secret,
                secret: true,
                source: InputSource::HiddenPromptOrStdin,
            });
            return StepResult::InputRequired(required);
        }
        self.inner.execute(step, context).await
    }
    fn can_resolve_input(&self) -> bool {
        true
    }
    fn input_is_interactive(&self) -> bool {
        true
    }
    async fn resolve_input(&mut self, _required: &SetupError) -> Result<(), SetupError> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.supplied = true;
        Ok(())
    }
    fn finish(self) -> Self::Data {
        self.inner.finish()
    }
}

fn resuming(context: &SetupContext, repeat_required: bool) -> ResumingExecutor {
    ResumingExecutor {
        inner: executor(context),
        supplied: false,
        repeat_required,
        requests: std::sync::Arc::default(),
    }
}

#[tokio::test]
async fn input_resolution_resumes_only_the_current_stage_and_preserves_prior_mutation() {
    let context = context();
    let executor = resuming(&context, false);
    let calls = executor.inner.calls.clone();
    let mut events = Vec::new();
    let result = run(&context, executor, |event| {
        events.push(event.clone());
        Ok(())
    })
    .await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stages[1].outcome, Outcome::Applied);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|step| **step == Step::Validate)
            .count(),
        1
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|step| **step == Step::Authenticate)
            .count(),
        2
    );
    let waiting = events
        .iter()
        .find(|event| event.step_status == StageStatus::WaitingInput)
        .unwrap();
    assert_eq!(waiting.status, RunStatus::WaitingInput);
    assert_eq!(waiting.required_inputs[0].field, "password");
    assert_eq!(result.sequence, 21);
}

#[tokio::test]
async fn a_stage_cannot_request_credentials_repeatedly_after_one_resolution() {
    let context = context();
    let executor = resuming(&context, true);
    let requests = executor.requests.clone();
    let result = run(&context, executor, |_| Ok(())).await;
    assert_eq!(result.exit_code, 4);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(result.stages[1].outcome, Outcome::Applied);
    assert!(
        result.stages[2..]
            .iter()
            .all(|stage| stage.status == StageStatus::Skipped)
    );
}

#[tokio::test]
async fn losing_the_progress_consumer_before_input_does_not_collect_credentials() {
    let context = context();
    let executor = resuming(&context, false);
    let requests = executor.requests.clone();
    let result = run(&context, executor, |event| {
        if event.step_status == StageStatus::WaitingInput {
            Err(ProgressUnavailable)
        } else {
            Ok(())
        }
    })
    .await;
    assert_eq!(result.exit_code, 1);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(result.stages[1].outcome, Outcome::Applied);
    assert!(
        result.stages[2..]
            .iter()
            .all(|stage| stage.status == StageStatus::Skipped)
    );
}
