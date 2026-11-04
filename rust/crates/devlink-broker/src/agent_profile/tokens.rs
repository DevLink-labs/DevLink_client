//! Setup token preparation: verified reuse, explicit issuance, no mutation retries.
use super::{registration::*, *};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VerificationFailure {
    Rejected,
    Unavailable,
    Protocol,
}
impl std::fmt::Display for VerificationFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Rejected => "CENTRAL_UNAUTHENTICATED: Agent token verification failed; explicit token issue may be required",
            Self::Unavailable => "CENTRAL_UNAVAILABLE: cannot verify Agent credential; no token was rotated",
            Self::Protocol => "Invalid Agent verification response",
        })
    }
}
impl std::error::Error for VerificationFailure {}

/// Shared with legacy bootstrap/start. Error types never carry response bodies or credentials.
pub(super) async fn verify(origin: &str, token: &str) -> Result<Agent, VerificationFailure> {
    use VerificationFailure as V;
    if !valid_token(token) {
        return Err(V::Protocol);
    }
    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| V::Protocol)?;
    authorization.set_sensitive(true);
    let client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| V::Unavailable)?;
    let url = reqwest::Url::parse(origin)
        .and_then(|u| u.join("/api/v1/agents/me"))
        .map_err(|_| V::Protocol)?;
    let mut response = client
        .get(url)
        .header(AUTHORIZATION, authorization)
        .send()
        .await
        .map_err(|_| V::Unavailable)?;
    match response.status().as_u16() {
        401 | 403 => return Err(V::Rejected),
        429 | 500..=599 => return Err(V::Unavailable),
        200..=299 => {}
        _ => return Err(V::Protocol),
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| V::Unavailable)? {
        if bytes.len() + chunk.len() > LIMIT {
            return Err(V::Protocol);
        }
        bytes.extend_from_slice(&chunk);
    }
    #[derive(Deserialize)]
    struct Envelope {
        success: bool,
        data: Agent,
    }
    let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| V::Protocol)?;
    if !envelope.success {
        return Err(V::Protocol);
    }
    Ok(envelope.data)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPreparation {
    pub agent_id: String,
    pub verified: bool,
    pub reused: bool,
    pub issued: bool,
    pub outcome: Mutation,
    pub metadata: Option<crate::setup_project::AgentMetadata>,
}
fn result(profile: &Profile, verified: bool, issued: bool, outcome: Mutation) -> TokenPreparation {
    TokenPreparation {
        agent_id: profile
            .agent()
            .map(|a| a.agent_id.to_string())
            .unwrap_or_default(),
        verified,
        reused: verified && !issued,
        issued,
        outcome,
        metadata: summary(profile, true).metadata,
    }
}
fn known(mut failure: Failure, id: u64) -> Failure {
    failure.agent_id = Some(id);
    failure
}
fn fail(kind: FailureKind, id: u64) -> Failure {
    Failure {
        kind,
        outcome: Mutation::NotApplied,
        agent_id: Some(id),
    }
}
fn check_identity(agent: &Agent, expected: &RegistrationContext, id: u64) -> Result<(), Failure> {
    // A credential belonging to another subject is a protocol contradiction,
    // never permission to rotate the expected subject's credential automatically.
    validate_agent(agent, expected).map_err(|_| fail(FailureKind::Protocol, id))?;
    if agent.agent_id != id {
        return Err(fail(FailureKind::Protocol, id));
    }
    Ok(())
}

/// Verify the stored token, and issue at most once only when permission is explicit.
/// A valid token is always reused. Caller grants first issuance only for a newly
/// confirmed creation in this run; rerunning a create is not a new issuance grant.
pub async fn ensure(
    path: &Path,
    expected: &RegistrationContext,
    id: u64,
    allow_issue: bool,
) -> Result<TokenPreparation, Failure> {
    if !valid_id(id) {
        return Err(fail(FailureKind::Protocol, id));
    }
    let mut client = UserClient::authenticated_for_setup(path, &expected.origin, expected.user_id)
        .await
        .map_err(|e| known(central(e), id))?;
    let lock = config::state_directory(path).join("control.lock");
    let _control = config::try_lock(&lock)
        .map_err(|_| fail(FailureKind::Storage, id))?
        .ok_or_else(|| fail(FailureKind::ConfigBusy, id))?;
    check_binding(path, expected).map_err(|e| known(e, id))?;
    let team = teams::require_selected(path, &mut client)
        .await
        .map_err(|e| known(central(e), id))?;
    if team.team_id != expected.team_id {
        return Err(fail(FailureKind::SessionConflict, id));
    }
    check_binding(path, expected).map_err(|e| known(e, id))?;
    let mut profile = load(path)
        .map_err(|_| fail(FailureKind::Storage, id))?
        .ok_or_else(|| fail(FailureKind::BindingConflict, id))?;
    if profile.origin != expected.origin
        || profile.user_id != expected.user_id
        || profile.team_id != expected.team_id
        || project(Path::new(&profile.project_root)).ok().as_deref()
            != expected.project.root.to_str()
        || !profile.agent().is_some_and(|a| a.agent_id == id)
    {
        return Err(fail(FailureKind::BindingConflict, id));
    }
    let saved_agent = profile
        .agent()
        .cloned()
        .ok_or_else(|| fail(FailureKind::BindingConflict, id))?;
    validate_agent(&saved_agent, expected).map_err(|e| known(e, id))?;
    if let State::Ready { token, .. } = &profile.state {
        match verify(&profile.origin, token).await {
            Ok(agent) => {
                check_identity(&agent, expected, id)?;
                check_binding(path, expected).map_err(|e| known(e, id))?;
                return Ok(result(&profile, true, false, Mutation::NotApplied));
            }
            Err(VerificationFailure::Rejected) => {}
            Err(VerificationFailure::Unavailable) => {
                return Err(fail(FailureKind::Unavailable, id));
            }
            Err(VerificationFailure::Protocol) => return Err(fail(FailureKind::Protocol, id)),
        }
    }
    if !allow_issue {
        return Ok(result(&profile, false, false, Mutation::NotApplied));
    }
    let _guard =
        require_mutation(path, &expected.origin, expected.user_id).map_err(|e| known(e, id))?;
    let owned: Agent = client
        .get(&format!("/api/v1/agents/{id}"))
        .await
        .map_err(|e| known(central(e), id))?;
    validate_agent(&owned, expected).map_err(|e| known(e, id))?;
    if owned.agent_id != id {
        return Err(fail(FailureKind::Protocol, id));
    }
    let prepared = client
        .prepare_post()
        .await
        .map_err(|e| known(central(e), id))?;
    check_binding(path, expected).map_err(|e| known(e, id))?;
    profile.state = State::NeedsToken { agent: owned };
    save(path, &profile).map_err(|_| Failure {
        kind: FailureKind::Storage,
        outcome: Mutation::Unknown,
        agent_id: Some(id),
    })?;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Issued {
        agent_id: u64,
        token: String,
    }
    let issued: Issued = match client
        .post_prepared(&format!("/api/v1/agents/{id}/token"), json!({}), prepared)
        .await
    {
        Ok(issued) => issued,
        Err(error) => {
            if user_auth::rejection_status(&error).is_some_and(|s| (400..500).contains(&s)) {
                let mut failure = known(central(error), id);
                failure.outcome = Mutation::Applied; // the old credential was durably removed
                return Err(failure);
            }
            return Ok(result(&profile, false, false, Mutation::Unknown));
        }
    };
    if issued.agent_id != id {
        return Ok(result(&profile, false, false, Mutation::Unknown));
    }
    let verified = match verify(&profile.origin, &issued.token).await {
        Ok(agent) if check_identity(&agent, expected, id).is_ok() => agent,
        _ => return Ok(result(&profile, false, false, Mutation::Unknown)),
    };
    // After issuance, every uncertain publication remains TOKEN_REQUIRED, with
    // the known ID and needs_token retained. No old token is restored or replayed.
    if check_binding(path, expected).is_err() || rotate_mcp_if_present(path).is_err() {
        return Ok(result(&profile, false, false, Mutation::Unknown));
    }
    let mut ready = profile.clone();
    ready.state = State::Ready {
        agent: verified,
        token: issued.token,
    };
    if save(path, &ready).is_err() {
        return Ok(result(&profile, false, false, Mutation::Unknown));
    }
    Ok(result(&ready, true, true, Mutation::Applied))
}
