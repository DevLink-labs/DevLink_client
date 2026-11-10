//! Explicit, recoverable Agent bootstrap. Server identity is the authority.
pub mod registration;
pub mod tokens;
use crate::{
    agent_metadata::{AgentPart, normalize_roles},
    config::{self, Config},
    credentials, private_file, teams,
    user_auth::{self, UserClient},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    time::Duration,
};

const LIMIT: usize = 16_384;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Tool {
    Codex,
    ClaudeCode,
}
impl Tool {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude-code" => Ok(Self::ClaudeCode),
            _ => bail!("Choose codex or claude-code"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub agent_id: u64,
    pub team_id: u64,
    pub owner_user_id: u64,
    pub name: String,
    pub role_description: Option<String>,
    #[serde(default)]
    pub part: AgentPart,
    #[serde(default)]
    pub roles: Vec<String>,
    pub tool_type: Tool,
    pub project_name: String,
    pub status: String,
    pub last_seen_at: Option<String>,
    pub created_at: String,
}
impl Agent {
    fn owned(&self, user: u64, team: u64) -> Result<()> {
        ensure!(
            valid_id(self.agent_id) && self.owner_user_id == user && self.team_id == team,
            "ACCESS_DENIED: Agent is not owned by the authenticated user in the selected team"
        );
        Ok(())
    }
}
pub struct CreateOptions {
    pub name: String,
    pub role_description: Option<String>,
    pub part: AgentPart,
    pub roles: Vec<String>,
    pub tool: Tool,
    pub project_name: String,
    pub project: PathBuf,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum State {
    Vacant,
    CreatePending {
        name: String,
        tool: Tool,
        project_name: String,
    },
    NeedsToken {
        agent: Agent,
    },
    Ready {
        agent: Agent,
        token: String,
    },
}
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Profile {
    schema_version: u32,
    origin: String,
    user_id: u64,
    team_id: u64,
    project_root: String,
    state: State,
}
impl Profile {
    fn agent(&self) -> Option<&Agent> {
        match &self.state {
            State::Ready { agent, .. } | State::NeedsToken { agent } => Some(agent),
            _ => None,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && valid_id(self.user_id)
                && valid_id(self.team_id)
                && Path::new(&self.project_root).is_absolute(),
            "Invalid Agent profile"
        );
        if let Some(agent) = self.agent() {
            agent.owned(self.user_id, self.team_id)?;
        }
        if let State::Ready { token, .. } = &self.state {
            ensure!(valid_token(token), "Invalid private Agent credential");
        }
        Ok(())
    }
    fn summary(&self, reused: bool) -> Value {
        json!({"agent":self.agent(),"projectRoot":self.project_root,"reused":reused,"state":match self.state{State::Vacant=>"vacant",State::CreatePending{..}=>"creation_unknown",State::NeedsToken{..}=>"token_required",State::Ready{..}=>"ready"}})
    }
}

pub fn path(config_path: &Path) -> PathBuf {
    config::state_directory(config_path).join("agent-profile.json")
}

/// Local display filter only. This is not an authenticated business principal.
pub fn bound_agent_id(config_path: &Path) -> Result<Option<u64>> {
    let Some(profile) = load(config_path)? else {
        return Ok(None);
    };
    if profile.origin != user_auth::origin(&Config::load(config_path)?)?.as_str() {
        return Ok(None);
    }
    Ok(profile.agent().map(|agent| agent.agent_id))
}

/// Caller holds account/control locks. Pending creation also binds a principal.
pub(crate) fn setup_team_conflicts(
    config_path: &Path,
    origin: &str,
    user: u64,
    team: u64,
) -> Result<bool> {
    Ok(load(config_path)?.is_some_and(|profile| {
        !matches!(profile.state, State::Vacant)
            && (profile.origin != origin || profile.user_id != user || profile.team_id != team)
    }))
}

/// Display provenance only. A live central identity must validate these selectors.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InspectionBinding {
    pub origin: String,
    pub user_id: u64,
    pub team_id: u64,
    pub agent_id: u64,
    pub project_root: String,
    pub tool: crate::sessions::ToolType,
}
pub(crate) fn inspection_binding(
    config_path: &Path,
) -> Result<Option<InspectionBinding>, crate::sessions::IdentityError> {
    use crate::sessions::IdentityError::{Forbidden, Unavailable};
    let Some(profile) = load(config_path).map_err(|_| Unavailable)? else {
        return Ok(None);
    };
    let selected = teams::stored_selection(config_path)
        .map_err(|_| Unavailable)?
        .ok_or(Unavailable)?;
    let config = Config::load(config_path).map_err(|_| Unavailable)?;
    let origin = user_auth::origin(&config).map_err(|_| Unavailable)?;
    let root = std::fs::canonicalize(&profile.project_root).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Forbidden
        } else {
            Unavailable
        }
    })?;
    let metadata = std::fs::metadata(&root).map_err(|_| Unavailable)?;
    if profile.origin != origin.as_str()
        || selected.origin != profile.origin
        || selected.user_id != profile.user_id
        || selected.team_id != profile.team_id
        || !metadata.is_dir()
        || root.to_str() != Some(profile.project_root.as_str())
    {
        return Err(Forbidden);
    }
    let agent = profile.agent().ok_or(Unavailable)?;
    let agent_id = agent.agent_id;
    let tool = match agent.tool_type {
        Tool::Codex => crate::sessions::ToolType::Codex,
        Tool::ClaudeCode => crate::sessions::ToolType::ClaudeCode,
    };
    Ok(Some(InspectionBinding {
        origin: profile.origin,
        user_id: profile.user_id,
        team_id: profile.team_id,
        agent_id,
        project_root: profile.project_root,
        tool,
    }))
}
fn valid_id(id: u64) -> bool {
    id > 0 && id <= i64::MAX as u64
}
fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 8192
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._~-".contains(&b))
}
pub(crate) fn project(path: &Path) -> Result<String> {
    let canonical = path
        .canonicalize()
        .map_err(|_| anyhow::anyhow!("Local project directory does not exist"))?;
    ensure!(canonical.is_dir(), "Local project must be a directory");
    canonical
        .to_str()
        .map(str::to_owned)
        .context("Local project path must be Unicode")
}
fn load(config_path: &Path) -> Result<Option<Profile>> {
    let value = private_file::read::<Profile>(&path(config_path), LIMIT)?;
    if let Some(p) = &value {
        p.validate()?;
    }
    Ok(value)
}

pub(crate) struct ProjectBinding {
    pub root: String,
    pub metadata: Option<crate::setup_project::AgentMetadata>,
    pub tool: Option<crate::sessions::ToolType>,
    pub origin: String,
    pub user_id: u64,
    pub team_id: u64,
}

/// Local comparison only; never exposes the private state or Agent token.
pub(crate) fn setup_project_binding(config_path: &Path) -> Result<Option<ProjectBinding>> {
    use crate::setup_project::AgentMetadata;
    Ok(load(config_path)?.map(|profile| {
        let metadata = match &profile.state {
            State::Ready { agent, .. } | State::NeedsToken { agent } => Some(AgentMetadata {
                project_name: agent.project_name.clone(),
                agent_name: agent.name.clone(),
                role_description: agent.role_description.clone(),
                part: agent.part,
                roles: agent.roles.clone(),
            }),
            State::CreatePending {
                name, project_name, ..
            } => Some(AgentMetadata {
                project_name: project_name.clone(),
                agent_name: name.clone(),
                role_description: None,
                part: AgentPart::default(),
                roles: Vec::new(),
            }),
            State::Vacant => None,
        };
        ProjectBinding {
            root: profile.project_root,
            metadata,
            tool: match &profile.state {
                State::Ready { agent, .. } | State::NeedsToken { agent } => Some(&agent.tool_type),
                State::CreatePending { tool, .. } => Some(tool),
                State::Vacant => None,
            }
            .map(|tool| match tool {
                Tool::Codex => crate::sessions::ToolType::Codex,
                Tool::ClaudeCode => crate::sessions::ToolType::ClaudeCode,
            }),
            origin: profile.origin,
            user_id: profile.user_id,
            team_id: profile.team_id,
        }
    }))
}
fn load_for_explicit_use(config_path: &Path) -> Result<Option<Profile>> {
    let Some(bytes) = private_file::read_bytes(&path(config_path), LIMIT)? else {
        return Ok(None);
    };
    Ok(serde_json::from_slice::<Profile>(&bytes)
        .ok()
        .filter(|p| p.validate().is_ok()))
}
fn save(config_path: &Path, profile: &Profile) -> Result<()> {
    profile.validate()?;
    private_file::write(&path(config_path), profile, LIMIT)
}

fn stopped(config_path: &Path) -> Result<crate::broker_control::StoppedGuard> {
    use crate::broker_control::{self, Failure, FailureKind};
    broker_control::require_stopped(config_path).map_err(|error| {
        let kind = match error.downcast_ref::<Failure>().map(|f| f.kind) {
            Some(FailureKind::ControlLockFailed) => user_auth::AuthFaultKind::ControlLockFailed,
            Some(FailureKind::ConfigBusy) => user_auth::AuthFaultKind::ConfigBusy,
            Some(FailureKind::BrokerLockFailed) => user_auth::AuthFaultKind::BrokerLockFailed,
            Some(FailureKind::BrokerBusy) => user_auth::AuthFaultKind::BrokerBusy,
            _ => return error,
        };
        user_auth::classify(error, kind)
    })
}

pub struct OwnerGuard {
    _legacy: File,
    _agent: Option<File>,
}

/// Agent별 중복 실행·토큰 변경 방지. 공유 잠금으로 구버전의 계정 단위 잠금과 호환.
fn owner_lock(origin: &str, user: u64, agent: Option<u64>) -> Result<OwnerGuard> {
    try_owner_lock(origin, user, agent)?
        .context("Another broker or setup operation is active for this Agent on this machine")
}

fn try_owner_lock(origin: &str, user: u64, agent: Option<u64>) -> Result<Option<OwnerGuard>> {
    let digest = Sha256::digest(format!("{origin}\0{user}"));
    let directory = dirs::data_local_dir()
        .context("Cannot locate private broker owner directory")?
        .join("DevLink")
        .join("broker-owners");
    credentials::ensure_safe_parent_directory(&directory)?;
    credentials::set_private_permissions(&directory)?;
    let lock = directory.join(format!("{digest:x}.lock"));
    if std::fs::symlink_metadata(&lock).is_ok() {
        credentials::verify_path_safety(&lock)?;
    }
    let legacy = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock)?;
    match fs2::FileExt::try_lock_shared(&legacy) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
            return Ok(None);
        }
        Err(error) => return Err(error.into()),
    }
    let agent = if let Some(id) = agent {
        let lock = directory.join(format!("{digest:x}.agent-{id}.lock"));
        let Some(guard) = config::try_lock(&lock)? else {
            return Ok(None);
        };
        Some(guard)
    } else {
        None
    };
    Ok(Some(OwnerGuard {
        _legacy: legacy,
        _agent: agent,
    }))
}

fn rotate_mcp_if_present(config_path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(credentials::path(config_path)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => bail!("Cannot inspect existing MCP credentials"),
        Ok(_) => {}
    }
    {
        let mut value = credentials::load(config_path)?;
        value.local_token = credentials::generate_token();
        credentials::write_atomic_private(&credentials::path(config_path), &value)?;
    }
    Ok(())
}

async fn context(origin: &str, token: &str) -> Result<Agent> {
    tokens::verify(origin, token).await.map_err(Into::into)
}

async fn owned(client: &mut UserClient, team: u64, id: u64) -> Result<Agent> {
    ensure!(valid_id(id), "Invalid Agent ID");
    let agent: Agent = client.get(&format!("/api/v1/agents/{id}")).await?;
    agent.owned(client.user()?.user_id, team)?;
    ensure!(agent.agent_id == id, "Central Agent identity mismatch");
    Ok(agent)
}

async fn issue(
    config_path: &Path,
    client: &mut UserClient,
    profile: &mut Profile,
    agent: Agent,
) -> Result<Value> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Issued {
        agent_id: u64,
        token: String,
    }
    profile.state = State::NeedsToken {
        agent: agent.clone(),
    };
    save(config_path, profile)?;
    let issued: Issued = client
        .post(
            &format!("/api/v1/agents/{}/token", agent.agent_id),
            json!({}),
        )
        .await?;
    ensure!(
        issued.agent_id == agent.agent_id,
        "Issued Agent identity mismatch"
    );
    let verified = context(&profile.origin, &issued.token).await?;
    verified.owned(profile.user_id, profile.team_id)?;
    ensure!(
        verified.agent_id == agent.agent_id,
        "Agent credential belongs to another Agent"
    );
    rotate_mcp_if_present(config_path)?;
    profile.state = State::Ready {
        agent: verified,
        token: issued.token,
    };
    save(config_path, profile)?;
    Ok(profile.summary(false))
}

pub async fn list(config_path: &Path) -> Result<Vec<Agent>> {
    let mut client = UserClient::authenticated(config_path).await?;
    let team = teams::require_selected(config_path, &mut client).await?;
    let agents: Vec<Agent> = client
        .get(&format!("/api/v1/agents/teams/{}", team.team_id))
        .await?;
    ensure!(
        agents
            .iter()
            .all(|a| valid_id(a.agent_id) && a.team_id == team.team_id),
        "Invalid central Agent list"
    );
    Ok(agents)
}

pub async fn create(config_path: &Path, mut options: CreateOptions) -> Result<Value> {
    options.roles = normalize_roles(&options.roles)?;
    ensure!(
        !options.name.trim().is_empty()
            && options.name.encode_utf16().count() <= 100
            && !options.project_name.trim().is_empty()
            && options.project_name.encode_utf16().count() <= 120
            && options
                .role_description
                .as_ref()
                .is_none_or(|s| s.encode_utf16().count() <= 255),
        "Invalid Agent metadata"
    );
    let mut client = UserClient::authenticated(config_path).await?;
    let team = teams::require_selected(config_path, &mut client).await?;
    let root = project(&options.project)?;
    let user = client.user()?.user_id;
    if let Some(existing) = load(config_path)?
        && !matches!(existing.state, State::Vacant)
    {
        ensure!(
            existing.origin == client.central_origin()
                && existing.user_id == user
                && existing.team_id == team.team_id
                && existing.project_root == root,
            "Profile already exists; use an explicit owned Agent ID or another profile"
        );
        let Some(agent) = existing.agent() else {
            bail!(
                "AGENT_CREATION_UNKNOWN: inspect agent list, then use an explicit owned ID; creation is not retried"
            );
        };
        ensure!(
            agent.name == options.name
                && agent.tool_type == options.tool
                && agent.project_name == options.project_name
                && agent.role_description == options.role_description
                && agent.part == options.part
                && agent.roles == options.roles,
            "Existing profile metadata differs; no Agent was created"
        );
        owned(&mut client, team.team_id, agent.agent_id).await?;
        if let State::Ready { token, .. } = &existing.state {
            let verified = context(&existing.origin, token).await?;
            verified.owned(user, team.team_id)?;
            ensure!(
                verified.agent_id == agent.agent_id,
                "Agent token identity mismatch"
            );
        }
        return Ok(existing.summary(true));
    }
    let _control = stopped(config_path)?;
    let _owner = owner_lock(client.central_origin(), user, None)?;
    let prepared = client.prepare_post().await?;
    let mut profile = Profile {
        schema_version: 1,
        origin: client.central_origin().to_owned(),
        user_id: user,
        team_id: team.team_id,
        project_root: root,
        state: State::CreatePending {
            name: options.name.clone(),
            tool: options.tool.clone(),
            project_name: options.project_name.clone(),
        },
    };
    save(config_path, &profile)?; // Crash/unknown HTTP outcome must never trigger another creation.
    let result=client.post_prepared::<Agent>(&format!("/api/v1/agents/teams/{}",team.team_id),json!({"name":options.name,"roleDescription":options.role_description,"toolType":options.tool,"projectName":options.project_name,"part":options.part,"roles":options.roles}),prepared).await;
    let agent = match result {
        Ok(agent) => agent,
        Err(error) => {
            if user_auth::rejection_status(&error).is_some_and(|s| (400..500).contains(&s)) {
                profile.state = State::Vacant;
                save(config_path, &profile)?;
            }
            bail!(
                "Agent creation not confirmed ({error}); do not retry blindly, inspect agent list"
            );
        }
    };
    agent.owned(user, team.team_id)?;
    ensure!(
        agent.part == options.part && agent.roles == options.roles,
        "Agent creation metadata not confirmed; inspect agent list before retrying"
    );
    profile.state = State::NeedsToken {
        agent: agent.clone(),
    };
    save(config_path,&profile).map_err(|_|anyhow::anyhow!("Agent {} created; local save failed. Recover with explicit agent use, never create again blindly",agent.agent_id))?;
    let _agent_owner = owner_lock(&profile.origin, profile.user_id, Some(agent.agent_id))?;
    issue(config_path, &mut client, &mut profile, agent).await
}

pub async fn use_agent(config_path: &Path, id: u64, local_project: Option<&Path>) -> Result<Value> {
    let mut client = UserClient::authenticated(config_path).await?;
    let team = teams::require_selected(config_path, &mut client).await?;
    let agent = owned(&mut client, team.team_id, id).await?;
    let existing = load_for_explicit_use(config_path)?;
    let root = match local_project {
        Some(p) => project(p)?,
        None => existing
            .as_ref()
            .filter(|p| {
                p.origin == client.central_origin()
                    && p.user_id == client.user().map(|u| u.user_id).unwrap_or(0)
                    && p.team_id == team.team_id
                    && p.agent().is_some_and(|a| a.agent_id == id)
            })
            .map(|p| p.project_root.clone())
            .context("Select a local project explicitly with --project")?,
    };
    if let Some(ref old) = existing
        && old.origin == client.central_origin()
        && old.user_id == client.user()?.user_id
        && old.team_id == team.team_id
        && old.project_root == root
        && old.agent().is_some_and(|a| a.agent_id == id)
        && let State::Ready { token, .. } = &old.state
    {
        let verified = context(&old.origin, token).await?;
        verified.owned(old.user_id, old.team_id)?;
        ensure!(verified.agent_id == id, "Agent token mismatch");
        return Ok(old.summary(true));
    }
    let _control = stopped(config_path)?;
    let _owner = owner_lock(client.central_origin(), client.user()?.user_id, Some(id))?;
    let profile = Profile {
        schema_version: 1,
        origin: client.central_origin().to_owned(),
        user_id: client.user()?.user_id,
        team_id: team.team_id,
        project_root: root,
        state: State::NeedsToken { agent },
    };
    rotate_mcp_if_present(config_path)?;
    save(config_path, &profile)?;
    Ok(profile.summary(false))
}

pub async fn token(config_path: &Path) -> Result<Value> {
    let mut client = UserClient::authenticated(config_path).await?;
    let team = teams::require_selected(config_path, &mut client).await?;
    let mut profile = load(config_path)?.context("Select an owned Agent first")?;
    ensure!(
        profile.origin == client.central_origin()
            && profile.user_id == client.user()?.user_id
            && profile.team_id == team.team_id,
        "Selected team/user differs from bound Agent; use an explicit owned ID"
    );
    let id = profile
        .agent()
        .context("Unknown Agent creation; inspect list and use explicit ID")?
        .agent_id;
    let agent = owned(&mut client, team.team_id, id).await?;
    let _control = stopped(config_path)?;
    let _owner = owner_lock(client.central_origin(), client.user()?.user_id, Some(id))?;
    issue(config_path, &mut client, &mut profile, agent).await
}

/// S15P31D108-146/-183: the only Agent fields a bound profile may change. Name, tool,
/// owner and team stay fixed, and no Agent or credential is created.
#[derive(Default)]
pub struct UpdateOptions {
    pub project_name: Option<String>,
    pub role_description: Option<String>,
    /// `None` keeps the stored part.
    pub part: Option<AgentPart>,
    /// `None` keeps the stored tags; an empty list removes all of them.
    pub roles: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateFaultKind {
    /// Local validation failed; nothing was sent.
    Invalid,
    /// The bound profile is missing, unknown, or belongs to another principal/Agent.
    Binding,
    /// The update was sent but the response did not confirm the requested state.
    Unconfirmed,
}
/// Typed, value-free update failure so callers can classify it without parsing text.
#[derive(Debug)]
pub struct UpdateFault {
    pub kind: UpdateFaultKind,
    message: &'static str,
}
impl std::fmt::Display for UpdateFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for UpdateFault {}
fn update_fault(kind: UpdateFaultKind, message: &'static str) -> anyhow::Error {
    UpdateFault { kind, message }.into()
}

pub struct UpdateResult {
    pub agent: Agent,
    pub changed: Vec<&'static str>,
    /// False when the central update succeeded but the local display cache was not rewritten.
    pub local_profile_synced: bool,
}
impl UpdateResult {
    pub fn summary(&self) -> Value {
        json!({"agent":self.agent,"updated":!self.changed.is_empty(),"changedFields":self.changed,
            "credentialsRotated":false,"localProfileSynced":self.local_profile_synced})
    }
}

/// Central `isBlank` treats some control characters as blank, so they are rejected here.
fn valid_text(value: &str, limit: usize) -> bool {
    !value.trim().is_empty()
        && value.encode_utf16().count() <= limit
        && !value.chars().any(char::is_control)
}

/// Change the bound Agent's project name, role, part or role tags through the central
/// owner session. Only fields that differ from the live central value are sent. `expected_agent`
/// pins the caller's verified Agent (the broker session) to the bound profile.
pub async fn update(
    config_path: &Path,
    options: UpdateOptions,
    expected_agent: Option<u64>,
) -> Result<UpdateResult> {
    prepare_update(config_path, options, expected_agent)
        .await?
        .commit()
        .await
}

/// Holds the account lock and prepared CSRF privately until the caller has fenced
/// its MCP session. No Debug/Serialize implementation may expose these credentials.
pub(crate) struct PreparedUpdate {
    config_path: PathBuf,
    client: UserClient,
    profile: Profile,
    current: Agent,
    options: UpdateOptions,
    body: serde_json::Map<String, Value>,
    changed: Vec<&'static str>,
    csrf: Option<user_auth::PreparedPost>,
}

pub(crate) async fn prepare_update(
    config_path: &Path,
    mut options: UpdateOptions,
    expected_agent: Option<u64>,
) -> Result<PreparedUpdate> {
    use UpdateFaultKind::{Binding, Invalid};
    // Same trimming and de-duplication as creation, so the comparison with the
    // central value and the confirmation of the reply use what central stores.
    let roles = options.roles.as_deref().map(normalize_roles).transpose();
    if (options.project_name.is_none()
        && options.role_description.is_none()
        && options.part.is_none()
        && options.roles.is_none())
        || options
            .project_name
            .as_deref()
            .is_some_and(|v| !valid_text(v, 120))
        || options
            .role_description
            .as_deref()
            .is_some_and(|v| !valid_text(v, 255))
        // A tag that trims to nothing is a central 400, and a control character such
        // as NUL inside a tag may fail in central storage with an unknown outcome.
        || roles.as_ref().map_or(true, |roles| {
            roles.iter().flatten().any(|tag| tag.is_empty() || tag.chars().any(char::is_control))
        })
    {
        return Err(update_fault(
            Invalid,
            "Invalid Agent update: pass a non-blank --project-name (up to 120) or --role-description (up to 255), --part, or at most 10 non-blank --roles tags (each up to 64, no control characters)",
        ));
    }
    options.roles = roles.unwrap_or_default();
    let mut client = UserClient::authenticated(config_path).await?;
    let team = teams::require_selected(config_path, &mut client).await?;
    let user = client.user()?.user_id;
    let profile = load(config_path)?.ok_or_else(|| {
        update_fault(
            Binding,
            "AGENT_PROFILE_REQUIRED: bind an owned Agent first; no Agent was updated",
        )
    })?;
    if profile.origin != client.central_origin()
        || profile.user_id != user
        || profile.team_id != team.team_id
    {
        return Err(update_fault(
            Binding,
            "Selected team/user differs from bound Agent; no Agent was updated",
        ));
    }
    let Some(bound) = profile.agent() else {
        return Err(update_fault(
            Binding,
            "AGENT_CREATION_UNKNOWN: inspect agent list and use an explicit owned ID; no Agent was updated",
        ));
    };
    let id = bound.agent_id;
    if expected_agent.is_some_and(|expected| expected != id) {
        return Err(update_fault(
            Binding,
            "The session Agent differs from the bound profile; no Agent was updated",
        ));
    }
    let current = owned(&mut client, team.team_id, id).await?;
    let mut body = serde_json::Map::new();
    let mut changed = Vec::new();
    if let Some(project) = &options.project_name
        && *project != current.project_name
    {
        body.insert("projectName".into(), json!(project));
        changed.push("projectName");
    }
    if let Some(role) = &options.role_description
        && current.role_description.as_ref() != Some(role)
    {
        body.insert("roleDescription".into(), json!(role));
        changed.push("roleDescription");
    }
    if let Some(part) = options.part
        && part != current.part
    {
        body.insert("part".into(), json!(part));
        changed.push("part");
    }
    // Central replaces the whole list; `[]` removes every tag.
    if let Some(roles) = &options.roles
        && *roles != current.roles
    {
        body.insert("roles".into(), json!(roles));
        changed.push("roles");
    }
    let csrf = if changed.is_empty() {
        None
    } else {
        Some(client.prepare_patch().await?)
    };
    Ok(PreparedUpdate {
        config_path: config_path.to_owned(),
        client,
        profile,
        current,
        options,
        body,
        changed,
        csrf,
    })
}

impl PreparedUpdate {
    /// Dispatch immediately after the caller's final authorization. Once sent,
    /// preserve a confirmed result even if that session is subsequently replaced.
    pub(crate) async fn commit(mut self) -> Result<UpdateResult> {
        let id = self.current.agent_id;
        let agent = if let Some(csrf) = self.csrf {
            let updated: Agent = self.client
            .patch_prepared(&format!("/api/v1/agents/{id}"), Value::Object(self.body), csrf)
            .await
            .map_err(|error| {
                if user_auth::rejection_status(&error).is_some_and(|s| (400..500).contains(&s)) {
                    error
                } else {
                    error.context(
                        "Agent update not confirmed; the outcome is unknown. Inspect agent status before retrying",
                    )
                }
            })?;
            let confirmed = updated
                .owned(self.profile.user_id, self.profile.team_id)
                .is_ok()
                && updated.agent_id == id
                && updated.name == self.current.name
                && updated.tool_type == self.current.tool_type
                && self
                    .options
                    .project_name
                    .as_ref()
                    .is_none_or(|p| *p == updated.project_name)
                && self
                    .options
                    .role_description
                    .as_ref()
                    .is_none_or(|r| updated.role_description.as_ref() == Some(r))
                && self.options.part.is_none_or(|p| p == updated.part)
                && self
                    .options
                    .roles
                    .as_ref()
                    .is_none_or(|r| *r == updated.roles);
            if !confirmed {
                return Err(update_fault(
                    UpdateFaultKind::Unconfirmed,
                    "Central Agent update response does not match the request; the outcome is unknown. Inspect agent status before retrying",
                ));
            }
            updated
        } else {
            self.current
        };
        let local_profile_synced = sync_cached_agent(&self.config_path, &self.profile, &agent);
        Ok(UpdateResult {
            agent,
            changed: self.changed,
            local_profile_synced,
        })
    }
}

/// Only the four editable fields are cached: identity, root, token and volatile
/// status stay, so an unchanged value never rewrites the profile. A profile changed
/// by another operation is left alone and reported as unsynced.
fn sync_cached_agent(config_path: &Path, before: &Profile, agent: &Agent) -> bool {
    let Ok(Some(mut current)) = load(config_path) else {
        return false;
    };
    if current != *before {
        return false;
    }
    match &mut current.state {
        State::Ready { agent: cached, .. } | State::NeedsToken { agent: cached } => {
            if cached.project_name == agent.project_name
                && cached.role_description == agent.role_description
                && cached.part == agent.part
                && cached.roles == agent.roles
            {
                return true;
            }
            cached.project_name = agent.project_name.clone();
            cached.role_description = agent.role_description.clone();
            cached.part = agent.part;
            cached.roles = agent.roles.clone();
        }
        _ => return false,
    }
    save(config_path, &current).is_ok()
}

pub async fn status(config_path: &Path) -> Result<Value> {
    let Some(profile) = load(config_path)? else {
        return Ok(json!({"state":"unconfigured"}));
    };
    ensure!(
        profile.origin == user_auth::origin(&Config::load(config_path)?)?.as_str(),
        "Agent profile belongs to another central server"
    );
    let mut summary = profile.summary(true);
    if let State::Ready { agent, token } = &profile.state {
        let verified = context(&profile.origin, token).await?;
        verified.owned(profile.user_id, profile.team_id)?;
        ensure!(
            verified.agent_id == agent.agent_id,
            "Profile Agent verification mismatch"
        );
        summary["verified"] = json!(true);
    }
    Ok(summary)
}

/// Explicit acknowledgement only: never creates or retries a central request.
pub async fn reset_pending(config_path: &Path, accept_duplicate_risk: bool) -> Result<Value> {
    ensure!(
        accept_duplicate_risk,
        "Inspect agent list first, then explicitly pass --accept-duplicate-risk; a delayed creation may still exist"
    );
    let mut client = UserClient::authenticated(config_path).await?;
    let team = teams::require_selected(config_path, &mut client).await?;
    let mut profile = load(config_path)?.context("No pending creation exists")?;
    ensure!(
        profile.origin == client.central_origin()
            && profile.user_id == client.user()?.user_id
            && profile.team_id == team.team_id,
        "Pending profile belongs to another server/user/team"
    );
    ensure!(
        matches!(profile.state, State::CreatePending { .. }),
        "Only an unknown creation can be reset; known Agent IDs must be reused"
    );
    let _control = stopped(config_path)?;
    let _owner = owner_lock(client.central_origin(), client.user()?.user_id, None)?;
    profile.state = State::Vacant;
    save(config_path, &profile)?;
    Ok(
        json!({"state":"vacant","creationRetried":false,"warning":"An unknown or delayed server creation may still exist; prefer an explicit owned ID"}),
    )
}

pub fn before_login_publish(config_path: &Path, user: u64) -> Result<()> {
    let Some(mut profile) = load_for_explicit_use(config_path)? else {
        return Ok(());
    };
    let current = user_auth::config_origin(config_path)?;
    if profile.user_id != user || profile.origin != current.as_str() {
        let _control = stopped(config_path)?;
        if let Some(agent) = profile.agent().cloned() {
            profile.state = State::NeedsToken { agent };
            save(config_path, &profile)?;
        }
    }
    Ok(())
}

pub fn stored_token(config_path: &Path) -> Result<Option<String>> {
    let Some(profile) = load(config_path)? else {
        return Ok(None);
    };
    ensure!(
        profile.origin == user_auth::origin(&Config::load(config_path)?)?.as_str(),
        "Agent profile belongs to another central server; explicitly bind an owned Agent"
    );
    let State::Ready { token, .. } = profile.state else {
        bail!("Agent profile is not ready; resolve creation or issue a token explicitly");
    };
    if let Some(environment) = std::env::var_os("DEVLINK_TOKEN") {
        ensure!(
            environment == std::ffi::OsStr::new(&token),
            "DEVLINK_TOKEN conflicts with the bound Agent profile; remove the override"
        );
    }
    Ok(Some(token))
}

pub struct BrokerIdentity {
    pub token: String,
    pub _owner: OwnerGuard,
}
pub fn startup_allowance(config_path: &Path) -> Duration {
    if std::fs::symlink_metadata(path(config_path)).is_ok() {
        credentials::ACL_CHECK_TIMEOUT * 5 + Duration::from_secs(5)
    } else {
        Duration::ZERO
    }
}
pub async fn prepare_broker(config_path: &Path) -> Result<Option<BrokerIdentity>> {
    let Some(profile) = load(config_path)? else {
        return Ok(None);
    };
    let token = stored_token(config_path)?.context("Agent profile has no token")?;
    ensure!(
        project(Path::new(&profile.project_root))? == profile.project_root,
        "Bound project directory changed; explicitly rebind its current path"
    );
    let selected = teams::stored_selection(config_path)?
        .context("Selected team missing; reselect the bound team")?;
    ensure!(
        selected.origin == profile.origin
            && selected.user_id == profile.user_id
            && selected.team_id == profile.team_id,
        "Selected team differs from bound Agent; stop and explicitly bind an owned Agent"
    );
    let agent = context(&profile.origin, &token).await?;
    agent.owned(profile.user_id, profile.team_id)?;
    ensure!(
        profile
            .agent()
            .is_some_and(|a| a.agent_id == agent.agent_id),
        "Agent profile token identity mismatch"
    );
    let owner = owner_lock(&profile.origin, profile.user_id, Some(agent.agent_id))?;
    ensure!(
        load(config_path)?.as_ref() == Some(&profile),
        "Agent profile changed during startup; retry explicitly"
    );
    Ok(Some(BrokerIdentity {
        token,
        _owner: owner,
    }))
}

#[cfg(test)]
mod owner_tests {
    use super::*;

    #[test]
    fn agent_guards_coexist_but_exclude_same_agent_and_legacy_exclusive_guard() {
        let origin = format!("http://lock-test-{}.invalid/", uuid::Uuid::new_v4());
        let first = owner_lock(&origin, 51, Some(201)).unwrap();
        let second = owner_lock(&origin, 51, Some(301)).unwrap();
        assert!(try_owner_lock(&origin, 51, Some(201)).unwrap().is_none());
        let digest = Sha256::digest(format!("{origin}\0{}", 51));
        let directory = dirs::data_local_dir()
            .unwrap()
            .join("DevLink/broker-owners");
        let legacy = directory.join(format!("{digest:x}.lock"));
        assert!(config::try_lock(&legacy).unwrap().is_none());
        drop(first);
        drop(second);
        let old = config::lock(&legacy).unwrap();
        assert!(try_owner_lock(&origin, 51, Some(201)).unwrap().is_none());
        assert!(try_owner_lock(&origin, 51, None).unwrap().is_none());
        drop(old);
        for name in [
            format!("{digest:x}.lock"),
            format!("{digest:x}.agent-201.lock"),
            format!("{digest:x}.agent-301.lock"),
        ] {
            std::fs::remove_file(directory.join(name)).unwrap();
        }
    }
}

#[cfg(test)]
mod update_cache_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PathBuf, Profile, Agent) {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        let agent = Agent {
            agent_id: 201,
            team_id: 71,
            owner_user_id: 51,
            name: "Cache fixture".into(),
            role_description: None,
            part: AgentPart::default(),
            roles: Vec::new(),
            tool_type: Tool::Codex,
            project_name: "Before".into(),
            status: "ONLINE".into(),
            last_seen_at: None,
            created_at: "2026-11-08T00:00:00Z".into(),
        };
        let before = Profile {
            schema_version: 1,
            origin: "http://127.0.0.1:8080/".into(),
            user_id: 51,
            team_id: 71,
            project_root: root.path().canonicalize().unwrap().to_str().unwrap().into(),
            state: State::Ready {
                agent: agent.clone(),
                token: "synthetic-cache-token".into(),
            },
        };
        save(&config, &before).unwrap();
        let mut updated = agent;
        updated.project_name = "Central value".into();
        (root, config, before, updated)
    }

    #[test]
    fn changed_profile_is_reported_unsynced_and_preserved_byte_for_byte() {
        let (_root, config, before, updated) = fixture();
        let mut concurrent = before.clone();
        if let State::Ready { agent, .. } = &mut concurrent.state {
            agent.role_description = Some("Concurrent value".into());
        }
        save(&config, &concurrent).unwrap();
        let bytes = std::fs::read(path(&config)).unwrap();
        assert!(!sync_cached_agent(&config, &before, &updated));
        assert_eq!(std::fs::read(path(&config)).unwrap(), bytes);
    }

    #[test]
    fn missing_or_corrupt_profile_is_reported_unsynced_without_recreating_it() {
        let (_root, config, before, updated) = fixture();
        std::fs::write(path(&config), b"corrupt-private-profile").unwrap();
        assert!(!sync_cached_agent(&config, &before, &updated));
        assert_eq!(
            std::fs::read(path(&config)).unwrap(),
            b"corrupt-private-profile"
        );
        std::fs::remove_file(path(&config)).unwrap();
        assert!(!sync_cached_agent(&config, &before, &updated));
        assert!(!path(&config).exists());
    }
}
