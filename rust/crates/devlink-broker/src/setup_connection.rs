//! Read-only evidence for setup completion and setup status.
use crate::{
    agent_profile::{self, InspectionBinding},
    central::ConnectionStatus,
    config, credentials,
    local::Runtime,
    mcp_config::{self, InspectionInput, InspectionStatus, McpConfig},
    sessions::{IdentityError, ToolType},
    setup_mcp::{self, Options, PreflightContext},
    setup_tools::{self, SelectionBinding},
};
use serde::{Deserialize, Serialize, Serializer};
use std::{
    fs::{self, File},
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

macro_rules! state {
    ($name:ident { $($variant:ident),+ }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}

state!(Broker {
    Running,
    Stopped,
    Unhealthy,
    Unknown
});
state!(Identity {
    Verified,
    Unauthenticated,
    Mismatch,
    Unknown
});
state!(Credentials {
    Valid,
    Missing,
    Invalid,
    Unknown
});
state!(HostSession {
    Bound,
    Unbound,
    Conflict,
    NotChecked,
    Unknown
});

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Principal {
    pub user_id: String,
    pub team_id: String,
    pub agent_id: String,
    pub tool_type: Option<ToolType>,
}

impl Principal {
    fn matches(&self, binding: &InspectionBinding) -> bool {
        self.user_id == binding.user_id.to_string()
            && self.team_id == binding.team_id.to_string()
            && self.agent_id == binding.agent_id.to_string()
            && self.tool_type == Some(binding.tool)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub observed_at: String,
    pub broker: Broker,
    #[serde(serialize_with = "serialize_central")]
    pub central: Option<ConnectionStatus>,
    pub identity: Identity,
    pub credentials: Credentials,
    pub mcp_config: InspectionStatus,
    pub host_session: HostSession,
    pub setup_ready: bool,
    pub collaboration_ready: Option<bool>,
}

fn serialize_central<S: Serializer>(
    status: &Option<ConnectionStatus>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match status {
        Some(status) => status.serialize(serializer),
        None => serializer.serialize_str("unknown"),
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    #[serde(flatten)]
    pub principal: Option<Principal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_config_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub broker_file: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_config: Option<setup_mcp::ConfigurationResult>,
    pub connection: Snapshot,
    pub next_actions: Vec<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    UnsafeFile,
    Unavailable,
    Protocol,
    Changed,
    /// The running broker predates the setup diagnostic endpoint.
    Unsupported,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WireSnapshot {
    central: ConnectionStatus,
    identity: Identity,
    credentials: Credentials,
    host_session: HostSession,
    principal: Option<Principal>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    request_id: String,
    data: WireSnapshot,
}

impl Report {
    fn new() -> Self {
        Self {
            principal: None,
            project_path: None,
            mcp_config_path: None,
            broker_file: None,
            mcp_config: None,
            connection: Snapshot {
                observed_at: String::new(),
                broker: Broker::Unknown,
                central: None,
                identity: Identity::Unknown,
                credentials: Credentials::Unknown,
                mcp_config: InspectionStatus::Unknown,
                host_session: HostSession::NotChecked,
                setup_ready: false,
                collaboration_ready: Some(false),
            },
            next_actions: Vec::new(),
        }
    }

    fn finish(mut self) -> Self {
        let state = &mut self.connection;
        state.observed_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("UTC timestamp");
        state.setup_ready = state.broker == Broker::Running
            && state.central == Some(ConnectionStatus::Connected)
            && state.identity == Identity::Verified
            && state.credentials == Credentials::Valid
            && state.mcp_config == InspectionStatus::Applied;
        state.collaboration_ready = match (state.setup_ready, state.host_session) {
            (false, _) | (true, HostSession::Unbound | HostSession::Conflict) => Some(false),
            (true, HostSession::Bound) => Some(true),
            (true, HostSession::NotChecked | HostSession::Unknown) => None,
        };
        self.next_actions = match (state.broker, state.central, state.identity) {
            (Broker::Stopped, _, _) => vec!["start_broker"],
            (Broker::Unhealthy | Broker::Unknown, _, _) => vec!["inspect_local_broker"],
            (_, Some(ConnectionStatus::Replaced), _) => vec!["inspect_connection_owner"],
            (_, Some(ConnectionStatus::InboxFailed), _) => vec!["repair_inbox_then_restart"],
            (_, _, Identity::Unauthenticated) => vec!["stop_then_reauthenticate_agent"],
            (_, _, Identity::Mismatch) => vec!["inspect_profile_then_rebind"],
            (_, Some(ConnectionStatus::ProtocolViolation), _)
            | (_, Some(ConnectionStatus::Connected), Identity::Unknown) => {
                vec!["check_provider_contract"]
            }
            (_, Some(ConnectionStatus::Connected), _) => Vec::new(),
            _ => vec!["check_network_then_retry"],
        };
        if state.broker == Broker::Running && state.credentials != Credentials::Valid {
            self.next_actions.push("repair_private_file");
        }
        if state.identity == Identity::Verified && state.mcp_config != InspectionStatus::Applied {
            self.next_actions.push("review_mcp_config");
        }
        if state.setup_ready {
            match state.host_session {
                HostSession::Bound => {}
                HostSession::Conflict => self.next_actions.push("inspect_connection_owner"),
                _ => self.next_actions.push("open_or_restart_host"),
            }
        }
        self
    }
}

/// Inspect the existing lock without creating it or cleaning stale runtime files.
fn running(path: &Path) -> Result<bool, ReadError> {
    let state = config::state_directory(path);
    match fs::symlink_metadata(&state) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(ReadError::UnsafeFile),
        Ok(_) => credentials::verify_parent_safety(&state).map_err(|_| ReadError::UnsafeFile)?,
    }
    let lock = state.join("broker.lock");
    if !credentials::ensure_safe_target_if_exists(&lock).map_err(|_| ReadError::UnsafeFile)? {
        return Ok(false);
    }
    let file = match File::open(lock) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(ReadError::UnsafeFile),
    };
    match fs2::FileExt::try_lock_shared(&file) {
        Ok(()) => Ok(false),
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            Ok(true)
        }
        Err(_) => Err(ReadError::UnsafeFile),
    }
}

async fn query(runtime: &Runtime) -> Result<WireSnapshot, ReadError> {
    let request_id = uuid::Uuid::new_v4().to_string();
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8))
        .build()
        .map_err(|_| ReadError::Unavailable)?;
    let mut response = client
        .get(runtime.url("/v1/setup-connection"))
        .bearer_auth(&runtime.control_token)
        .header("x-instance-id", &runtime.instance_id)
        .header("x-request-id", &request_id)
        .send()
        .await
        .map_err(|_| ReadError::Unavailable)?;
    match response.status().as_u16() {
        200 => {}
        // API_VERSION matched in /health, so an unknown route is an older broker build.
        404 => return Err(ReadError::Unsupported),
        409 => return Err(ReadError::Changed),
        _ => return Err(ReadError::Protocol),
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ReadError::Unavailable)? {
        if bytes.len() + chunk.len() > 16_384 {
            return Err(ReadError::Protocol);
        }
        bytes.extend_from_slice(&chunk);
    }
    let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| ReadError::Protocol)?;
    if envelope.request_id != request_id
        || (envelope.data.identity == Identity::Verified
            && envelope.data.central != ConnectionStatus::Connected)
    {
        return Err(ReadError::Protocol);
    }
    Ok(envelope.data)
}

fn inspect_mcp(
    path: &Path,
    binding: &InspectionBinding,
    options: &Options,
    executable: &Path,
    selected: Option<&crate::tool_discovery::SelectedTool>,
) -> (PathBuf, InspectionStatus) {
    let project = PathBuf::from(&binding.project_root);
    let target = mcp_config::project_target(binding.tool, &project);
    let stored = setup_tools::inspect(
        path,
        &SelectionBinding {
            origin: binding.origin.clone(),
            user_id: binding.user_id,
            team_id: binding.team_id,
            project_root: project.clone(),
        },
    )
    .ok();
    let launcher = selected
        .or_else(|| stored.as_ref()?.preferred())
        .filter(|tool| tool.tool_type == binding.tool)
        .map(|tool| tool.executable.command.clone());
    let context = PreflightContext {
        config_path: path.into(),
        project,
        tool_type: binding.tool,
        host_launcher: launcher,
        mcp_automatic_supported: false,
    };
    let status = match setup_mcp::runtime_input(&context, options, executable) {
        Ok(input) => McpConfig::inspect_for(
            binding.tool,
            InspectionInput {
                tool_type: input.tool_type,
                target_path: input.target_path,
                broker_file: input.broker_file,
                executable: input.executable,
            },
        )
        .map(|result| result.status)
        .unwrap_or(InspectionStatus::Unknown),
        Err(_) if matches!(fs::symlink_metadata(&target), Err(e) if e.kind() == ErrorKind::NotFound) => {
            InspectionStatus::Missing
        }
        Err(_) => InspectionStatus::Unknown,
    };
    (target, status)
}

/// Both entry points use the same read-only checks. Setup additionally pins the
/// observation to the profile and broker selected by its earlier stages.
pub async fn inspect(
    path: &Path,
    options: &Options,
    executable: &Path,
    expected: Option<&setup_mcp::Context>,
) -> Result<Report, ReadError> {
    let mut report = Report::new();
    if !running(path)? {
        if expected.is_some() {
            return Err(ReadError::Changed);
        }
        report.connection.broker = Broker::Stopped;
        if matches!(fs::symlink_metadata(credentials::path(path)), Err(e) if e.kind() == ErrorKind::NotFound)
        {
            report.connection.credentials = Credentials::Missing;
        }
        return Ok(report.finish());
    }
    let runtime = match Runtime::load(path) {
        Ok(runtime) => runtime,
        Err(_) => {
            report.connection.broker = Broker::Unhealthy;
            return Ok(report.finish());
        }
    };
    if expected
        .is_some_and(|e| runtime.pid != e.broker_pid || runtime.instance_id != e.broker_instance)
    {
        return Err(ReadError::Changed);
    }
    let Ok(health) = runtime.health().await else {
        report.connection.broker = Broker::Unhealthy;
        return Ok(report.finish());
    };
    report.connection.broker = Broker::Running;
    report.connection.central = Some(health.central_server);

    let profile = path.to_path_buf();
    let binding = tokio::task::spawn_blocking(move || agent_profile::inspection_binding(&profile))
        .await
        .map_err(|_| ReadError::Unavailable)?;
    if let Some(expected) = expected
        && !matches!(&binding, Ok(Some(b))
            if b.origin == expected.origin && b.user_id == expected.user_id
                && b.team_id == expected.team_id && b.agent_id == expected.agent_id
                && b.tool == expected.tool.tool_type && Path::new(&b.project_root) == expected.project)
    {
        return Err(ReadError::Changed);
    }

    let observed = match query(&runtime).await {
        Ok(observed) => observed,
        // Report the reused broker as not meeting the current local contract so
        // recovery points at restarting it, not at the central provider.
        Err(ReadError::Unsupported) => {
            report.connection.broker = Broker::Unhealthy;
            return Ok(report.finish());
        }
        Err(error) => return Err(error),
    };
    report.connection.central = Some(observed.central);
    report.connection.credentials = observed.credentials;
    report.connection.identity = observed.identity;
    if let Ok(Some(binding)) = &binding
        && (expected.is_none() || observed.identity == Identity::Verified)
    {
        let (profile, local, options, executable, selected) = (
            path.to_path_buf(),
            binding.clone(),
            options.clone(),
            executable.to_path_buf(),
            expected.map(|e| e.tool.clone()),
        );
        let (target, status) = tokio::task::spawn_blocking(move || {
            inspect_mcp(&profile, &local, &options, &executable, selected.as_ref())
        })
        .await
        .map_err(|_| ReadError::Unavailable)?;
        report.project_path = Some(PathBuf::from(&binding.project_root));
        report.mcp_config_path = Some(target);
        report.broker_file = Some(credentials::path(path));
        report.connection.mcp_config = status;
    }
    if let (Ok(Some(binding)), Identity::Verified) = (&binding, observed.identity) {
        match observed.principal {
            Some(principal) if principal.matches(binding) => {
                report.principal = Some(principal);
                report.connection.host_session = observed.host_session;
            }
            Some(principal) if principal.tool_type.is_some() => {
                report.connection.identity = Identity::Mismatch;
            }
            _ => report.connection.identity = Identity::Unknown,
        }
    } else if observed.identity == Identity::Verified {
        report.connection.identity = match binding {
            Err(IdentityError::Forbidden) => Identity::Mismatch,
            _ => Identity::Unknown,
        };
    }

    let profile = path.to_path_buf();
    let latest = tokio::task::spawn_blocking(move || agent_profile::inspection_binding(&profile))
        .await
        .map_err(|_| ReadError::Unavailable)?;
    if binding != latest
        || !running(path)?
        || Runtime::load(path).ok().is_none_or(|current| {
            current.pid != runtime.pid
                || current.instance_id != runtime.instance_id
                || current.port != runtime.port
                || current.control_token != runtime.control_token
        })
    {
        return Err(ReadError::Changed);
    }
    Ok(report.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_older_broker_without_the_setup_endpoint_is_unsupported() {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new().fallback(|| async { axum::http::StatusCode::NOT_FOUND });
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let runtime = Runtime {
            api_version: crate::local::API_VERSION,
            pid: std::process::id(),
            port,
            instance_id: "instance".into(),
            control_token: "control".into(),
        };
        assert_eq!(query(&runtime).await.err(), Some(ReadError::Unsupported));
        server.abort();
    }

    #[test]
    fn an_unhealthy_broker_points_to_local_broker_recovery() {
        let mut report = Report::new();
        report.connection.broker = Broker::Unhealthy;
        let report = report.finish();
        assert!(!report.connection.setup_ready);
        assert_eq!(report.next_actions, ["inspect_local_broker"]);
    }
}
