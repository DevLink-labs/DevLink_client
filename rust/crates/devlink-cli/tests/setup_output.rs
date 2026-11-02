use devlink_cli::setup::{
    Completion, ErrorCode, ErrorLocation, Outcome, ProgressEvent, RunResult, RunStatus, SetupError,
    Stage, StageStatus, Step,
    terminal::{OutputFormat, Renderer},
};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::io::{self, Write};

fn progress() -> ProgressEvent {
    ProgressEvent {
        schema_version: 1,
        event_type: "progress",
        run_id: "9d978406-2adb-4a98-9ddc-5f6386a70618".into(),
        sequence: 1,
        status: RunStatus::Running,
        step: Step::Validate,
        step_status: StageStatus::Running,
        outcome: Outcome::NotApplied,
    }
}

fn result<D>(data: D) -> RunResult<D> {
    RunResult {
        schema_version: 1,
        event_type: "result",
        run_id: progress().run_id,
        sequence: 19,
        status: RunStatus::Succeeded,
        exit_code: 0,
        stages: Step::ALL
            .into_iter()
            .map(|step| Stage {
                step,
                status: StageStatus::Succeeded,
                outcome: Outcome::NotApplied,
                reused: false,
            })
            .collect(),
        completion: Completion::Success { data },
    }
}

#[test]
fn json_outputs_only_one_result_and_escapes_controls_without_losing_unicode() {
    let data = json!({ "label": "프로젝트\u{0085}\u{202e}작업\n다음 줄" });
    let mut output = Vec::new();
    let mut renderer = Renderer::new(&mut output, OutputFormat::Json);
    renderer.progress(&progress()).unwrap();
    renderer.result(&result(data.clone())).unwrap();
    let text = String::from_utf8(output).unwrap();
    assert_eq!(text.lines().count(), 1);
    assert!(!text.contains('\u{0085}'));
    assert!(!text.contains('\u{202e}'));
    assert!(text.contains("\\u0085"));
    assert!(text.contains("\\u202e"));
    let decoded: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(decoded["type"], "result");
    assert_eq!(decoded["data"], data);
}

#[test]
fn jsonl_keeps_each_progress_and_final_result_in_a_separate_record() {
    let mut output = Vec::new();
    let mut renderer = Renderer::new(&mut output, OutputFormat::Jsonl);
    renderer.progress(&progress()).unwrap();
    renderer.result(&result(json!({}))).unwrap();
    let text = String::from_utf8(output).unwrap();
    let records: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["type"], "progress");
    assert_eq!(records[1]["type"], "result");
    assert_eq!(records[0]["runId"], records[1]["runId"]);
}

struct Disconnected;

impl Write for Disconnected {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "private diagnostic",
        ))
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "private diagnostic",
        ))
    }
}

#[test]
fn a_disconnected_output_is_reported_without_forwarding_the_io_diagnostic() {
    for format in [OutputFormat::Human, OutputFormat::Jsonl] {
        let mut renderer = Renderer::new(Disconnected, format);
        let error = renderer.progress(&progress()).unwrap_err();
        assert!(!format!("{error:?}").contains("private diagnostic"));
    }
    for format in [OutputFormat::Human, OutputFormat::Json, OutputFormat::Jsonl] {
        let mut renderer = Renderer::new(Disconnected, format);
        assert!(renderer.result(&result(json!({}))).is_err());
    }
}

struct InvalidData;

impl Serialize for InvalidData {
    fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("private serializer diagnostic"))
    }
}

#[test]
fn serialization_failure_does_not_write_a_partial_record_or_expose_the_diagnostic() {
    let mut output = Vec::new();
    let mut renderer = Renderer::new(&mut output, OutputFormat::Json);
    let error = renderer.result(&result(InvalidData)).unwrap_err();
    assert!(output.is_empty());
    assert!(!format!("{error:?}").contains("private serializer diagnostic"));
}

#[cfg(any(windows, unix))]
#[test]
fn invalid_os_path_units_do_not_hide_the_structured_error_and_recovery() {
    #[cfg(windows)]
    let invalid = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xd800])
    };
    #[cfg(unix)]
    let invalid = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    };
    let invalid_path = std::path::PathBuf::from(invalid);
    assert!(serde_json::to_value(&invalid_path).is_err());
    let mut failure = result(());
    failure.status = RunStatus::Failed;
    failure.exit_code = 5;
    failure.stages[0].status = StageStatus::Failed;
    for stage in &mut failure.stages[1..] {
        stage.status = StageStatus::Skipped;
    }
    let mut error = SetupError::new(ErrorCode::UnsafeFile, Step::Validate, Outcome::NotApplied);
    error.location = Some(ErrorLocation {
        file: invalid_path,
        field: None,
    });
    failure.completion = Completion::Failure { error };
    let mut output = Vec::new();
    Renderer::new(&mut output, OutputFormat::Json)
        .result(&failure)
        .unwrap();
    let decoded: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(decoded["error"]["location"]["file"], "\u{fffd}");
    assert_eq!(
        decoded["error"]["recovery"]["action"],
        "repair_private_file"
    );
}
