use serde::{Serialize, Serializer};
use std::path::{Path, PathBuf};

/// Resolved profile path shared by every stage. No terminal handles or credentials.
pub struct SetupContext {
    pub config_path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    Validate,
    Authenticate,
    SelectTeam,
    BindProject,
    SelectTool,
    RegisterAgent,
    StartBroker,
    ConfigureMcp,
    VerifyConnection,
}

impl Step {
    pub const ALL: [Self; 9] = [
        Self::Validate,
        Self::Authenticate,
        Self::SelectTeam,
        Self::BindProject,
        Self::SelectTool,
        Self::RegisterAgent,
        Self::StartBroker,
        Self::ConfigureMcp,
        Self::VerifyConnection,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Running,
    WaitingInput,
    Succeeded,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    NotApplied,
    Applied,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    WaitingInput,
    Succeeded,
    ActionRequired,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Serialize)]
pub struct Stage {
    pub step: Step,
    pub status: StageStatus,
    pub outcome: Outcome,
    pub reused: bool,
}

/// Only public, allowlisted error codes belong here; never forward provider messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ErrorCode {
    #[serde(rename = "SETUP_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "SETUP_INPUT_INVALID")]
    InputInvalid,
    #[serde(rename = "SETUP_REGISTRATION_UNKNOWN")]
    RegistrationUnknown,
    #[serde(rename = "SETUP_TOKEN_REQUIRED")]
    TokenRequired,
    #[serde(rename = "SETUP_STEP_UNAVAILABLE")]
    StepUnavailable,
    #[serde(rename = "SETUP_INTERNAL_ERROR")]
    InternalError,
    #[serde(rename = "SETUP_CONFIG_INVALID")]
    ConfigInvalid,
    #[serde(rename = "SETUP_CONFIG_PATH_INVALID")]
    ConfigPathInvalid,
    #[serde(rename = "SETUP_UNSAFE_FILE")]
    UnsafeFile,
    #[serde(rename = "SETUP_CONFIG_BUSY")]
    ConfigBusy,
    #[serde(rename = "SETUP_BROKER_BUSY")]
    BrokerBusy,
    #[serde(rename = "SETUP_CONFIG_STORAGE_FAILED")]
    ConfigStorageFailed,
}

impl ErrorCode {
    pub fn exit_code(self) -> u8 {
        match self {
            Self::InputRequired
            | Self::InputInvalid
            | Self::ConfigInvalid
            | Self::ConfigPathInvalid => 2,
            Self::RegistrationUnknown | Self::TokenRequired | Self::StepUnavailable => 3,
            Self::InternalError => 1,
            Self::UnsafeFile | Self::ConfigStorageFailed => 5,
            Self::ConfigBusy | Self::BrokerBusy => 6,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::InputRequired => "Required setup input is missing.",
            Self::InputInvalid => "Setup input is invalid.",
            Self::RegistrationUnknown => {
                "Agent registration outcome is unknown. Inspect your existing Agents."
            }
            Self::TokenRequired => "The known Agent requires explicit token issuance.",
            Self::StepUnavailable => {
                "This setup stage is not available in this build. Use the existing individual commands."
            }
            Self::InternalError => "Setup could not complete an internal operation.",
            Self::ConfigInvalid => {
                "Configuration is invalid. Repair the indicated file and field, then rerun setup. The existing file was preserved."
            }
            Self::ConfigPathInvalid => {
                "Choose a configuration file outside the internal runtime directory and rerun setup with --config."
            }
            Self::UnsafeFile => {
                "A setup file or directory is unsafe or unreadable. Inspect the indicated path and repair its ownership, permissions, or contents before retrying. Existing files were preserved."
            }
            Self::ConfigBusy => {
                "Another configuration operation holds the control lock. Wait for it to finish, then rerun setup."
            }
            Self::BrokerBusy => {
                "The broker is running or starting. Stop this profile's broker before creating its missing configuration."
            }
            Self::ConfigStorageFailed => {
                "Configuration storage failed. Inspect the indicated path and existing file before retrying; setup did not remove it."
            }
        }
    }

    fn recovery(self) -> &'static str {
        match self {
            Self::InputRequired => "provide_inputs",
            Self::InputInvalid => "correct_inputs",
            Self::RegistrationUnknown => "inspect_owned_agents_then_use",
            Self::TokenRequired => "explicit_issue_token",
            Self::StepUnavailable => "use_individual_commands",
            Self::InternalError => "inspect_safe_diagnostics",
            Self::ConfigInvalid => "repair_config_then_retry",
            Self::ConfigPathInvalid => "choose_config_path",
            Self::UnsafeFile => "repair_private_file",
            Self::ConfigBusy => "wait_for_config_operation",
            Self::BrokerBusy => "stop_owned_broker_then_retry",
            Self::ConfigStorageFailed => "inspect_config_before_retry",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputKind {
    Value,
    Selection,
    Secret,
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InputSource {
    Input,
    HiddenPromptOrStdin,
    VerifiedTeamList,
    OwnedAgentList,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequiredInput {
    pub field: &'static str,
    pub kind: InputKind,
    pub secret: bool,
    pub source: InputSource,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Recovery {
    pub action: &'static str,
    pub automatic: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ErrorLocation {
    #[serde(serialize_with = "serialize_location_path")]
    pub file: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<&'static str>,
}

fn serialize_location_path<S: Serializer>(path: &Path, serializer: S) -> Result<S::Ok, S::Error> {
    // Display invalid OS path units safely instead of losing the entire error response.
    serializer.serialize_str(&path.to_string_lossy())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetupError {
    pub code: ErrorCode,
    pub step: Step,
    pub outcome: Outcome,
    pub required_inputs: Vec<RequiredInput>,
    pub known: KnownIdentity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<ErrorLocation>,
    message: &'static str,
    retryable: bool,
    recovery: Recovery,
}

impl SetupError {
    pub fn new(code: ErrorCode, step: Step, outcome: Outcome) -> Self {
        Self {
            code,
            step,
            outcome,
            required_inputs: Vec::new(),
            known: KnownIdentity::default(),
            location: None,
            message: code.message(),
            retryable: matches!(
                code,
                ErrorCode::InputRequired
                    | ErrorCode::InputInvalid
                    | ErrorCode::ConfigInvalid
                    | ErrorCode::ConfigPathInvalid
                    | ErrorCode::ConfigBusy
                    | ErrorCode::BrokerBusy
            ),
            recovery: Recovery {
                action: code.recovery(),
                automatic: false,
            },
        }
    }

    pub fn message(&self) -> &'static str {
        self.message
    }

    pub fn recovery_action(&self) -> &'static str {
        self.recovery.action
    }
}

/// A stage must return a result; the flow never reads input or retries mutations.
pub enum StepResult {
    Complete { outcome: Outcome, reused: bool },
    Failed(SetupError),
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProgressEvent {
    pub schema_version: u8,
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub run_id: String,
    pub sequence: u64,
    pub status: RunStatus,
    pub step: Step,
    pub step_status: StageStatus,
    pub outcome: Outcome,
}

/// Flattened variants enforce exactly one of data/error in a final result.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Completion<D> {
    Success { data: D },
    Failure { error: SetupError },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult<D> {
    pub schema_version: u8,
    #[serde(rename = "type")]
    pub event_type: &'static str,
    pub run_id: String,
    pub sequence: u64,
    pub status: RunStatus,
    pub exit_code: u8,
    pub stages: Vec<Stage>,
    #[serde(flatten)]
    pub completion: Completion<D>,
}
