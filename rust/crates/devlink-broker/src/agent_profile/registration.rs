//! Setup registration only: never issues tokens or retries an uncertain creation.
use super::*;
use crate::{
    sessions::ToolType,
    setup_project::{AgentMetadata, PreparedProject},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    Create,
    Use(u64),
}

/// Frozen setup principal and project; no caller-supplied identity reaches central DTOs.
#[derive(Clone)]
pub struct RegistrationContext {
    pub origin: String,
    pub user_id: u64,
    pub team_id: u64,
    pub project: PreparedProject,
    pub tool: ToolType,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationState {
    Vacant,
    CreationUnknown,
    TokenRequired,
    Ready,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Registration {
    pub state: RegistrationState,
    #[serde(serialize_with = "serialize_id")]
    pub agent_id: Option<u64>,
    pub created: bool,
    pub reused: bool,
    pub outcome: Mutation,
    /// Verified metadata for subsequent in-process setup stages; no credential values.
    #[serde(skip)]
    pub metadata: Option<AgentMetadata>,
}
fn serialize_id<S: serde::Serializer>(id: &Option<u64>, s: S) -> Result<S::Ok, S::Error> {
    id.map(|id| id.to_string()).serialize(s)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mutation {
    NotApplied,
    Applied,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    Storage,
    Busy,
    ConfigBusy,
    BrokerBusy,
    BindingConflict,
    Forbidden,
    SessionConflict,
    Rejected,
    AlreadyExists,
    AuthRejected,
    Unavailable,
    Protocol,
}
#[derive(Debug)]
pub struct Failure {
    pub kind: FailureKind,
    pub outcome: Mutation,
    pub agent_id: Option<u64>,
}
fn failure(kind: FailureKind) -> Failure {
    Failure {
        kind,
        outcome: Mutation::NotApplied,
        agent_id: None,
    }
}
pub(super) fn central(error: anyhow::Error) -> Failure {
    if let Some(fault) = error.downcast_ref::<teams::SetupTeamFault>() {
        return failure(match fault.kind {
            teams::SetupTeamFaultKind::Forbidden => FailureKind::Forbidden,
            teams::SetupTeamFaultKind::Protocol
            | teams::SetupTeamFaultKind::ListResourceNotFound => FailureKind::Protocol,
            teams::SetupTeamFaultKind::BindingConflict => FailureKind::BindingConflict,
            teams::SetupTeamFaultKind::Storage => FailureKind::Storage,
            teams::SetupTeamFaultKind::Busy => FailureKind::ConfigBusy,
            teams::SetupTeamFaultKind::SessionConflict => FailureKind::SessionConflict,
        });
    }
    let kind = if let Some(fault) = error.downcast_ref::<user_auth::AuthFault>() {
        use user_auth::AuthFaultKind as A;
        match fault.kind {
            A::Unavailable => FailureKind::Unavailable,
            A::Protocol => FailureKind::Protocol,
            A::Busy => FailureKind::Busy,
            A::ConfigBusy => FailureKind::ConfigBusy,
            A::BrokerBusy => FailureKind::BrokerBusy,
            A::SessionConflict | A::IdentityMismatch => FailureKind::SessionConflict,
            _ => FailureKind::Storage,
        }
    } else {
        if error
            .downcast_ref::<user_auth::ApiRejection>()
            .is_some_and(|rejection| {
                rejection.status == 409 && rejection.code == "AGENT_ALREADY_EXISTS"
            })
        {
            return failure(FailureKind::AlreadyExists);
        }
        match user_auth::rejection_status(&error) {
            Some(401) => FailureKind::AuthRejected,
            Some(403) => FailureKind::Forbidden,
            Some(404) => FailureKind::Rejected,
            Some(500..=599) => FailureKind::Unavailable,
            Some(_) => FailureKind::Rejected,
            None => FailureKind::Storage,
        }
    };
    failure(kind)
}

pub(super) fn check_binding(path: &Path, expected: &RegistrationContext) -> Result<(), Failure> {
    let current = Config::load(path)
        .and_then(|c| user_auth::origin(&c))
        .map_err(|_| failure(FailureKind::Storage))?;
    let selected = teams::stored_selection(path).map_err(|_| failure(FailureKind::Storage))?;
    if current.as_str() != expected.origin
        || !selected.is_some_and(|s| {
            s.origin == expected.origin
                && s.user_id == expected.user_id
                && s.team_id == expected.team_id
        })
    {
        return Err(failure(FailureKind::SessionConflict));
    }
    let root =
        project(&expected.project.root).map_err(|_| failure(FailureKind::BindingConflict))?;
    if Path::new(&root) != expected.project.root {
        return Err(failure(FailureKind::BindingConflict));
    }
    let selected = crate::setup_tools::inspect(
        path,
        &crate::setup_tools::SelectionBinding {
            origin: expected.origin.clone(),
            user_id: expected.user_id,
            team_id: expected.team_id,
            project_root: expected.project.root.clone(),
        },
    )
    .map_err(|error| {
        failure(match error.kind {
            crate::setup_tools::FailureKind::Storage => FailureKind::Storage,
            crate::setup_tools::FailureKind::SessionConflict => FailureKind::SessionConflict,
            crate::setup_tools::FailureKind::Busy => FailureKind::ConfigBusy,
            _ => FailureKind::BindingConflict,
        })
    })?;
    if selected
        .preferred()
        .is_none_or(|s| s.tool_type != expected.tool)
    {
        return Err(failure(FailureKind::BindingConflict));
    }
    Ok(())
}
fn tool(expected: &RegistrationContext) -> Tool {
    match expected.tool {
        ToolType::Codex => Tool::Codex,
        ToolType::ClaudeCode => Tool::ClaudeCode,
    }
}
fn matches_metadata(agent: &Agent, expected: &RegistrationContext) -> bool {
    agent.name == expected.project.metadata.agent_name
        && agent.project_name == expected.project.metadata.project_name
        && agent.role_description == expected.project.metadata.role_description
        && agent.part == expected.project.metadata.part
        && agent.roles == expected.project.metadata.roles
        && agent.tool_type == tool(expected)
}
pub(super) fn validate_agent(agent: &Agent, expected: &RegistrationContext) -> Result<(), Failure> {
    validate_owned_agent(agent, expected)?;
    if agent.tool_type != tool(expected) {
        return Err(failure(FailureKind::BindingConflict));
    }
    Ok(())
}

fn validate_owned_agent(agent: &Agent, expected: &RegistrationContext) -> Result<(), Failure> {
    if !valid_id(agent.agent_id) || !valid_id(agent.owner_user_id) || !valid_id(agent.team_id) {
        return Err(failure(FailureKind::Protocol));
    }
    if agent.owner_user_id != expected.user_id || agent.team_id != expected.team_id {
        return Err(failure(FailureKind::Forbidden));
    }
    if agent.name.trim().is_empty()
        || normalize_roles(&agent.roles).is_err()
        || agent.name.encode_utf16().count() > 100
        || agent.project_name.trim().is_empty()
        || agent.project_name.encode_utf16().count() > 120
        || agent
            .role_description
            .as_ref()
            .is_some_and(|s| s.encode_utf16().count() > 255)
    {
        return Err(failure(FailureKind::Protocol));
    }
    Ok(())
}
pub(super) fn summary(profile: &Profile, reused: bool) -> Registration {
    Registration {
        state: match profile.state {
            State::Vacant => RegistrationState::Vacant,
            State::CreatePending { .. } => RegistrationState::CreationUnknown,
            State::NeedsToken { .. } => RegistrationState::TokenRequired,
            State::Ready { .. } => RegistrationState::Ready,
        },
        agent_id: profile.agent().map(|a| a.agent_id),
        created: false,
        reused: reused && profile.agent().is_some(),
        outcome: if matches!(profile.state, State::CreatePending { .. }) {
            Mutation::Unknown
        } else {
            Mutation::NotApplied
        },
        metadata: profile.agent().map(|agent| AgentMetadata {
            agent_name: agent.name.clone(),
            project_name: agent.project_name.clone(),
            role_description: agent.role_description.clone(),
            part: agent.part,
            roles: agent.roles.clone(),
        }),
    }
}

/// Inspect existing ownership and metadata. Never creates, binds or issues credentials.
pub async fn inspect(path: &Path, expected: &RegistrationContext) -> Result<Registration, Failure> {
    ensure(path, expected, None).await
}

/// Read-only recovery choices. A name match never authorizes binding or issuance.
pub async fn owned_agents(
    path: &Path,
    expected: &RegistrationContext,
) -> Result<Vec<Agent>, Failure> {
    let mut client = UserClient::authenticated_for_setup(path, &expected.origin, expected.user_id)
        .await
        .map_err(central)?;
    let lock = config::state_directory(path).join("control.lock");
    let _control = config::try_lock(&lock)
        .map_err(|_| failure(FailureKind::Storage))?
        .ok_or_else(|| failure(FailureKind::ConfigBusy))?;
    check_binding(path, expected)?;
    let team = teams::require_selected(path, &mut client)
        .await
        .map_err(central)?;
    if team.team_id != expected.team_id {
        return Err(failure(FailureKind::SessionConflict));
    }
    let mut agents: Vec<Agent> = client
        .get_with_query(
            &format!("/api/v1/agents/teams/{}", expected.team_id),
            &[("ownedByMe", "true")],
        )
        .await
        .map_err(central)?;
    let mut ids = std::collections::HashSet::new();
    for agent in &agents {
        validate_owned_agent(agent, expected).map_err(|_| failure(FailureKind::Protocol))?;
        if !ids.insert(agent.agent_id) {
            return Err(failure(FailureKind::Protocol));
        }
    }
    check_binding(path, expected)?;
    agents.retain(|agent| agent.tool_type == tool(expected));
    agents.sort_by_key(|agent| agent.agent_id);
    Ok(agents)
}

/// Explicit create/use, or validation/reuse of an existing profile when selection is absent.
/// Account -> control -> broker -> owner follows the individual command lock order.
pub async fn ensure(
    path: &Path,
    expected: &RegistrationContext,
    selection: Option<Selection>,
) -> Result<Registration, Failure> {
    ensure_tracked(path, expected, selection, &mut false).await
}

/// Set the recovery marker after durably recording creation and before sending POST.
pub async fn ensure_tracked(
    path: &Path,
    expected: &RegistrationContext,
    selection: Option<Selection>,
    creation_pending: &mut bool,
) -> Result<Registration, Failure> {
    if matches!(selection, Some(Selection::Use(id)) if !valid_id(id)) {
        return Err(failure(FailureKind::Rejected));
    }
    if selection == Some(Selection::Create) {
        crate::setup_project::resolve(&crate::setup_project::ProjectOptions {
            path: Some(expected.project.root.clone()),
            project_name: Some(expected.project.metadata.project_name.clone()),
            agent_name: Some(expected.project.metadata.agent_name.clone()),
            role_description: expected.project.metadata.role_description.clone(),
            part: Some(expected.project.metadata.part),
            roles: Some(expected.project.metadata.roles.clone()),
        })
        .map_err(|_| failure(FailureKind::BindingConflict))?;
    }
    let mut client = UserClient::authenticated_for_setup(path, &expected.origin, expected.user_id)
        .await
        .map_err(central)?;
    if selection == Some(Selection::Create) {
        stop_deleted_agent_broker(path, expected, &mut client).await?;
    }
    let lock = config::state_directory(path).join("control.lock");
    let _control = config::try_lock(&lock)
        .map_err(|_| failure(FailureKind::Storage))?
        .ok_or_else(|| failure(FailureKind::ConfigBusy))?;
    check_binding(path, expected)?;
    let team = teams::require_selected(path, &mut client)
        .await
        .map_err(central)?;
    if team.team_id != expected.team_id {
        return Err(failure(FailureKind::SessionConflict));
    }
    check_binding(path, expected)?;
    let existing = load(path).map_err(|_| failure(FailureKind::Storage))?;
    let mut replacing_deleted = false;
    if let Some(old) = &existing
        && !matches!(old.state, State::Vacant)
    {
        let old_root = project(Path::new(&old.project_root))
            .map_err(|_| failure(FailureKind::BindingConflict))?;
        if old.origin != expected.origin
            || old.user_id != expected.user_id
            || old.team_id != expected.team_id
            || Path::new(&old_root) != expected.project.root
        {
            return Err(failure(FailureKind::BindingConflict));
        }
        if let Some(agent) = old.agent() {
            validate_agent(agent, expected)?;
        } else if let State::CreatePending { tool: saved, .. } = &old.state
            && *saved != tool(expected)
        {
            return Err(failure(FailureKind::BindingConflict));
        }
        if !matches!(selection, Some(Selection::Use(_)))
            && !matches!(old.state, State::Vacant)
            && let Some(agent) = old.agent()
        {
            let lookup = client
                .get::<CentralAgent>(&format!("/api/v1/agents/{}", agent.agent_id))
                .await;
            // An explicit create replaces a binding whose Agent central reports deleted.
            // IDs are never reused, so a 404 cannot point at another live Agent.
            if selection == Some(Selection::Create) && lookup.as_ref().is_err_and(agent_deleted) {
                replacing_deleted = true;
            } else {
                if !matches_metadata(agent, expected) {
                    return Err(failure(FailureKind::BindingConflict));
                }
                let known = |mut failure: Failure| {
                    failure.agent_id = Some(agent.agent_id);
                    failure
                };
                // A reply without the part or role tags cannot confirm the saved metadata.
                let verified = lookup
                    .map_err(|error| known(central(error)))?
                    .confirmed()
                    .map_err(|_| known(failure(FailureKind::Protocol)))?;
                validate_agent(&verified, expected)?;
                if verified.agent_id != agent.agent_id || !matches_metadata(&verified, expected) {
                    return Err(failure(FailureKind::BindingConflict));
                }
            }
        }
        if !replacing_deleted
            && !matches!(selection, Some(Selection::Use(_)))
            && !matches!(old.state, State::Vacant)
        {
            check_binding(path, expected)?;
            // Token validation/issuance is the next adapter's responsibility (-09).
            return Ok(summary(old, true));
        }
    }
    if selection.is_none() {
        return Ok(Registration {
            state: RegistrationState::Vacant,
            agent_id: None,
            created: false,
            reused: false,
            outcome: Mutation::NotApplied,
            metadata: None,
        });
    }
    if let Some(Selection::Use(id)) = selection {
        let agent = client
            .get::<CentralAgent>(&format!("/api/v1/agents/{id}"))
            .await
            .map_err(central)?
            .confirmed()
            .map_err(|_| failure(FailureKind::Protocol))?;
        validate_agent(&agent, expected)?;
        if agent.agent_id != id {
            return Err(failure(FailureKind::Protocol));
        }
        if let Some(old) = &existing
            && old.agent().is_some_and(|a| a.agent_id == id)
        {
            // An explicit same-ID selection acknowledges metadata drift. It never
            // issues a token, edits central metadata or changes the local project.
            if old.agent().is_some_and(|a| {
                a.name != agent.name
                    || a.project_name != agent.project_name
                    || a.role_description != agent.role_description
                    || a.part != agent.part
                    || a.roles != agent.roles
                    || a.tool_type != agent.tool_type
            }) {
                let _guard = require_mutation(path, &expected.origin, expected.user_id, Some(id))?;
                check_binding(path, expected)?;
                let mut refreshed = old.clone();
                match &mut refreshed.state {
                    State::Ready { agent: saved, .. } | State::NeedsToken { agent: saved } => {
                        *saved = agent
                    }
                    _ => return Err(failure(FailureKind::Protocol)),
                }
                save(path, &refreshed).map_err(|_| Failure {
                    kind: FailureKind::Storage,
                    outcome: Mutation::Unknown,
                    agent_id: Some(id),
                })?;
                let mut result = summary(&refreshed, true);
                result.outcome = Mutation::Applied;
                return Ok(result);
            }
            check_binding(path, expected)?;
            return Ok(summary(old, true));
        }
        let _owner = require_mutation(path, &expected.origin, expected.user_id, Some(id))?;
        check_binding(path, expected)?;
        let profile = Profile {
            schema_version: 1,
            origin: expected.origin.clone(),
            user_id: expected.user_id,
            team_id: expected.team_id,
            project_root: expected.project.root.to_string_lossy().into_owned(),
            state: State::NeedsToken { agent },
        };
        rotate_mcp_if_present(path).map_err(|_| Failure {
            kind: FailureKind::Storage,
            outcome: Mutation::Unknown,
            agent_id: Some(id),
        })?;
        save(path, &profile).map_err(|_| Failure {
            kind: FailureKind::Storage,
            outcome: Mutation::Unknown,
            agent_id: Some(id),
        })?;
        let mut result = summary(&profile, false);
        result.outcome = Mutation::Applied;
        return Ok(result);
    }
    let _owner = require_mutation(path, &expected.origin, expected.user_id, None)?;
    let prepared = client.prepare_post().await.map_err(central)?;
    check_binding(path, expected)?;
    // Like switching IDs, MCP sessions of the deleted Agent must not reach the new one.
    if replacing_deleted {
        rotate_mcp_if_present(path).map_err(|_| Failure {
            kind: FailureKind::Storage,
            outcome: Mutation::Unknown,
            agent_id: None,
        })?;
    }
    let metadata = &expected.project.metadata;
    let mut profile = Profile {
        schema_version: 1,
        origin: expected.origin.clone(),
        user_id: expected.user_id,
        team_id: expected.team_id,
        project_root: expected.project.root.to_string_lossy().into_owned(),
        state: State::CreatePending {
            name: metadata.agent_name.clone(),
            tool: tool(expected),
            project_name: metadata.project_name.clone(),
        },
    };
    save(path, &profile).map_err(|_| Failure {
        kind: FailureKind::Storage,
        outcome: Mutation::Unknown,
        agent_id: None,
    })?;
    *creation_pending = true;
    let response = client
        .post_prepared::<CentralAgent>(
            &format!("/api/v1/agents/teams/{}", expected.team_id),
            json!({"name":metadata.agent_name,"projectName":metadata.project_name,
            "roleDescription":metadata.role_description,"toolType":tool(expected),
            "part":metadata.part,"roles":metadata.roles}),
            prepared,
        )
        .await;
    let CentralAgent { agent, reported } = match response {
        Ok(created) => created,
        Err(error) => {
            if user_auth::rejection_status(&error).is_some_and(|s| (400..500).contains(&s)) {
                profile.state = State::Vacant;
                save(path, &profile).map_err(|_| Failure {
                    kind: FailureKind::Storage,
                    outcome: Mutation::Unknown,
                    agent_id: None,
                })?;
                return Err(central(error));
            }
            return Ok(Registration {
                state: RegistrationState::CreationUnknown,
                agent_id: None,
                created: false,
                reused: false,
                outcome: Mutation::Unknown,
                metadata: None,
            });
        }
    };
    // An untrusted success remains pending; never bind a different principal or DTO.
    // A reply without part/roles reads as the default request, so only values central
    // actually sent can match (GitHub #127).
    if validate_agent(&agent, expected).is_err()
        || !reported.complete()
        || !matches_metadata(&agent, expected)
    {
        return Ok(Registration {
            state: RegistrationState::CreationUnknown,
            agent_id: None,
            created: false,
            reused: false,
            outcome: Mutation::Unknown,
            metadata: None,
        });
    }
    let id = agent.agent_id;
    profile.state = State::NeedsToken { agent };
    save(path, &profile).map_err(|_| Failure {
        kind: FailureKind::Storage,
        outcome: Mutation::Unknown,
        agent_id: Some(id),
    })?;
    Ok(Registration {
        state: RegistrationState::TokenRequired,
        agent_id: Some(id),
        created: true,
        reused: false,
        outcome: Mutation::Applied,
        metadata: None,
    })
}

/// The broker of a deleted Agent can no longer act centrally but still holds the
/// broker lock. Stop it gracefully before taking the control lock that `stop` needs;
/// registration re-reads the profile and decides the replacement under its locks.
async fn stop_deleted_agent_broker(
    path: &Path,
    expected: &RegistrationContext,
    client: &mut UserClient,
) -> Result<(), Failure> {
    let Some(old) = load(path).map_err(|_| failure(FailureKind::Storage))? else {
        return Ok(());
    };
    let Some(agent) = old.agent() else {
        return Ok(());
    };
    if old.origin != expected.origin
        || old.user_id != expected.user_id
        || old.team_id != expected.team_id
    {
        return Ok(());
    }
    let lookup = client
        .get::<Agent>(&format!("/api/v1/agents/{}", agent.agent_id))
        .await;
    if lookup.as_ref().is_err_and(agent_deleted) {
        crate::broker_control::stop(path)
            .await
            .map_err(|_| failure(FailureKind::BrokerBusy))?;
    }
    Ok(())
}

fn agent_deleted(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<user_auth::ApiRejection>()
        .is_some_and(|rejection| rejection.status == 404 && rejection.code == "RESOURCE_NOT_FOUND")
}

pub(super) fn require_mutation(
    path: &Path,
    origin: &str,
    user: u64,
    agent: Option<u64>,
) -> Result<(File, OwnerGuard), Failure> {
    // Keep the broker lock, rather than only probing it: a foreground broker must
    // not start between the stopped check and durable profile publication.
    let lock = config::state_directory(path).join("broker.lock");
    let broker = config::try_lock(&lock)
        .map_err(|_| failure(FailureKind::Storage))?
        .ok_or_else(|| failure(FailureKind::BrokerBusy))?;
    let owner = try_owner_lock(origin, user, agent)
        .map_err(|_| failure(FailureKind::Storage))?
        .ok_or_else(|| failure(FailureKind::BrokerBusy))?;
    Ok((broker, owner))
}
