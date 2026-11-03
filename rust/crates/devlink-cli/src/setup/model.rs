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
    #[serde(rename = "SETUP_AUTH_REQUIRED")]
    AuthRequired,
    #[serde(rename = "SETUP_AUTH_REJECTED")]
    AuthRejected,
    #[serde(rename = "SETUP_CENTRAL_UNAVAILABLE")]
    CentralUnavailable,
    #[serde(rename = "SETUP_PROTOCOL_ERROR")]
    ProtocolError,
    #[serde(rename = "SETUP_AUTH_SESSION_CONFLICT")]
    AuthSessionConflict,
    #[serde(rename = "SETUP_AUTH_STORAGE_FAILED")]
    AuthStorageFailed,
    #[serde(rename = "SETUP_AUTH_BUSY")]
    AuthBusy,
    #[serde(rename = "SETUP_CANCELLED")]
    Cancelled,
    #[serde(rename = "SETUP_TEAM_REQUIRED")]
    TeamRequired,
    #[serde(rename = "SETUP_TEAM_MEMBERSHIP_REQUIRED")]
    TeamMembershipRequired,
    #[serde(rename = "SETUP_TEAM_FORBIDDEN")]
    TeamForbidden,
    #[serde(rename = "SETUP_TEAM_LIST_RESOURCE_NOT_FOUND")]
    TeamListResourceNotFound,
    #[serde(rename = "SETUP_TEAM_BINDING_CONFLICT")]
    TeamBindingConflict,
    #[serde(rename = "SETUP_TEAM_STORAGE_FAILED")]
    TeamStorageFailed,
    #[serde(rename = "SETUP_PROJECT_PATH_INVALID")]
    ProjectPathInvalid,
    #[serde(rename = "SETUP_PROJECT_BINDING_CONFLICT")]
    ProjectBindingConflict,
    #[serde(rename = "SETUP_PROJECT_STORAGE_FAILED")]
    ProjectStorageFailed,
    #[serde(rename = "SETUP_TOOL_REQUIRED")]
    ToolRequired,
    #[serde(rename = "SETUP_TOOL_UNAVAILABLE")]
    ToolUnavailable,
    #[serde(rename = "SETUP_TOOL_BINDING_CONFLICT")]
    ToolBindingConflict,
    #[serde(rename = "SETUP_TOOL_STORAGE_FAILED")]
    ToolStorageFailed,
    #[serde(rename = "SETUP_TOOL_SELECTION_CONFLICT")]
    ToolSelectionConflict,
    #[serde(rename = "SETUP_BROKER_START_FAILED")]
    BrokerStartFailed,
    #[serde(rename = "SETUP_AGENT_BINDING_CONFLICT")]
    AgentBindingConflict,
    #[serde(rename = "SETUP_AGENT_REJECTED")]
    AgentRejected,
    #[serde(rename = "SETUP_AGENT_ALREADY_EXISTS")]
    AgentAlreadyExists,
    #[serde(rename = "SETUP_AGENT_FORBIDDEN")]
    AgentForbidden,
    #[serde(rename = "SETUP_AGENT_STORAGE_FAILED")]
    AgentStorageFailed,
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
            Self::AuthRequired | Self::AuthRejected => 4,
            Self::CentralUnavailable | Self::ProtocolError => 7,
            Self::AuthSessionConflict | Self::AuthBusy => 6,
            Self::AuthStorageFailed => 5,
            Self::Cancelled => 130,
            Self::TeamRequired => 2,
            Self::TeamMembershipRequired => 3,
            Self::TeamForbidden => 4,
            Self::TeamListResourceNotFound => 7,
            Self::TeamBindingConflict => 6,
            Self::TeamStorageFailed => 5,
            Self::ProjectPathInvalid => 2,
            Self::ProjectBindingConflict => 6,
            Self::ProjectStorageFailed => 5,
            Self::ToolRequired => 2,
            Self::ToolUnavailable | Self::ToolStorageFailed => 5,
            Self::ToolBindingConflict | Self::ToolSelectionConflict => 6,
            Self::BrokerStartFailed | Self::AgentStorageFailed => 5,
            Self::AgentBindingConflict | Self::AgentAlreadyExists => 6,
            Self::AgentRejected | Self::AgentForbidden => 4,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::InputRequired => "Required setup input is missing.",
            Self::InputInvalid => "Setup input is invalid.",
            Self::RegistrationUnknown => {
                "Agent registration outcome is unknown. Run devlink agent list, then recover with setup --agent-id <ID>. Creation was not retried."
            }
            Self::TokenRequired => {
                "The known Agent requires explicit token preparation. Rerun setup --issue-token (or use devlink agent token) after inspecting the saved ID. An uncertain issuance is never replayed automatically."
            }
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
                "The broker is running or starting. Stop this profile's broker before creating configuration or changing Agent credentials."
            }
            Self::ConfigStorageFailed => {
                "Configuration storage failed. Inspect the indicated path and existing file before retrying; setup did not remove it."
            }
            Self::AuthRequired => {
                "Sign in is required. Provide an email and hidden password, or use --email with --password-stdin."
            }
            Self::AuthRejected => {
                "Authentication was rejected. Check the account credentials and retry explicitly."
            }
            Self::CentralUnavailable => {
                "The central service is temporarily unavailable. Check the connection and retry; this is not evidence of session expiry."
            }
            Self::ProtocolError => {
                "The central authentication response could not be verified. Inspect the provider contract before retrying."
            }
            Self::AuthSessionConflict => {
                "The central origin or stored session changed. Inspect the profile, then restart setup or sign in explicitly with devlink auth login."
            }
            Self::AuthStorageFailed => {
                "The user session could not be read or saved. Inspect its private files, permissions, and any running broker before retrying."
            }
            Self::AuthBusy => {
                "Another user session operation holds the account lock. Wait for it to finish, then rerun setup."
            }
            Self::Cancelled => {
                "Setup input was cancelled. Inspect the completed stages before resuming."
            }
            Self::TeamRequired => "Choose a joined team using --team-id or the terminal selection.",
            Self::TeamMembershipRequired => {
                "No joined teams were found. Use devlink team create --name <NAME> or devlink team join, then rerun setup."
            }
            Self::TeamForbidden => {
                "Current team membership could not be confirmed. Choose a joined team and retry explicitly."
            }
            Self::TeamListResourceNotFound => {
                "The central service could not find a team referenced by your membership records while listing teams (HTTP 404). Ask the server administrator to check the team and membership data, then rerun setup."
            }
            Self::TeamBindingConflict => {
                "The existing Agent binding conflicts with the selected server, user, or team. Inspect devlink agent status and explicitly bind an owned Agent, or choose a separate profile before rerunning setup."
            }
            Self::TeamStorageFailed => {
                "The team selection or Agent binding could not be read or saved. Inspect private profile files before retrying."
            }
            Self::ProjectPathInvalid => {
                "Choose an existing project directory with --project. File paths and missing directories are not accepted."
            }
            Self::ProjectBindingConflict => {
                "The profile belongs to another local project. Inspect devlink agent status and explicitly bind an owned Agent, or choose a separate --config profile. The existing profile was preserved."
            }
            Self::ProjectStorageFailed => {
                "The existing project binding could not be read. Inspect the private Agent profile before retrying; it was preserved."
            }
            Self::ToolRequired => {
                "Choose an available tool using --tool codex or --tool claude-code, or the terminal selection."
            }
            Self::ToolUnavailable => {
                "The requested tool is unavailable. Install Codex or Claude Code using the reported official installation link, reopen the terminal, then rerun setup --tool codex or setup --tool claude-code. Use --tool-path for an installation outside PATH. For unsupported pnpm/yarn shim layouts, specify the package's native executable or use a native installation. No AI session was started."
            }
            Self::ToolBindingConflict => {
                "The selected tool conflicts with the existing Agent binding. Inspect devlink agent status and explicitly bind an owned Agent, or use a separate profile."
            }
            Self::ToolStorageFailed => {
                "The private tool selection could not be read or saved. Inspect the indicated file before retrying."
            }
            Self::ToolSelectionConflict => {
                "The profile or tool selection changed while selecting a tool. Inspect the current selection and rerun setup explicitly."
            }
            Self::AgentBindingConflict => {
                "Agent identity, metadata or binding differs. For changed display metadata, stop this profile's broker and rerun setup --agent-id <ID> to explicitly refresh the same owned Agent. Inspect other binding conflicts before retrying; no new Agent was created."
            }
            Self::AgentForbidden => {
                "The selected Agent is not owned by the current user in the selected team, or access was denied. Select an owned Agent ID with --agent-id."
            }
            Self::AgentRejected => {
                "Agent creation or selection was rejected. Check the current account, team and explicit Agent ID before retrying."
            }
            Self::AgentAlreadyExists => {
                "This team already has an Agent with the same name and tool type, possibly owned by another user. Choose a different --agent-name with --create-agent, or inspect the owned Agent list and rerun setup --agent-id <ID>. No Agent was selected and no token was issued."
            }
            Self::AgentStorageFailed => {
                "Agent profile storage failed. Inspect the preserved profile and known Agent ID before retrying; do not create blindly."
            }
            Self::BrokerStartFailed => {
                "Broker startup or identity verification failed. Inspect devlink status, the profile and local port. Use devlink stop for a verified running broker before changing its profile or credentials; no unknown process was force-stopped."
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
            Self::AuthRequired => "login",
            Self::AuthRejected => "login",
            Self::CentralUnavailable => "check_network_then_retry",
            Self::ProtocolError => "check_provider_contract",
            Self::AuthSessionConflict => "inspect_profile_then_reauthenticate",
            Self::AuthStorageFailed => "inspect_private_session",
            Self::AuthBusy => "wait_for_auth_operation",
            Self::Cancelled => "inspect_status_before_resume",
            Self::TeamRequired => "select_verified_team",
            Self::TeamMembershipRequired => "create_or_join_team_then_retry",
            Self::TeamForbidden => "select_verified_team",
            Self::TeamListResourceNotFound => "inspect_central_team_data",
            Self::TeamBindingConflict => "inspect_agent_binding",
            Self::TeamStorageFailed => "inspect_team_selection",
            Self::ProjectPathInvalid => "choose_existing_project_directory",
            Self::ProjectBindingConflict => "inspect_project_binding",
            Self::ProjectStorageFailed => "inspect_agent_profile",
            Self::ToolRequired => "select_detected_tool",
            Self::ToolUnavailable => "install_or_select_tool",
            Self::ToolBindingConflict => "inspect_agent_binding",
            Self::ToolStorageFailed => "inspect_tool_selection",
            Self::ToolSelectionConflict => "inspect_tool_selection_then_retry",
            Self::BrokerStartFailed => "inspect_broker_then_retry",
            Self::AgentBindingConflict => "inspect_agent_binding",
            Self::AgentRejected => "inspect_owned_agents_then_use",
            Self::AgentAlreadyExists => "rename_agent_or_select_owned_id",
            Self::AgentForbidden => "select_owned_agent",
            Self::AgentStorageFailed => "inspect_agent_profile",
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
    DetectedToolList,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequiredInput {
    pub field: &'static str,
    pub kind: InputKind,
    pub secret: bool,
    pub source: InputSource,
}

#[derive(Clone, Debug, Serialize)]
pub struct TeamChoice {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentChoice {
    pub id: String,
    pub name: String,
    pub project_name: String,
    pub tool_type: devlink_broker::sessions::ToolType,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_type: Option<devlink_broker::sessions::ToolType>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub team_choices: Vec<TeamChoice>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agent_choices: Vec<AgentChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_list_error: Option<ErrorCode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<devlink_broker::tool_discovery::ToolDetection>,
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
            team_choices: Vec::new(),
            agent_choices: Vec::new(),
            agent_list_error: None,
            tools: Vec::new(),
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
                    | ErrorCode::AuthRequired
                    | ErrorCode::AuthRejected
                    | ErrorCode::CentralUnavailable
                    | ErrorCode::TeamRequired
                    | ErrorCode::TeamForbidden
                    | ErrorCode::ProjectPathInvalid
                    | ErrorCode::ToolRequired
                    | ErrorCode::ToolUnavailable
                    | ErrorCode::BrokerStartFailed
                    | ErrorCode::AgentForbidden
                    | ErrorCode::AgentAlreadyExists
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
    InputRequired(SetupError),
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub required_inputs: Vec<RequiredInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub team_choices: Vec<TeamChoice>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agent_choices: Vec<AgentChoice>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<devlink_broker::tool_discovery::ToolDetection>,
}

/// Flattened variants enforce exactly one of data/error in a final result.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Completion<D> {
    Success { data: D },
    Failure { error: Box<SetupError> },
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
