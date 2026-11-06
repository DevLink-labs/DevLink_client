//! Setup's in-process MCP configuration adapter. Host formats live in mcp_config.
use crate::{
    agent_profile, config, credentials, local,
    mcp_config::{self, *},
    private_file, setup_tools,
    tool_discovery::SelectedTool,
    user_auth,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};

type Result<T> = std::result::Result<T, ConfigError>;
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Apply,
    Manual,
}
pub fn parse_mode(value: &str) -> std::result::Result<Mode, &'static str> {
    match value {
        "apply" => Ok(Mode::Apply),
        "manual" => Ok(Mode::Manual),
        _ => Err("Choose apply or manual"),
    }
}
#[derive(Clone, Default)]
pub struct Options {
    pub mode: Mode,
    pub target: Option<PathBuf>,
    pub node: Option<PathBuf>,
    pub entry: Option<PathBuf>,
    pub apply: bool,
}
impl Options {
    pub fn validate(&self) -> Result<()> {
        if self.mode == Mode::Manual && (self.target.is_some() || self.apply) {
            return Err(error("SETUP_INPUT_INVALID"));
        }
        for path in [&self.target, &self.node, &self.entry]
            .into_iter()
            .flatten()
        {
            if !path.is_absolute()
                || path
                    .to_str()
                    .is_none_or(|s| s.contains("${") || s.chars().any(char::is_control))
                || path.components().any(|p| {
                    matches!(
                        p,
                        std::path::Component::ParentDir | std::path::Component::CurDir
                    )
                })
            {
                return Err(error("SETUP_INPUT_INVALID"));
            }
        }
        if [&self.node, &self.entry]
            .into_iter()
            .flatten()
            .any(|path| !path.is_file())
        {
            return Err(error("SETUP_TOOL_UNAVAILABLE"));
        }
        Ok(())
    }
}
#[derive(Clone)]
pub struct Context {
    pub config_path: PathBuf,
    pub origin: String,
    pub user_id: u64,
    pub team_id: u64,
    pub agent_id: u64,
    pub project: PathBuf,
    pub tool: SelectedTool,
    pub broker_pid: u32,
    pub broker_instance: String,
}
#[derive(Clone)]
pub struct PreflightContext {
    pub config_path: PathBuf,
    pub project: PathBuf,
    pub tool_type: crate::sessions::ToolType,
    pub host_launcher: Option<PathBuf>,
    /// Only a provider-verified Host version is applied automatically. An unverified
    /// one ends in manual guidance, where an existing entry is the user's to merge.
    pub mcp_automatic_supported: bool,
}
impl From<&Context> for PreflightContext {
    fn from(context: &Context) -> Self {
        Self {
            config_path: context.config_path.clone(),
            project: context.project.clone(),
            tool_type: context.tool.tool_type,
            host_launcher: Some(context.tool.executable.command.clone()),
            mcp_automatic_supported: context.tool.verified_host().is_ok(),
        }
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationResult {
    #[serde(flatten)]
    pub config: ConfigApplyResult,
    pub runtime_verified: bool,
    pub host_session: &'static str,
    pub next_actions: Vec<&'static str>,
}
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    profile: [u8; 32],
    credentials: [u8; 32],
    node: [u8; 32],
    entry: [u8; 32],
}
pub struct Preparation {
    context: Context,
    input: PlanInput,
    provider: Option<McpConfig>,
    plan: Option<ConfigPlan>,
    manual: ManualConfig,
    fingerprint: Fingerprint,
}
impl Preparation {
    pub fn plan(&self) -> Option<&ConfigPlan> {
        self.plan.as_ref()
    }
    pub fn manual(&self) -> &ManualConfig {
        &self.manual
    }
    pub fn requires_manual(&self) -> bool {
        self.provider.is_none()
    }
    pub async fn apply(self) -> Result<ConfigurationResult> {
        tokio::task::spawn_blocking(move || {
            let mut prepared = self;
            let _guards = check_context(&prepared.context)?;
            if fingerprints(&prepared.context, &prepared.input)? != prepared.fingerprint {
                return Err(error("SETUP_PROFILE_CONFLICT"));
            }
            let provider = prepared
                .provider
                .as_mut()
                .ok_or_else(|| error("SETUP_MANUAL_ACTION_REQUIRED"))?;
            let plan = prepared
                .plan
                .as_ref()
                .ok_or_else(|| error("SETUP_MCP_CONFIG_CONFLICT"))?;
            let applied = provider.apply(&plan.plan_id, plan.expected_digest.as_deref())?;
            let inspection = provider
                .inspect(InspectionInput {
                    tool_type: prepared.input.tool_type,
                    target_path: prepared.input.target_path,
                    broker_file: prepared.input.broker_file,
                    executable: prepared.input.executable,
                })
                .map_err(|_| ConfigError::uncertain("not_needed", applied.backup_path.clone()))?;
            if inspection.status != InspectionStatus::Applied {
                return Err(ConfigError::uncertain("not_needed", applied.backup_path));
            }
            let next = if applied.restart_required {
                "restart_host_then_register_agent"
            } else {
                "open_host_then_register_agent"
            };
            let mut next_actions = Vec::new();
            if prepared.context.tool.tool_type == crate::sessions::ToolType::Codex {
                next_actions.push("trust_codex_project");
            }
            next_actions.push(next);
            Ok(ConfigurationResult {
                config: applied,
                runtime_verified: true,
                host_session: "not_checked",
                next_actions,
            })
        })
        .await
        .map_err(|_| ConfigError::uncertain("not_needed", None))?
    }
}
fn error(code: &'static str) -> ConfigError {
    ConfigError::new(code)
}

// Account and control locks prevent reconfiguration while the provider publishes.
// During version/stdio probes and user input they are released and checked again.
fn check_context(context: &Context) -> Result<(File, File)> {
    let state = config::state_directory(&context.config_path);
    let account_path = state.join("account.lock");
    let account = config::try_lock(&account_path)
        .map_err(|e| ConfigError::unsafe_at(&account_path, e))?
        .ok_or_else(|| error("SETUP_AUTH_BUSY"))?;
    let control_path = state.join("control.lock");
    let control = config::try_lock(&control_path)
        .map_err(|e| ConfigError::unsafe_at(&control_path, e))?
        .ok_or_else(|| error("SETUP_CONFIG_BUSY"))?;
    let binding = agent_profile::inspection_binding(&context.config_path)
        .map_err(|_| error("SETUP_PROFILE_CONFLICT"))?
        .ok_or_else(|| error("SETUP_PROFILE_CONFLICT"))?;
    let current = config::Config::load(&context.config_path)
        .and_then(|c| user_auth::origin(&c))
        .map_err(|e| ConfigError::unsafe_at(&context.config_path, e))?;
    if current.as_str() != context.origin
        || binding.origin != context.origin
        || binding.user_id != context.user_id
        || binding.team_id != context.team_id
        || binding.agent_id != context.agent_id
        || Path::new(&binding.project_root) != context.project
        || agent_profile::stored_token(&context.config_path)
            .map_err(|_| error("SETUP_PROFILE_CONFLICT"))?
            .is_none()
    {
        return Err(error("SETUP_PROFILE_CONFLICT"));
    }
    let selected = setup_tools::inspect(
        &context.config_path,
        &setup_tools::SelectionBinding {
            origin: context.origin.clone(),
            user_id: context.user_id,
            team_id: context.team_id,
            project_root: context.project.clone(),
        },
    )
    .map_err(|_| error("SETUP_PROFILE_CONFLICT"))?;
    if selected.preferred() != Some(&context.tool) {
        return Err(error("SETUP_PROFILE_CONFLICT"));
    }
    if !local::running(&context.config_path)
        .map_err(|e| ConfigError::unsafe_at(&state.join("broker.lock"), e))?
    {
        return Err(error("SETUP_BROKER_START_FAILED"));
    }
    let runtime = local::Runtime::load(&context.config_path)
        .map_err(|_| error("SETUP_BROKER_START_FAILED"))?;
    let descriptor_path = credentials::path(&context.config_path);
    let descriptor = credentials::load(&context.config_path)
        .map_err(|e| ConfigError::unsafe_at(&descriptor_path, e))?;
    let url = url::Url::parse(&descriptor.broker_url)
        .map_err(|_| error("SETUP_UNSAFE_FILE").at(&descriptor_path))?;
    if runtime.pid != context.broker_pid
        || runtime.instance_id != context.broker_instance
        || url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port_or_known_default() != Some(runtime.port)
    {
        return Err(error("SETUP_PROFILE_CONFLICT"));
    }
    Ok((account, control))
}
fn file_hash(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).map_err(|_| error("SETUP_UNSAFE_FILE").at(path))?;
    let mut digest = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|_| error("SETUP_UNSAFE_FILE").at(path))?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
    }
    Ok(digest.finalize().into())
}
fn fingerprints(context: &Context, input: &PlanInput) -> Result<Fingerprint> {
    let profile_path = agent_profile::path(&context.config_path);
    let profile = private_file::read_bytes(&profile_path, 16_384)
        .map_err(|e| ConfigError::unsafe_at(&profile_path, e))?
        .ok_or_else(|| error("SETUP_PROFILE_CONFLICT"))?;
    let descriptor_path = credentials::path(&context.config_path);
    let credentials =
        private_file::read_bytes(&descriptor_path, credentials::MAX_CREDENTIALS_SIZE as usize)
            .map_err(|e| ConfigError::unsafe_at(&descriptor_path, e))?
            .ok_or_else(|| error("SETUP_PROFILE_CONFLICT"))?;
    Ok(Fingerprint {
        profile: Sha256::digest(profile).into(),
        credentials: Sha256::digest(credentials).into(),
        node: file_hash(&input.executable.command)?,
        entry: file_hash(Path::new(&input.executable.args[0]))?,
    })
}
fn path_node() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .filter(|p| p.is_absolute())
            .flat_map(|p| {
                if cfg!(windows) {
                    vec![p.join("node.exe")]
                } else {
                    vec![p.join("node"), p.join("nodejs")]
                }
            })
            .find(|p| p.is_file())
    })
}
fn runtime_entry(options: &Options, executable: &Path) -> Result<PathBuf> {
    options
        .entry
        .clone()
        .or_else(|| {
            executable.parent().and_then(|parent| {
                let adjacent = parent.join("mcp-server/dist/index.js");
                if adjacent.is_file() {
                    return Some(adjacent);
                }
                // Development checkout, resolved from the CLI location, never cwd or a compile-time PC path.
                parent.ancestors().find_map(|root| {
                    let entry = root.join("packages/mcp-server/dist/index.js");
                    (root.join("rust/Cargo.toml").is_file()
                        && root.join("packages/mcp-server/package.json").is_file()
                        && entry.is_file())
                    .then_some(entry)
                })
            })
        })
        .ok_or_else(|| error("SETUP_TOOL_UNAVAILABLE"))
}

fn runtime_target(context: &PreflightContext, options: &Options) -> PathBuf {
    options
        .target
        .clone()
        .unwrap_or_else(|| mcp_config::project_target(context.tool_type, &context.project))
}

pub(crate) fn runtime_input(
    context: &PreflightContext,
    options: &Options,
    executable: &Path,
) -> Result<PlanInput> {
    options.validate()?;
    let node = options
        .node
        .clone()
        .or_else(|| {
            let launcher = context.host_launcher.as_ref()?;
            let name = launcher.file_name()?.to_str()?;
            ["node", "nodejs", "node.exe"]
                .iter()
                .any(|n| name.eq_ignore_ascii_case(n))
                .then(|| launcher.clone())
        })
        .or_else(path_node)
        .ok_or_else(|| error("SETUP_TOOL_UNAVAILABLE"))?;
    let entry = runtime_entry(options, executable)?;
    Ok(PlanInput {
        tool_type: context.tool_type,
        project_path: context.project.clone(),
        target_path: runtime_target(context, options),
        broker_file: credentials::path(&context.config_path),
        executable: Executable {
            command: node,
            args: vec![
                entry
                    .to_str()
                    .ok_or_else(|| error("SETUP_INPUT_INVALID"))?
                    .into(),
                "--transport".into(),
                "stdio".into(),
            ],
        },
    })
}

/// No Agent, token, descriptor or running broker is needed for this read-only
/// check. It grants no apply permission; prepare/apply validate everything again.
/// The existing Host entry is merged only when it would be applied automatically.
pub async fn preflight(
    context: PreflightContext,
    options: Options,
    executable: PathBuf,
) -> Result<()> {
    let inspect_target = options.mode == Mode::Apply && context.mcp_automatic_supported;
    let input = tokio::task::spawn_blocking(move || {
        let input = runtime_input(&context, &options, &executable)?;
        mcp_config::preflight(input.clone(), inspect_target)?;
        Ok::<_, ConfigError>(input)
    })
    .await
    .map_err(|_| error("SETUP_INTERNAL_ERROR"))??;
    probe_runtime(&input.executable).await?;
    tokio::task::spawn_blocking(move || mcp_config::preflight(input, inspect_target))
        .await
        .map_err(|_| error("SETUP_INTERNAL_ERROR"))?
}

/// Check paths before authentication, when the saved Host is not yet scoped.
/// Only an explicit Node can be probed now. A saved Host may supply a different
/// Node than PATH, so probe the selected runtime after scoping the saved Host.
pub async fn preflight_runtime(
    mut context: PreflightContext,
    options: Options,
    executable: PathBuf,
) -> Result<()> {
    if options.node.is_some() {
        context.mcp_automatic_supported = false;
        return preflight(context, options, executable).await;
    }
    tokio::task::spawn_blocking(move || {
        options.validate()?;
        mcp_config::preflight_paths(
            context.tool_type,
            &context.project,
            &runtime_target(&context, &options),
            &credentials::path(&context.config_path),
            &runtime_entry(&options, &executable)?,
        )
    })
    .await
    .map_err(|_| error("SETUP_INTERNAL_ERROR"))?
}

/// ACL/path validation precedes subprocess execution. Runtime discovery only initializes
/// a credential-free MCP process: no business tool, claim, bind or AI Host is called.
pub async fn prepare(
    context: Context,
    options: Options,
    executable: PathBuf,
) -> Result<Preparation> {
    let mut prepared = tokio::task::spawn_blocking(move || {
        let _guards = check_context(&context)?;
        let input = runtime_input(&PreflightContext::from(&context), &options, &executable)?;
        let manual = mcp_config::manual_config(input.clone())?;
        let fingerprint = fingerprints(&context, &input)?;
        let provider = if options.mode == Mode::Apply {
            context.tool.verified_host().ok().map(McpConfig::new)
        } else {
            None
        };
        Ok(Preparation {
            context,
            input,
            provider,
            plan: None,
            manual,
            fingerprint,
        })
    })
    .await
    .map_err(|_| error("SETUP_INTERNAL_ERROR"))??;
    probe_runtime(&prepared.input.executable).await?;
    tokio::task::spawn_blocking(move || {
        let _guards = check_context(&prepared.context)?;
        if fingerprints(&prepared.context, &prepared.input)? != prepared.fingerprint {
            return Err(error("SETUP_PROFILE_CONFLICT"));
        }
        if let Some(provider) = &mut prepared.provider {
            prepared.plan = Some(provider.plan(prepared.input.clone())?);
        }
        Ok(prepared)
    })
    .await
    .map_err(|_| error("SETUP_INTERNAL_ERROR"))?
}

async fn bounded(stream: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stream.take(16_385).read_to_end(&mut bytes).await?;
    if bytes.len() > 16_384 {
        return Err(std::io::Error::other("Probe output limit"));
    }
    Ok(bytes)
}
fn command(executable: &Executable) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(&executable.command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_some_and(|s| {
            let name = s.to_ascii_uppercase();
            name.starts_with("DEVLINK_") || ["NODE_OPTIONS", "NODE_PATH"].contains(&name.as_str())
        }) {
            command.env_remove(key);
        }
    }
    #[cfg(windows)]
    {
        command.creation_flags(0x08000000);
    }
    command
}
/// Termination may be delayed by the OS or security software, so the reap after a
/// failed probe is bounded as well as the probe itself. kill_on_drop stays as a backstop.
/// If the bound passes, a child still exiting can keep the probe's temporary working
/// directory in use on Windows; that empty directory is then left behind.
async fn terminate(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(crate::tool_discovery::REAP_TIMEOUT, child.wait()).await;
}
async fn probe_runtime(executable: &Executable) -> Result<()> {
    let cwd = tempfile::tempdir().map_err(|_| error("SETUP_TOOL_UNAVAILABLE"))?;
    let mut version = command(executable)
        .arg("--version")
        .current_dir(cwd.path())
        .spawn()
        .map_err(|_| error("SETUP_TOOL_UNAVAILABLE"))?;
    drop(version.stdin.take());
    let stdout = version
        .stdout
        .take()
        .ok_or_else(|| error("SETUP_TOOL_UNAVAILABLE"))?;
    let stderr = version
        .stderr
        .take()
        .ok_or_else(|| error("SETUP_TOOL_UNAVAILABLE"))?;
    let checked = tokio::time::timeout(PROBE_TIMEOUT, async {
        let (out, err) = tokio::join!(bounded(stdout), bounded(stderr));
        let status = version.wait().await?;
        Ok::<_, std::io::Error>((status, out?, err?))
    })
    .await;
    let valid = match checked {
        Ok(Ok((status, out, _))) if status.success() => std::str::from_utf8(&out)
            .ok()
            .and_then(|s| {
                let value = s.trim().strip_prefix('v')?;
                let parts: Vec<_> = value.split('.').collect();
                (parts.len() == 3
                    && parts
                        .iter()
                        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
                .then(|| parts[0].parse::<u32>().ok().is_some_and(|n| n >= 24))
            })
            .unwrap_or(false),
        _ => false,
    };
    if !valid {
        terminate(&mut version).await;
        return Err(error("SETUP_TOOL_UNAVAILABLE"));
    }
    let mut child = command(executable)
        .args([
            mcp_config::host_path(Path::new(&executable.args[0])),
            "--transport".into(),
            "stdio".into(),
        ])
        .current_dir(cwd.path())
        .spawn()
        .map_err(|_| error("SETUP_TOOL_UNAVAILABLE"))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| error("SETUP_TOOL_UNAVAILABLE"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| error("SETUP_TOOL_UNAVAILABLE"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| error("SETUP_TOOL_UNAVAILABLE"))?;
    let checked = tokio::time::timeout(PROBE_TIMEOUT, async {
        let read_error = bounded(stderr);
        let run = async {
            stdin.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"devlink-setup-probe\",\"version\":\"0.1.0\"}}}\n").await?;
            let mut response = Vec::new();
            BufReader::new(stdout.take(16_385)).read_until(b'\n', &mut response).await?;
            if response.len() > 16_384 { return Err(std::io::Error::other("Probe output limit")); }
            let value: serde_json::Value = serde_json::from_slice(&response).map_err(std::io::Error::other)?;
            let valid = value["jsonrpc"] == "2.0" && value["id"] == 1 && value.get("error").is_none()
                && value["result"]["serverInfo"]["name"] == "devlink-mcp"
                && value["result"]["serverInfo"]["version"].as_str().is_some_and(|s| !s.is_empty())
                && value["result"]["capabilities"]["tools"].is_object();
            drop(stdin);
            let status = child.wait().await?;
            Ok::<_, std::io::Error>(valid && status.success())
        };
        let (valid, err) = tokio::join!(run, read_error);
        Ok::<_, std::io::Error>(valid? && err.is_ok())
    }).await;
    if !matches!(checked, Ok(Ok(true))) {
        terminate(&mut child).await;
        return Err(error("SETUP_TOOL_UNAVAILABLE"));
    }
    Ok(())
}
