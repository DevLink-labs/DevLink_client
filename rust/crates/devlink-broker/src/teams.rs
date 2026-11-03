//! Team selectors are server-verified membership, never user authority claims.
use crate::{config, private_file, user_auth::UserClient};
use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupTeamFaultKind {
    Forbidden,
    ListResourceNotFound,
    Protocol,
    Storage,
    BindingConflict,
    Busy,
    SessionConflict,
}

#[derive(Debug)]
pub struct SetupTeamFault {
    pub kind: SetupTeamFaultKind,
    pub mutation_uncertain: bool,
    pub file: Option<PathBuf>,
    message: &'static str,
}

impl std::fmt::Display for SetupTeamFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message)
    }
}
impl std::error::Error for SetupTeamFault {}

fn setup_fault(kind: SetupTeamFaultKind, message: &'static str) -> anyhow::Error {
    SetupTeamFault {
        kind,
        mutation_uncertain: false,
        file: None,
        message,
    }
    .into()
}

fn storage_fault(file: PathBuf, message: &'static str) -> anyhow::Error {
    SetupTeamFault {
        kind: SetupTeamFaultKind::Storage,
        mutation_uncertain: false,
        file: Some(file),
        message,
    }
    .into()
}

pub enum SetupSelection {
    Selected { team: Team, reused: bool },
    Choose(Vec<Team>),
    NoTeams,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Role {
    Owner,
    Member,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Team {
    pub team_id: u64,
    pub name: String,
    pub my_role: Role,
    pub created_at: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreatedTeam {
    #[serde(flatten)]
    pub team: Team,
    pub invite_code: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinedTeam {
    pub team_id: u64,
    pub name: String,
    pub my_role: Role,
    pub joined_at: String,
    pub replayed: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub user_id: u64,
    pub name: String,
    pub nickname: Option<String>,
    pub role: Role,
    pub joined_at: String,
}

/// Cached selector only. require_selected always rechecks the current server user
/// and membership before a caller may use it for Agent bootstrap.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Selection {
    pub schema_version: u32,
    pub origin: String,
    pub user_id: u64,
    pub team_id: u64,
}

pub fn selection_path(config_path: &Path) -> PathBuf {
    config::state_directory(config_path).join("selected-team.json")
}

pub fn stored_selection(config_path: &Path) -> Result<Option<Selection>> {
    let value = private_file::read::<Selection>(&selection_path(config_path), 16_384)?;
    if let Some(s) = &value {
        ensure!(
            s.schema_version == 1 && valid_id(s.user_id) && valid_id(s.team_id),
            "Invalid selected team snapshot"
        );
    }
    Ok(value)
}

fn valid_id(id: u64) -> bool {
    id > 0 && id <= i64::MAX as u64
}
fn validate_team(team: &Team) -> Result<()> {
    if !valid_id(team.team_id) || team.name.trim().is_empty() {
        return Err(setup_fault(
            SetupTeamFaultKind::Protocol,
            "Invalid central team response",
        ));
    }
    Ok(())
}

async fn member_team(client: &mut UserClient, id: u64) -> Result<Team> {
    ensure!(valid_id(id), "Invalid team ID");
    let team: Team = client.get(&format!("/api/v1/teams/{id}")).await?;
    validate_team(&team)?;
    if team.team_id != id {
        return Err(setup_fault(
            SetupTeamFaultKind::Protocol,
            "Central team identity mismatch",
        ));
    }
    let members: Vec<Member> = client.get(&format!("/api/v1/teams/{id}/members")).await?;
    let user = client.user()?.user_id;
    let mut identities = std::collections::HashSet::new();
    if !members
        .iter()
        .all(|m| valid_id(m.user_id) && identities.insert(m.user_id))
    {
        return Err(setup_fault(
            SetupTeamFaultKind::Protocol,
            "Invalid central membership response",
        ));
    }
    if !members
        .iter()
        .any(|m| m.user_id == user && m.role == team.my_role)
    {
        return Err(setup_fault(
            SetupTeamFaultKind::Forbidden,
            "ACCESS_DENIED: verified current user is not a member of this team",
        ));
    }
    Ok(team)
}

pub async fn create(config_path: &Path, name: &str) -> Result<CreatedTeam> {
    ensure!(
        !name.trim().is_empty() && name.encode_utf16().count() <= 120,
        "Team name must contain 1 to 120 characters"
    );
    let mut client = UserClient::authenticated(config_path).await?;
    let created: CreatedTeam = client.post("/api/v1/teams", json!({"name":name})).await?;
    validate_team(&created.team)?;
    ensure!(
        created.team.my_role == Role::Owner
            && !created.invite_code.is_empty()
            && created.invite_code.len() <= 256,
        "Invalid central team creation response"
    );
    let verified=member_team(&mut client,created.team.team_id).await.map_err(|_|anyhow::anyhow!("Team {} was created but membership confirmation failed; check team list before retrying",created.team.team_id))?;
    ensure!(
        verified.my_role == Role::Owner,
        "Created team owner membership was not confirmed"
    );
    // Creation does not silently replace a selected team or an Agent binding.
    Ok(created)
}

pub async fn list(config_path: &Path) -> Result<Vec<Team>> {
    let mut client = UserClient::authenticated(config_path).await?;
    let teams: Vec<Team> = client.get("/api/v1/teams").await?;
    for team in &teams {
        validate_team(team)?;
    }
    Ok(teams)
}

pub async fn join(config_path: &Path, code: &str) -> Result<JoinedTeam> {
    ensure!(
        !code.is_empty() && code.len() <= 256,
        "Invalid invite code input"
    );
    let mut client = UserClient::authenticated(config_path).await?;
    let joined: JoinedTeam = client
        .post("/api/v1/teams/join", json!({"inviteCode":code}))
        .await?;
    ensure!(valid_id(joined.team_id), "Invalid joined team response");
    let verified = member_team(&mut client, joined.team_id).await?;
    ensure!(
        verified.my_role == joined.my_role,
        "Joined team membership mismatch"
    );
    Ok(joined)
}

pub async fn members(config_path: &Path, id: u64) -> Result<Vec<Member>> {
    ensure!(valid_id(id), "Invalid team ID");
    let mut client = UserClient::authenticated(config_path).await?;
    let members: Vec<Member> = client.get(&format!("/api/v1/teams/{id}/members")).await?;
    ensure!(
        members.iter().all(|m| valid_id(m.user_id)),
        "Invalid central membership response"
    );
    Ok(members)
}

pub async fn select(config_path: &Path, id: u64) -> Result<Team> {
    let mut client = UserClient::authenticated(config_path).await?;
    let team = member_team(&mut client, id).await?;
    let selection = Selection {
        schema_version: 1,
        origin: client.central_origin().to_owned(),
        user_id: client.user()?.user_id,
        team_id: team.team_id,
    };
    private_file::write(&selection_path(config_path), &selection, 16_384)?;
    Ok(team)
}

pub async fn require_selected(config_path: &Path, client: &mut UserClient) -> Result<Team> {
    let Some(selection) = stored_selection(config_path)? else {
        bail!("TEAM_SELECTION_REQUIRED: run devlink team select --id <ID>");
    };
    ensure!(
        selection.origin == client.central_origin() && selection.user_id == client.user()?.user_id,
        "TEAM_SELECTION_REQUIRED: select a team for the current central user"
    );
    member_team(client, selection.team_id).await
}

pub async fn current(config_path: &Path) -> Result<Option<Team>> {
    let mut client = UserClient::authenticated(config_path).await?;
    if stored_selection(config_path)?.is_none() {
        return Ok(None);
    }
    require_selected(config_path, &mut client).await.map(Some)
}

/// Resolve and publish a selector under the same account lock as existing team
/// commands. Cached choices never authorize a selection after input resumes.
pub async fn select_for_setup(
    config_path: &Path,
    expected_origin: &str,
    expected_user: u64,
    requested: Option<u64>,
) -> Result<SetupSelection> {
    let mut client =
        UserClient::authenticated_for_setup(config_path, expected_origin, expected_user).await?;
    let previous = stored_selection(config_path).map_err(|_| {
        storage_fault(
            selection_path(config_path),
            "Cannot read the selected team snapshot",
        )
    })?;
    let teams: Vec<Team> = client.get("/api/v1/teams").await.map_err(|error| {
        // List 404 means a membership references a missing team, not that the
        // selected team's membership was denied. Preserve other auth/HTTP faults.
        if crate::user_auth::rejection_status(&error) == Some(404) {
            setup_fault(
                SetupTeamFaultKind::ListResourceNotFound,
                "The team list references a missing central resource",
            )
        } else {
            error
        }
    })?;
    let mut ids = std::collections::HashSet::new();
    for team in &teams {
        validate_team(team)?;
        if !ids.insert(team.team_id) {
            return Err(setup_fault(
                SetupTeamFaultKind::Protocol,
                "Duplicate central team identity",
            ));
        }
    }
    let previous_id = previous
        .as_ref()
        .filter(|s| s.origin == expected_origin && s.user_id == expected_user)
        .map(|s| s.team_id);
    if teams.is_empty() {
        return Ok(SetupSelection::NoTeams);
    }
    let id = if let Some(id) = requested {
        id
    } else if let Some(id) = previous_id.filter(|id| ids.contains(id)) {
        id
    } else if teams.len() == 1 {
        teams[0].team_id
    } else {
        return Ok(SetupSelection::Choose(teams));
    };
    if !valid_id(id) || !ids.contains(&id) {
        return Err(setup_fault(
            SetupTeamFaultKind::Forbidden,
            "Choose a current joined team",
        ));
    }
    let team = member_team(&mut client, id).await?;
    let listed = teams
        .iter()
        .find(|t| t.team_id == id)
        .expect("validated team ID");
    if listed.my_role != team.my_role {
        return Err(setup_fault(
            SetupTeamFaultKind::Protocol,
            "Central team membership changed",
        ));
    }
    let lock = config::state_directory(config_path).join("control.lock");
    if std::fs::symlink_metadata(&lock).is_ok() {
        crate::credentials::verify_path_safety(&lock)
            .map_err(|_| storage_fault(lock.clone(), "Cannot inspect the team control lock"))?;
    }
    let _control = config::try_lock(&lock)
        .map_err(|_| storage_fault(lock.clone(), "Cannot acquire the team control lock"))?
        .ok_or_else(|| {
            setup_fault(
                SetupTeamFaultKind::Busy,
                "Another profile operation is running",
            )
        })?;
    // config set uses control.lock, not account.lock. Membership may have been
    // verified on the old server while the current profile's origin changed.
    // Keep this guard through both selector publication and reuse success.
    let current_config = config::Config::load(config_path).map_err(|_| {
        storage_fault(
            config_path.to_path_buf(),
            "Cannot reload the setup configuration",
        )
    })?;
    let current_origin = crate::user_auth::origin(&current_config).map_err(|_| {
        storage_fault(
            config_path.to_path_buf(),
            "Cannot verify the current central origin",
        )
    })?;
    if current_origin.as_str() != expected_origin {
        return Err(SetupTeamFault {
            kind: SetupTeamFaultKind::SessionConflict,
            mutation_uncertain: false,
            file: Some(config_path.to_path_buf()),
            message: "The central origin changed during team verification; restart setup",
        }
        .into());
    }
    if crate::agent_profile::setup_team_conflicts(config_path, expected_origin, expected_user, id)
        .map_err(|_| {
        storage_fault(
            crate::agent_profile::path(config_path),
            "Cannot inspect the Agent binding",
        )
    })? {
        return Err(setup_fault(
            SetupTeamFaultKind::BindingConflict,
            "The existing Agent is bound to another server, user, or team",
        ));
    }
    let reused = previous_id == Some(id);
    if !reused {
        private_file::write(
            &selection_path(config_path),
            &Selection {
                schema_version: 1,
                origin: expected_origin.to_owned(),
                user_id: expected_user,
                team_id: id,
            },
            16_384,
        )
        .map_err(|_| SetupTeamFault {
            kind: SetupTeamFaultKind::Storage,
            mutation_uncertain: true,
            file: Some(selection_path(config_path)),
            message: "Cannot publish the selected team snapshot",
        })?;
    }
    Ok(SetupSelection::Selected { team, reused })
}
