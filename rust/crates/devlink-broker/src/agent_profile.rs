//! Explicit, recoverable Agent bootstrap. Server identity is the authority.
pub mod registration;
pub mod tokens;
use crate::{
    config::{self, Config},
    credentials, private_file, teams,
    user_auth::{self, UserClient},
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
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
            }),
            State::CreatePending {
                name, project_name, ..
            } => Some(AgentMetadata {
                project_name: project_name.clone(),
                agent_name: name.clone(),
                role_description: None,
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

/// Local-machine, per-central-user lock. It is not a claim of server-wide policy.
fn owner_lock(origin: &str, user: u64) -> Result<File> {
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
    config::lock(&lock).context(
        "Another broker or setup operation is active for this central user on this machine",
    )
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

pub async fn create(config_path: &Path, options: CreateOptions) -> Result<Value> {
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
                && agent.role_description == options.role_description,
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
    let _owner = owner_lock(client.central_origin(), user)?;
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
    let result=client.post_prepared::<Agent>(&format!("/api/v1/agents/teams/{}",team.team_id),json!({"name":options.name,"roleDescription":options.role_description,"toolType":options.tool,"projectName":options.project_name}),prepared).await;
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
    profile.state = State::NeedsToken {
        agent: agent.clone(),
    };
    save(config_path,&profile).map_err(|_|anyhow::anyhow!("Agent {} created; local save failed. Recover with explicit agent use, never create again blindly",agent.agent_id))?;
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
    let _owner = owner_lock(client.central_origin(), client.user()?.user_id)?;
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
    let _owner = owner_lock(client.central_origin(), client.user()?.user_id)?;
    issue(config_path, &mut client, &mut profile, agent).await
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
    let _owner = owner_lock(client.central_origin(), client.user()?.user_id)?;
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
    pub _owner: File,
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
    let owner = owner_lock(&profile.origin, profile.user_id)?;
    ensure!(
        load(config_path)?.as_ref() == Some(&profile),
        "Agent profile changed during startup; retry explicitly"
    );
    Ok(Some(BrokerIdentity {
        token,
        _owner: owner,
    }))
}
