//! 로컬 MCP 세션 임대 및 소유권 관리 엔진
//!
//! - MCP 클라이언트(AI 도구)와의 로컬 세션을 브로커 시계 기준으로 90초 동안 유지합니다.
//! - 중앙 서버의 사용자/팀 인증 및 Agent 소유권 검증 결과를 기반으로 Principal을 바인딩합니다.
//! - fresh 모드 시 기존 세션을 안전하게 펜싱(대체)하고, recover 모드 시 선착순 복구를 보장합니다.
//! - 활성 세션 바인딩 여부에 따라 중앙 WebSocket에 실시간 presence(ONLINE/OFFLINE)를 전파합니다.

use crate::{
    central::{CentralIdentity, CentralState, ConnectionStatus},
    owner_process::OwnerProcess,
    protocol::PresenceStatus,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error};
use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::{
    sync::Mutex,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

/// 세션 임대 기본 유효 시간 (90초)
pub const LEASE_DURATION: Duration = Duration::from_secs(90);

pub(crate) fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// 공통 식별자 타입 (최대 128자, 제어 문자 및 유니코드 공백 제외)
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Identifier(String);

impl Identifier {
    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.chars().count() > 128
            || value.chars().any(|c| {
                matches!(
                    c as u32,
                    0x00..=0x20 | 0x7f..=0x9f | 0xa0 | 0x1680 | 0x2000..=0x200a |
                    0x2028 | 0x2029 | 0x202f | 0x205f | 0x3000 | 0xfeff
                )
            })
        {
            return Err("Invalid identifier");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn random() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl<'de> Deserialize<'de> for Identifier {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

/// 지원하는 AI 도구 유형 (Codex, Claude Code)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolType {
    Codex,
    ClaudeCode,
}

/// 검증된 사용자/팀 및 바인딩된 Agent 식별 정보
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Principal {
    pub user_id: Identifier,
    pub team_id: Identifier,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub agent_id: Option<Identifier>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_type: Option<ToolType>,
}

impl Principal {
    fn same_user_team(&self, other: &Self) -> bool {
        self.user_id == other.user_id && self.team_id == other.team_id
    }
}

/// 클라이언트에 노출되는 활성 세션 정보
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Session {
    pub session_id: Identifier,
    pub principal: Principal,
}

/// 세션 획득 모드
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaimMode {
    /// 신규 세션 생성 (동일 Agent의 이전 세션이 있으면 펜싱 및 교체)
    Fresh,
    /// 기존 세션 복구 (이전 세션이 살아있으면 재사용, 없으면 선착순 복구)
    Recover,
}

/// 중앙 신원 검증 에러 유형
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    Unauthenticated,
    Unavailable,
    Forbidden,
}

pub type IdentityFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Principal, IdentityError>> + Send + 'a>>;

/// 중앙 신원 및 Agent 소유권 확인 인터페이스
pub trait IdentityProvider: Send + Sync {
    fn resolve<'a>(&'a self, agent_id: Option<&'a Identifier>) -> IdentityFuture<'a>;
    fn requires_current_ready(&self) -> bool {
        false
    }
    fn requires_live_check(&self) -> bool {
        false
    }
}

/// Trust only identity supplied by the authenticated current central connection.
pub struct ReadyIdentityProvider {
    central: CentralState,
}

impl ReadyIdentityProvider {
    pub fn new(central: CentralState) -> Self {
        Self { central }
    }
}

fn ready_principal(
    status: ConnectionStatus,
    identity: Option<&CentralIdentity>,
) -> Result<Principal, IdentityError> {
    if status != ConnectionStatus::Connected {
        return Err(if status == ConnectionStatus::AuthFailed {
            IdentityError::Unauthenticated
        } else {
            IdentityError::Unavailable
        });
    }
    let identity = identity.ok_or(IdentityError::Unauthenticated)?;
    let (Some(user), Some(team)) = (identity.user_id, identity.team_id) else {
        return Err(IdentityError::Unauthenticated);
    };
    let id = |number: u64| {
        if number == 0 || number > i64::MAX as u64 {
            return Err(IdentityError::Unauthenticated);
        }
        Identifier::parse(number.to_string()).map_err(|_| IdentityError::Unauthenticated)
    };
    Ok(Principal {
        user_id: id(user)?,
        team_id: id(team)?,
        agent_id: Some(id(identity.agent_id)?),
        tool_type: None,
    })
}

impl IdentityProvider for ReadyIdentityProvider {
    fn requires_live_check(&self) -> bool {
        self.central.context_client.is_some()
    }
    fn requires_current_ready(&self) -> bool {
        true
    }

    fn resolve<'a>(&'a self, agent: Option<&'a Identifier>) -> IdentityFuture<'a> {
        Box::pin(async move {
            if let Some(client) = &self.central.context_client {
                let (generation, status, identity) = self.central.context_snapshot().await;
                if status != ConnectionStatus::Connected {
                    return Err(if status == ConnectionStatus::AuthFailed {
                        IdentityError::Unauthenticated
                    } else {
                        IdentityError::Unavailable
                    });
                }
                let identity = identity.ok_or(IdentityError::Unauthenticated)?;
                let context = match client.get().await {
                    Ok(context) => context,
                    Err(error) => {
                        if error == IdentityError::Unauthenticated {
                            let _ = self
                                .central
                                .confirm_context(generation, &identity, None)
                                .await;
                        }
                        return Err(error);
                    }
                };
                if context.agent_id != identity.agent_id
                    || identity
                        .user_id
                        .is_some_and(|id| id != context.owner_user_id)
                    || identity.team_id.is_some_and(|id| id != context.team_id)
                {
                    let _ = self
                        .central
                        .confirm_context(generation, &identity, None)
                        .await;
                    return Err(IdentityError::Forbidden);
                }
                self.central
                    .confirm_context(generation, &identity, Some(&context))
                    .await?;
            }
            self.central
                .with_ready_context(|status, identity, _| {
                    let mut principal = ready_principal(status, identity)?;
                    if let Some(agent) = agent {
                        if principal.agent_id.as_ref() != Some(agent) {
                            return Err(IdentityError::Forbidden);
                        }
                    } else {
                        // Agent registration/binding is explicit in the existing MCP contract.
                        principal.agent_id = None;
                    }
                    Ok(principal)
                })
                .await
        })
    }
}

/// 중앙 신원 연동 전 기본 프로바이더 (Unauthenticated 처리)
pub struct UnconfiguredIdentityProvider;

impl IdentityProvider for UnconfiguredIdentityProvider {
    fn resolve<'a>(&'a self, _agent_id: Option<&'a Identifier>) -> IdentityFuture<'a> {
        Box::pin(async { Err(IdentityError::Unauthenticated) })
    }
}

fn check_current_principal(
    expected: &Principal,
    current: Option<Result<Principal, IdentityError>>,
) -> Result<(), SessionError> {
    if let Some(current) = current {
        let current = current.map_err(SessionError::Identity)?;
        if !expected.same_user_team(&current)
            || (expected.agent_id.is_some() && expected.agent_id != current.agent_id)
        {
            return Err(SessionError::Identity(IdentityError::Forbidden));
        }
    }
    Ok(())
}

/// 세션 처리 중 발생하는 도메인 오류
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionError {
    /// 세션 만료 (TTL 90초 초과)
    Expired,
    /// 더 새로운 fresh 세션에 의해 대체(펜싱)됨
    Replaced,
    /// 인스턴스/Agent 충돌 (다른 인스턴스가 이미 슬롯 점유 등)
    Conflict,
    /// 요청한 Agent가 등록/바인딩되지 않음
    AgentUnregistered,
    /// 중앙 신원 검증 실패
    Identity(IdentityError),
}

/// 세션 레지스트리 내부 저장 항목
struct Entry {
    view: Session,
    instance_id: Identifier,
    expires_at: Instant,
    replaced: bool,
    /// MCP process that claimed the session, when it reported one (#108).
    owner: Option<OwnerProcess>,
}

/// Decides whether a recorded owner process has ended; replaceable in unit tests.
struct OwnerCheck(fn(&OwnerProcess) -> bool);
impl Default for OwnerCheck {
    fn default() -> Self {
        Self(OwnerProcess::is_gone)
    }
}

/// 인메모리 세션 레지스트리
#[derive(Default)]
struct Registry {
    owner_gone: OwnerCheck,
    /// sessionId -> Entry
    sessions: HashMap<Identifier, Entry>,
    /// instanceId -> sessionId (인스턴스별 단일 활성 세션 매핑)
    instances: HashMap<Identifier, Identifier>,
    /// agentId -> sessionId (Agent별 활성 소유 세션 매핑)
    owners: HashMap<Identifier, Identifier>,
    /// 세션 등록 이력 유무 (최초 관측 전에는 presence 미발행)
    observed: bool,
    /// 브로커 종료로 레지스트리 폐쇄 여부
    closed: bool,
    subject_epoch: u64,
}

impl Registry {
    fn sync_subject(&mut self, epoch: u64) {
        if self.subject_epoch != epoch {
            self.sessions.clear();
            self.instances.clear();
            self.owners.clear();
            self.subject_epoch = epoch;
        }
    }
    /// 만료된 세션 정리
    fn expire(&mut self, now: Instant) {
        // A session whose owner process has definitely ended expires now, not at the lease
        // end: a Host that kills its MCP child gives it no chance to release (#108).
        let gone = self.owner_gone.0;
        self.sessions
            .retain(|_, entry| entry.expires_at > now && !entry.owner.as_ref().is_some_and(gone));
        self.instances
            .retain(|_, id| self.sessions.contains_key(id));
        self.owners
            .retain(|_, id| self.sessions.get(id).is_some_and(|e| !e.replaced));
    }

    /// 활성 Agent 소유자 존재 여부에 따라 중앙 presence 발행
    fn publish_presence(&self, central: &CentralState) {
        if self.observed {
            central.set_presence(if self.owners.is_empty() {
                PresenceStatus::Offline
            } else {
                PresenceStatus::Online
            });
        }
    }

    /// 현재 인스턴스가 소유한 유효한 세션인지 검증
    fn current(&self, id: &Identifier, instance: &Identifier) -> Result<&Entry, SessionError> {
        let entry = self.sessions.get(id).ok_or(SessionError::Expired)?;
        if &entry.instance_id != instance {
            return Err(SessionError::Conflict);
        }
        if entry.replaced {
            return Err(SessionError::Replaced);
        }
        Ok(entry)
    }

    /// 세션 획득 로직 처리
    fn claim(
        &mut self,
        instance: Identifier,
        mode: ClaimMode,
        requested_agent: Option<&Identifier>,
        principal: Principal,
        now: Instant,
        lease: Duration,
    ) -> Result<Session, SessionError> {
        if self.closed {
            return Err(SessionError::Expired);
        }
        // 동일 instanceId로 이미 발급된 세션이 있는 경우 (멱등적 갱신)
        if let Some(id) = self.instances.get(&instance) {
            let entry = self.current(id, &instance)?;
            if !entry.view.principal.same_user_team(&principal) {
                return Err(SessionError::Identity(IdentityError::Forbidden));
            }
            if requested_agent.is_some()
                && entry.view.principal.agent_id.as_ref() != requested_agent
            {
                return Err(if entry.view.principal.agent_id.is_none() {
                    SessionError::AgentUnregistered
                } else {
                    SessionError::Conflict
                });
            }
            if principal.agent_id.is_some() && entry.view.principal.agent_id != principal.agent_id {
                return Err(SessionError::Conflict);
            }
            let id = id.clone();
            let entry = self.sessions.get_mut(&id).unwrap();
            entry.expires_at = now + lease;
            return Ok(entry.view.clone());
        }
        // 다른 인스턴스가 동일 Agent를 이미 점유 중인 경우
        if let Some(agent) = &principal.agent_id
            && let Some(owner_id) = self.owners.get(agent)
        {
            if !self
                .sessions
                .get(owner_id)
                .unwrap()
                .view
                .principal
                .same_user_team(&principal)
            {
                return Err(SessionError::Identity(IdentityError::Forbidden));
            }
            // recover 모드에서는 이미 존재하는 활성 슬롯을 빼앗을 수 없음
            if matches!(mode, ClaimMode::Recover) {
                return Err(SessionError::Conflict);
            }
            // fresh 모드인 경우 이전 세션을 펜싱(replaced) 처리
            self.sessions.get_mut(owner_id).unwrap().replaced = true;
        }
        // 신규 세션 등록
        let id = Identifier::random();
        let view = Session {
            session_id: id.clone(),
            principal,
        };
        if let Some(agent) = &view.principal.agent_id {
            self.owners.insert(agent.clone(), id.clone());
        }
        self.instances.insert(instance.clone(), id.clone());
        self.sessions.insert(
            id,
            Entry {
                view: view.clone(),
                instance_id: instance,
                expires_at: now + lease,
                replaced: false,
                owner: None,
            },
        );
        self.observed = true;
        Ok(view)
    }

    /// 세션에 검증된 Agent 바인딩
    fn bind(
        &mut self,
        id: &Identifier,
        instance: &Identifier,
        principal: Principal,
        now: Instant,
        lease: Duration,
    ) -> Result<Session, SessionError> {
        let entry = self.current(id, instance)?;
        if !entry.view.principal.same_user_team(&principal) {
            return Err(SessionError::Identity(IdentityError::Forbidden));
        }
        let agent = principal
            .agent_id
            .as_ref()
            .ok_or(SessionError::Identity(IdentityError::Forbidden))?;
        if let Some(previous) = &entry.view.principal.agent_id
            && previous != agent
        {
            return Err(SessionError::Conflict);
        }
        if self.owners.get(agent).is_some_and(|owner| owner != id) {
            return Err(SessionError::Conflict);
        }
        if entry.view.principal.tool_type.is_some() && principal.tool_type.is_none() {
            return Err(SessionError::Identity(IdentityError::Forbidden));
        }
        self.owners.insert(agent.clone(), id.clone());
        let entry = self.sessions.get_mut(id).unwrap();
        entry.view.principal = principal;
        entry.expires_at = now + lease;
        Ok(entry.view.clone())
    }

    /// 세션 임대 기간 연장 (하트비트)
    fn renew(
        &mut self,
        id: &Identifier,
        instance: &Identifier,
        now: Instant,
        lease: Duration,
    ) -> Result<(), SessionError> {
        self.current(id, instance)?;
        self.sessions.get_mut(id).unwrap().expires_at = now + lease;
        Ok(())
    }

    /// 세션 명시적 해제
    fn release(&mut self, id: &Identifier, instance: &Identifier) -> Result<(), SessionError> {
        self.current(id, instance)?;
        let entry = self.sessions.remove(id).unwrap();
        self.instances.remove(&entry.instance_id);
        if let Some(agent) = &entry.view.principal.agent_id
            && self.owners.get(agent) == Some(id)
        {
            self.owners.remove(agent);
        }
        Ok(())
    }

    fn authorize_context(
        &self,
        id: &Identifier,
        expected: &Principal,
        allow_unbound: bool,
    ) -> Result<Principal, SessionError> {
        if self.closed {
            return Err(SessionError::Expired);
        }
        let entry = self.sessions.get(id).ok_or(SessionError::Expired)?;
        if entry.replaced {
            return Err(SessionError::Replaced);
        }
        if entry.view.principal.agent_id.is_none() && !allow_unbound {
            return Err(SessionError::AgentUnregistered);
        }
        if &entry.view.principal != expected {
            return Err(SessionError::Identity(IdentityError::Forbidden));
        }
        Ok(entry.view.principal.clone())
    }
}

/// 외부에서 접근하는 세션 관리자 (Thread-safe)
#[derive(Clone)]
pub struct SessionManager {
    registry: Arc<Mutex<Registry>>,
    identity: Arc<dyn IdentityProvider>,
    central: CentralState,
    lease: Duration,
}

impl SessionManager {
    /// Inspect without expiring entries, claiming, renewing, binding or publishing presence.
    pub(crate) async fn inspect(
        &self,
        instance: Option<&Identifier>,
        context: &crate::agent_context::AgentContext,
    ) -> Result<&'static str, IdentityError> {
        let registry = self.registry.lock().await;
        self.central
            .with_ready_context(|status, identity, epoch| {
                let identity = identity.ok_or(IdentityError::Unavailable)?;
                if status != ConnectionStatus::Connected {
                    return Err(IdentityError::Unavailable);
                }
                if identity.agent_id != context.agent_id
                    || identity
                        .user_id
                        .is_some_and(|id| id != context.owner_user_id)
                    || identity.team_id.is_some_and(|id| id != context.team_id)
                {
                    return Err(IdentityError::Forbidden);
                }
                if registry.closed || registry.subject_epoch != epoch {
                    return Ok("unbound");
                }
                let now = Instant::now();
                let agent = Identifier::parse(context.agent_id.to_string()).unwrap();
                let owned = registry
                    .owners
                    .get(&agent)
                    .and_then(|id| registry.sessions.get(id))
                    .filter(|e| e.expires_at > now && !e.replaced);
                let Some(instance) = instance else {
                    let Some(entry) = owned else {
                        return Ok("unbound");
                    };
                    let principal = &entry.view.principal;
                    return Ok(
                        if principal.user_id.as_str() == context.owner_user_id.to_string()
                            && principal.team_id.as_str() == context.team_id.to_string()
                            && principal.agent_id.as_ref() == Some(&agent)
                        {
                            "bound"
                        } else {
                            "conflict"
                        },
                    );
                };
                if let Some(entry) = registry
                    .instances
                    .get(instance)
                    .and_then(|id| registry.sessions.get(id))
                    && entry.expires_at > now
                {
                    if entry.replaced {
                        return Ok("conflict");
                    }
                    let p = &entry.view.principal;
                    if p.user_id.as_str() != context.owner_user_id.to_string()
                        || p.team_id.as_str() != context.team_id.to_string()
                        || p.agent_id
                            .as_ref()
                            .is_some_and(|id| id.as_str() != context.agent_id.to_string())
                    {
                        return Err(IdentityError::Forbidden);
                    }
                    return Ok(if p.agent_id.is_some() {
                        "bound"
                    } else if owned.is_some() {
                        "conflict"
                    } else {
                        "unbound"
                    });
                }
                Ok(if owned.is_some() {
                    "conflict"
                } else {
                    "unbound"
                })
            })
            .await
    }

    pub fn new(identity: Arc<dyn IdentityProvider>, central: CentralState) -> Self {
        Self {
            registry: Arc::new(Mutex::new(Registry::default())),
            identity,
            central,
            lease: LEASE_DURATION,
        }
    }

    async fn with_context<T>(
        &self,
        registry: &mut Registry,
        operation: impl FnOnce(
            &mut Registry,
            Option<Result<Principal, IdentityError>>,
        ) -> Result<T, SessionError>,
    ) -> Result<T, SessionError> {
        if !self.identity.requires_current_ready() {
            let result = operation(registry, None);
            registry.publish_presence(&self.central);
            return result;
        }
        self.central
            .with_ready_context(|status, identity, epoch| {
                registry.sync_subject(epoch);
                let current = ready_principal(status, identity);
                let confirmed = current.is_ok();
                let result = operation(registry, Some(current));
                // A legacy reconnect must not republish old ONLINE ownership
                // before live context confirms the current WS subject.
                if self.identity.requires_live_check() && !confirmed {
                    if registry.observed {
                        self.central.set_presence(PresenceStatus::Offline);
                    }
                } else {
                    registry.publish_presence(&self.central);
                }
                result
            })
            .await
    }

    /// 중앙 신원 해석 및 요청 Agent 일치 여부 검증
    async fn resolve(&self, agent: Option<&Identifier>) -> Result<Principal, SessionError> {
        let principal = timeout(Duration::from_secs(5), self.identity.resolve(agent))
            .await
            .map_err(|_| SessionError::Identity(IdentityError::Unavailable))?
            .map_err(SessionError::Identity)?;
        if agent.is_some() && principal.agent_id.as_ref() != agent {
            return Err(SessionError::Identity(IdentityError::Forbidden));
        }
        Ok(principal)
    }

    /// 세션 생성 또는 복구 (신원 확인 후 레지스트리 갱신 및 presence 전파)
    pub async fn claim(
        &self,
        instance: Identifier,
        mode: ClaimMode,
        agent: Option<Identifier>,
    ) -> Result<Session, SessionError> {
        self.claim_with_owner(instance, mode, agent, None).await
    }

    /// Like `claim`, recording the claiming MCP process so the session ends with it (#108).
    /// The owner is set only on a session that has none; it never moves to another process.
    pub async fn claim_with_owner(
        &self,
        instance: Identifier,
        mode: ClaimMode,
        agent: Option<Identifier>,
        owner: Option<OwnerProcess>,
    ) -> Result<Session, SessionError> {
        let principal = self.resolve(agent.as_ref()).await?;
        let mut registry = self.registry.lock().await;
        self.with_context(&mut registry, |registry, current| {
            let now = Instant::now();
            registry.expire(now);
            check_current_principal(&principal, current)?;
            let session =
                registry.claim(instance, mode, agent.as_ref(), principal, now, self.lease)?;
            if let Some(entry) = registry.sessions.get_mut(&session.session_id) {
                entry.owner = entry.owner.or(owner);
            }
            Ok(session)
        })
        .await
    }

    /// 세션에 Agent 바인딩 (활성화 및 ONLINE presence 전파)
    pub async fn bind(
        &self,
        id: &Identifier,
        instance: &Identifier,
        agent: Identifier,
    ) -> Result<Session, SessionError> {
        // 비동기 작업 전 세션 유효성을 먼저 확인하여 만료된 요청 차단
        {
            let mut registry = self.registry.lock().await;
            self.with_context(&mut registry, |registry, _| {
                registry.expire(Instant::now());
                registry.current(id, instance).map(|_| ())
            })
            .await?;
        }
        let principal = self.resolve(Some(&agent)).await?;
        let mut registry = self.registry.lock().await;
        self.with_context(&mut registry, |registry, current| {
            let now = Instant::now();
            registry.expire(now);
            registry.current(id, instance)?;
            check_current_principal(&principal, current)?;
            registry.bind(id, instance, principal, now, self.lease)
        })
        .await
    }

    /// 세션 임대 기간 연장 (heartbeat)
    pub async fn heartbeat(
        &self,
        id: &Identifier,
        instance: &Identifier,
    ) -> Result<(), SessionError> {
        // Reject stale or foreign local sessions before any central request.
        // Do not keep the registry lock across the identity provider await.
        {
            let mut registry = self.registry.lock().await;
            self.with_context(&mut registry, |registry, _| {
                registry.expire(Instant::now());
                registry.current(id, instance).map(|_| ())
            })
            .await?;
        }
        if self.identity.requires_live_check() {
            self.resolve(None).await?;
        }
        let mut registry = self.registry.lock().await;
        self.with_context(&mut registry, |registry, current| {
            let now = Instant::now();
            registry.expire(now);
            check_current_principal(&registry.current(id, instance)?.view.principal, current)?;
            registry.renew(id, instance, now, self.lease)
        })
        .await
    }

    /// 비즈니스 액터(호출 주체)의 유효성을 실시간 세션 레지스트리에서 검증
    ///
    /// 호출자가 전달한 컨텍스트를 그대로 신뢰하지 않고, 브로커에 등록된 실제 활성 세션 상태와 대조합니다.
    /// 세션 만료 시간(lease)을 갱신하지 않고 세션 소유권 및 중앙 서버 신원 일치 여부만 확인합니다.
    pub async fn authorize(
        &self,
        id: &Identifier,
        expected: &Principal,
    ) -> Result<Principal, SessionError> {
        self.authorize_context(id, expected, false).await
    }

    /// Authorization for reads of locally stored data only. Keeps the same local
    /// ownership, expiry, replacement and subject-change checks as `authorize`, but
    /// never requires a live central connection and never performs central IO: a
    /// transient disconnect or an unconfirmed legacy READY of the same Agent is
    /// served from the verified local session. Authentication failure and a
    /// different connected subject still reject.
    pub async fn authorize_local_read(
        &self,
        id: &Identifier,
        expected: &Principal,
    ) -> Result<Principal, SessionError> {
        let mut registry = self.registry.lock().await;
        if !self.identity.requires_current_ready() {
            registry.expire(Instant::now());
            let result = registry.authorize_context(id, expected, false);
            registry.publish_presence(&self.central);
            return result;
        }
        self.central
            .with_ready_context(|status, identity, epoch| {
                // A subject change or authentication rejection bumps the epoch and
                // invalidates every local session before any read.
                registry.sync_subject(epoch);
                registry.expire(Instant::now());
                let confirmed = ready_principal(status, identity).is_ok();
                let result =
                    registry
                        .authorize_context(id, expected, false)
                        .and_then(|principal| {
                            if status == ConnectionStatus::AuthFailed {
                                return Err(SessionError::Identity(IdentityError::Unauthenticated));
                            }
                            // Compare only what the current connection asserts; a legacy
                            // READY without user/team is not resolved here.
                            let differs = |asserted: Option<u64>, local: Option<&Identifier>| {
                                asserted.is_some_and(|value| {
                                    local.is_none_or(|local| value.to_string() != local.as_str())
                                })
                            };
                            if status == ConnectionStatus::Connected
                                && identity.is_some_and(|current| {
                                    differs(Some(current.agent_id), principal.agent_id.as_ref())
                                        || differs(current.user_id, Some(&principal.user_id))
                                        || differs(current.team_id, Some(&principal.team_id))
                                })
                            {
                                return Err(SessionError::Identity(IdentityError::Forbidden));
                            }
                            Ok(principal)
                        });
                // Same presence rule as other registry operations: an unconfirmed
                // legacy reconnect never republishes old ONLINE ownership.
                if self.identity.requires_live_check() && !confirmed {
                    if registry.observed {
                        self.central.set_presence(PresenceStatus::Offline);
                    }
                } else {
                    registry.publish_presence(&self.central);
                }
                result
            })
            .await
    }

    /// Only register_agent may confirm the current bootstrap Agent before bind.
    pub async fn authorize_registration(
        &self,
        id: &Identifier,
        expected: &Principal,
    ) -> Result<Principal, SessionError> {
        self.authorize_context(id, expected, true).await
    }

    pub async fn verify_registration_agent(
        &self,
        id: &Identifier,
        expected: &Principal,
        agent: &Identifier,
    ) -> Result<(), SessionError> {
        self.authorize_registration(id, expected).await?;
        let actual = self.resolve(Some(agent)).await?;
        if !actual.same_user_team(expected) {
            return Err(SessionError::Identity(IdentityError::Forbidden));
        }
        self.authorize_registration(id, expected).await?;
        Ok(())
    }

    async fn authorize_context(
        &self,
        id: &Identifier,
        expected: &Principal,
        allow_unbound: bool,
    ) -> Result<Principal, SessionError> {
        // Validate local ownership and caller context before resolving a legacy
        // reconnect. Unrelated/stale requests must not trigger central IO.
        {
            let mut registry = self.registry.lock().await;
            self.with_context(&mut registry, |registry, _| {
                registry.expire(Instant::now());
                registry
                    .authorize_context(id, expected, allow_unbound)
                    .map(|_| ())
            })
            .await?;
        }
        if self.identity.requires_live_check() {
            let needs_context = self
                .central
                .with_ready_context(|status, identity, _| {
                    status == ConnectionStatus::Connected
                        && identity.is_some_and(|identity| {
                            identity.user_id.is_none() || identity.team_id.is_none()
                        })
                })
                .await;
            if needs_context {
                self.resolve(None).await?;
            }
        }
        let mut registry = self.registry.lock().await;
        self.with_context(&mut registry, |registry, current| {
            // Recheck after the await: release, replacement, expiry or a central
            // subject change may have happened while resolving identity.
            registry.expire(Instant::now());
            let principal = registry.authorize_context(id, expected, allow_unbound)?;
            check_current_principal(&principal, current)?;
            Ok(principal)
        })
        .await
    }

    /// 세션 명시적 해제 및 OFFLINE presence 전파
    pub async fn release(
        &self,
        id: &Identifier,
        instance: &Identifier,
    ) -> Result<(), SessionError> {
        let mut registry = self.registry.lock().await;
        self.with_context(&mut registry, |registry, _| {
            registry.expire(Instant::now());
            registry.release(id, instance)
        })
        .await?;
        Ok(())
    }

    /// 만료 주기 실행
    pub async fn expire(&self) {
        let mut registry = self.registry.lock().await;
        let _ = self
            .with_context(&mut registry, |registry, _| {
                registry.expire(Instant::now());
                Ok(())
            })
            .await;
    }

    /// 백그라운드 세션 만료 타이머 태스크 (HTTP 요청이 없어도 90초 경과 시 자동 OFFLINE 전파)
    pub async fn run_expiration(self, cancel: CancellationToken) {
        let mut subjects = self.central.subject_changes();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let mut registry = self.registry.lock().await;
                    registry.closed = true;
                    registry.sessions.clear();
                    registry.instances.clear();
                    registry.owners.clear();
                    registry.publish_presence(&self.central);
                    return;
                }
                _ = tick.tick() => self.expire().await,
                changed = subjects.changed(), if self.identity.requires_current_ready() => {
                    if changed.is_err() { return; }
                    self.expire().await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::central::ConnectionStatus;
    use tokio::sync::{Notify, RwLock};

    async fn ready_as(
        central: &CentralState,
        user: u64,
        team: u64,
        agent: u64,
    ) -> crate::central::SessionHandle {
        let handle = central.begin_session().await;
        handle
            .set_identity(CentralIdentity {
                connection_id: uuid::Uuid::new_v4().to_string(),
                agent_id: agent,
                user_id: Some(user),
                team_id: Some(team),
            })
            .await;
        handle.set_status(ConnectionStatus::Connected).await;
        handle
    }

    #[tokio::test]
    async fn ready_provider_requires_current_complete_identity_and_checks_agent() {
        let central = central();
        let provider = ReadyIdentityProvider::new(central.clone());
        assert_eq!(
            provider.resolve(None).await,
            Err(IdentityError::Unavailable)
        );
        let legacy = central.begin_session().await;
        legacy
            .set_identity(CentralIdentity {
                connection_id: "legacy".into(),
                agent_id: 12,
                user_id: None,
                team_id: None,
            })
            .await;
        legacy.set_status(ConnectionStatus::Connected).await;
        assert_eq!(
            provider.resolve(None).await,
            Err(IdentityError::Unauthenticated)
        );
        let handle = ready_as(&central, 3, 1, 12).await;
        assert_eq!(
            provider.resolve(None).await.unwrap(),
            Principal {
                user_id: id("3"),
                team_id: id("1"),
                agent_id: None,
                tool_type: None
            }
        );
        assert_eq!(
            provider.resolve(Some(&id("12"))).await.unwrap().agent_id,
            Some(id("12"))
        );
        assert_eq!(
            provider.resolve(Some(&id("13"))).await,
            Err(IdentityError::Forbidden)
        );
        handle.clear_identity().await;
        handle.set_status(ConnectionStatus::Reconnecting).await;
        assert_eq!(
            provider.resolve(None).await,
            Err(IdentityError::Unavailable)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn inspection_never_renews_expires_claims_or_publishes_presence() {
        let central = central();
        ready_as(&central, 3, 1, 12).await;
        let manager = SessionManager::new(
            Arc::new(ReadyIdentityProvider::new(central.clone())),
            central.clone(),
        );
        let owner = id("owner");
        let session = manager
            .claim(owner.clone(), ClaimMode::Recover, Some(id("12")))
            .await
            .unwrap();
        let context =
            serde_json::from_value(serde_json::json!({"agentId":12,"ownerUserId":3,"teamId":1}))
                .unwrap();
        let expiry = manager.registry.lock().await.sessions[&session.session_id].expires_at;
        manager
            .claim(id("unbound"), ClaimMode::Recover, None)
            .await
            .unwrap();
        assert_eq!(
            manager
                .inspect(Some(&id("unbound")), &context)
                .await
                .unwrap(),
            "conflict"
        );
        let presence = central.presence().await;
        assert_eq!(
            manager.inspect(Some(&owner), &context).await.unwrap(),
            "bound"
        );
        assert_eq!(manager.inspect(None, &context).await.unwrap(), "bound");
        assert_eq!(
            manager.inspect(Some(&id("other")), &context).await.unwrap(),
            "conflict"
        );
        assert_eq!(
            manager.registry.lock().await.sessions[&session.session_id].expires_at,
            expiry
        );
        tokio::time::advance(Duration::from_secs(91)).await;
        assert_eq!(
            manager.inspect(Some(&owner), &context).await.unwrap(),
            "unbound"
        );
        assert_eq!(manager.inspect(None, &context).await.unwrap(), "unbound");
        assert_eq!(manager.registry.lock().await.sessions.len(), 2);
        assert_eq!(central.presence().await, presence);
    }

    #[tokio::test]
    async fn same_ready_subject_preserves_sessions_and_changed_subject_expires_them() {
        let central = central();
        let manager = SessionManager::new(
            Arc::new(ReadyIdentityProvider::new(central.clone())),
            central.clone(),
        );
        let old = ready_as(&central, 3, 1, 12).await;
        let session = manager
            .claim(id("host"), ClaimMode::Fresh, Some(id("12")))
            .await
            .unwrap();
        assert_eq!(central.presence().await, Some(PresenceStatus::Online));
        old.clear_identity().await;
        old.set_status(ConnectionStatus::Reconnecting).await;
        assert_eq!(
            manager.heartbeat(&session.session_id, &id("host")).await,
            Err(SessionError::Identity(IdentityError::Unavailable))
        );
        ready_as(&central, 3, 1, 12).await;
        manager
            .heartbeat(&session.session_id, &id("host"))
            .await
            .unwrap();
        assert_eq!(
            manager
                .claim(id("host"), ClaimMode::Recover, Some(id("12")))
                .await
                .unwrap()
                .session_id,
            session.session_id
        );
        // A -> B -> A also invalidates old sessions even when the watcher skips B.
        ready_as(&central, 3, 2, 12).await;
        ready_as(&central, 3, 1, 12).await;
        assert_eq!(central.presence().await, Some(PresenceStatus::Offline));
        assert_eq!(
            manager.heartbeat(&session.session_id, &id("host")).await,
            Err(SessionError::Expired)
        );
        let new = manager
            .claim(id("host"), ClaimMode::Recover, Some(id("12")))
            .await
            .unwrap();
        assert_ne!(new.session_id, session.session_id);
        assert_eq!(
            manager.release(&session.session_id, &id("host")).await,
            Err(SessionError::Expired)
        );
        assert_eq!(central.presence().await, Some(PresenceStatus::Online));
    }

    #[tokio::test]
    async fn changed_ready_subject_clears_ownership_without_http_requests() {
        let central = central();
        let manager = SessionManager::new(
            Arc::new(ReadyIdentityProvider::new(central.clone())),
            central.clone(),
        );
        ready_as(&central, 3, 1, 12).await;
        manager
            .claim(id("host"), ClaimMode::Fresh, Some(id("12")))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let expiration = tokio::spawn(manager.clone().run_expiration(cancel.clone()));
        ready_as(&central, 4, 1, 13).await;
        timeout(Duration::from_secs(2), async {
            loop {
                if manager.registry.lock().await.sessions.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(central.presence().await, Some(PresenceStatus::Offline));
        cancel.cancel();
        expiration.await.unwrap();
    }

    fn id(value: &str) -> Identifier {
        Identifier::parse(value).unwrap()
    }
    fn principal(agent: Option<&str>) -> Principal {
        Principal {
            user_id: id("user-a"),
            team_id: id("team-a"),
            agent_id: agent.map(id),
            tool_type: agent.map(|_| ToolType::Codex),
        }
    }
    fn central() -> CentralState {
        CentralState::new(Arc::new(RwLock::new(ConnectionStatus::Connecting)))
    }
    fn claim(
        registry: &mut Registry,
        instance: &str,
        mode: ClaimMode,
        agent: Option<&str>,
        now: Instant,
    ) -> Result<Session, SessionError> {
        let requested = agent.map(id);
        registry.claim(
            id(instance),
            mode,
            requested.as_ref(),
            principal(agent),
            now,
            LEASE_DURATION,
        )
    }

    /// #108: a session whose recorded owner process ended is expired before its lease, so
    /// another instance can bind the Agent; an owner that is still alive keeps it.
    #[test]
    fn a_session_whose_owner_process_ended_expires_before_the_lease() {
        let now = Instant::now();
        let mut registry = Registry {
            owner_gone: OwnerCheck(|owner| *owner == OwnerProcess::synthetic(41, 7)),
            ..Registry::default()
        };
        let dead = claim(&mut registry, "codex", ClaimMode::Recover, Some("12"), now).unwrap();
        registry.sessions.get_mut(&dead.session_id).unwrap().owner =
            Some(OwnerProcess::synthetic(41, 7));
        let other = claim(&mut registry, "next", ClaimMode::Recover, None, now).unwrap();
        // Before expire: the dead owner still blocks the bind.
        assert!(matches!(
            registry.bind(
                &other.session_id,
                &id("next"),
                principal(Some("12")),
                now,
                LEASE_DURATION
            ),
            Err(SessionError::Conflict)
        ));
        registry.expire(now);
        assert!(!registry.sessions.contains_key(&dead.session_id));
        assert!(!registry.instances.contains_key(&id("codex")));
        let bound = registry
            .bind(
                &other.session_id,
                &id("next"),
                principal(Some("12")),
                now,
                LEASE_DURATION,
            )
            .unwrap();
        assert_eq!(bound.principal.agent_id, Some(id("12")));

        // A live owner (and a session without an owner) keeps the normal lease rules.
        let mut registry = Registry {
            owner_gone: OwnerCheck(|_| false),
            ..Registry::default()
        };
        let live = claim(&mut registry, "claude", ClaimMode::Recover, Some("12"), now).unwrap();
        registry.sessions.get_mut(&live.session_id).unwrap().owner =
            Some(OwnerProcess::synthetic(42, 9));
        registry.expire(now + Duration::from_secs(1));
        assert!(registry.sessions.contains_key(&live.session_id));
        registry.expire(now + LEASE_DURATION);
        assert!(!registry.sessions.contains_key(&live.session_id));
    }

    struct TestIdentity;
    impl IdentityProvider for TestIdentity {
        fn resolve<'a>(&'a self, agent: Option<&'a Identifier>) -> IdentityFuture<'a> {
            Box::pin(async move { Ok(principal(agent.map(Identifier::as_str))) })
        }
    }

    #[test]
    fn shared_identity_fixtures_and_optional_fields_match_mcp() {
        let fixtures: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../contracts/local-broker/v1/identity-fixtures.json"
        ))
        .unwrap();
        for fixture in fixtures.as_array().unwrap() {
            let result = match fixture["type"].as_str().unwrap() {
                "principal" => {
                    serde_json::from_value::<Principal>(fixture["value"].clone()).is_ok()
                }
                "session" => serde_json::from_value::<Session>(fixture["value"].clone()).is_ok(),
                _ => continue,
            };
            assert_eq!(
                result,
                fixture["valid"].as_bool().unwrap(),
                "{}",
                fixture["name"]
            );
        }
        for field in ["agentId", "toolType"] {
            let mut value = serde_json::to_value(principal(None)).unwrap();
            value[field] = serde_json::Value::Null;
            assert!(serde_json::from_value::<Principal>(value).is_err());
        }
        assert!(Identifier::parse("😀".repeat(128)).is_ok());
        assert!(Identifier::parse("😀".repeat(129)).is_err());
    }

    #[test]
    fn same_instance_claim_is_idempotent_but_cannot_change_agent() {
        let mut registry = Registry::default();
        let now = Instant::now();
        let first = claim(
            &mut registry,
            "instance-a",
            ClaimMode::Fresh,
            Some("11"),
            now,
        )
        .unwrap();
        assert_eq!(
            first,
            claim(
                &mut registry,
                "instance-a",
                ClaimMode::Recover,
                Some("11"),
                now + Duration::from_secs(30)
            )
            .unwrap()
        );
        assert_eq!(
            first,
            claim(
                &mut registry,
                "instance-a",
                ClaimMode::Fresh,
                Some("11"),
                now
            )
            .unwrap()
        );
        assert_eq!(
            claim(
                &mut registry,
                "instance-a",
                ClaimMode::Fresh,
                Some("12"),
                now
            ),
            Err(SessionError::Conflict)
        );
        assert_eq!(registry.sessions.len(), 1);
    }

    #[test]
    fn fresh_replacement_fences_old_claim_heartbeat_and_delete() {
        let mut registry = Registry::default();
        let now = Instant::now();
        let old = claim(&mut registry, "old", ClaimMode::Fresh, Some("11"), now).unwrap();
        let new = claim(&mut registry, "new", ClaimMode::Fresh, Some("11"), now).unwrap();
        assert_ne!(old.session_id, new.session_id);
        assert_eq!(
            registry.renew(&old.session_id, &id("old"), now, LEASE_DURATION),
            Err(SessionError::Replaced)
        );
        assert_eq!(
            registry.release(&old.session_id, &id("old")),
            Err(SessionError::Replaced)
        );
        assert_eq!(
            claim(&mut registry, "old", ClaimMode::Fresh, Some("11"), now),
            Err(SessionError::Replaced)
        );
        assert_eq!(
            claim(&mut registry, "old", ClaimMode::Recover, Some("11"), now),
            Err(SessionError::Replaced)
        );
        assert_eq!(registry.owners.get(&id("11")), Some(&new.session_id));
    }

    #[test]
    fn restart_first_recover_wins_and_agents_have_separate_slots() {
        let now = Instant::now();
        let mut registry = Registry::default();
        let previous = claim(&mut registry, "previous", ClaimMode::Fresh, Some("11"), now).unwrap();
        let mut restarted = Registry::default();
        let winner = claim(
            &mut restarted,
            "winner",
            ClaimMode::Recover,
            Some("11"),
            now,
        )
        .unwrap();
        assert_ne!(previous.session_id, winner.session_id);
        assert_eq!(
            claim(
                &mut restarted,
                "previous",
                ClaimMode::Recover,
                Some("11"),
                now
            ),
            Err(SessionError::Conflict)
        );
        assert!(
            claim(
                &mut restarted,
                "other-agent",
                ClaimMode::Recover,
                Some("12"),
                now
            )
            .is_ok()
        );
        assert_eq!(restarted.owners.len(), 2);
    }

    #[test]
    fn lease_expires_at_90_seconds_and_renewal_uses_broker_time() {
        let now = Instant::now();
        let mut registry = Registry::default();
        let first = claim(&mut registry, "a", ClaimMode::Fresh, Some("11"), now).unwrap();
        registry.expire(now + Duration::from_secs(89));
        assert!(registry.current(&first.session_id, &id("a")).is_ok());
        registry
            .renew(
                &first.session_id,
                &id("a"),
                now + Duration::from_secs(89),
                LEASE_DURATION,
            )
            .unwrap();
        registry.expire(now + Duration::from_secs(90));
        assert!(registry.current(&first.session_id, &id("a")).is_ok());
        registry.expire(now + Duration::from_secs(179));
        assert_eq!(
            registry.renew(
                &first.session_id,
                &id("a"),
                now + Duration::from_secs(179),
                LEASE_DURATION
            ),
            Err(SessionError::Expired)
        );
        assert!(
            registry.sessions.is_empty()
                && registry.instances.is_empty()
                && registry.owners.is_empty()
        );
    }

    #[test]
    fn unbound_bind_cannot_steal_or_switch_agent_or_user() {
        let now = Instant::now();
        let mut registry = Registry::default();
        let owner = claim(&mut registry, "owner", ClaimMode::Fresh, Some("11"), now).unwrap();
        let unbound = claim(&mut registry, "unbound", ClaimMode::Fresh, None, now).unwrap();
        assert_eq!(
            registry.bind(
                &unbound.session_id,
                &id("unbound"),
                principal(Some("11")),
                now,
                LEASE_DURATION
            ),
            Err(SessionError::Conflict)
        );
        assert_eq!(
            registry.release(&owner.session_id, &id("unbound")),
            Err(SessionError::Conflict)
        );
        registry.release(&owner.session_id, &id("owner")).unwrap();
        let bound = registry
            .bind(
                &unbound.session_id,
                &id("unbound"),
                principal(Some("11")),
                now,
                LEASE_DURATION,
            )
            .unwrap();
        assert_eq!(bound.session_id, unbound.session_id);
        assert_eq!(
            registry.bind(
                &bound.session_id,
                &id("unbound"),
                principal(Some("12")),
                now,
                LEASE_DURATION
            ),
            Err(SessionError::Conflict)
        );
        let mut foreign = principal(Some("11"));
        foreign.user_id = id("other-user");
        assert_eq!(
            registry.bind(
                &bound.session_id,
                &id("unbound"),
                foreign.clone(),
                now,
                LEASE_DURATION
            ),
            Err(SessionError::Identity(IdentityError::Forbidden))
        );
        assert_eq!(
            registry.claim(
                id("foreign"),
                ClaimMode::Fresh,
                Some(&id("11")),
                foreign,
                now,
                LEASE_DURATION
            ),
            Err(SessionError::Identity(IdentityError::Forbidden))
        );
    }

    #[tokio::test]
    async fn presence_tracks_bound_owners_and_diagnostics_remain_unknown() {
        let central = central();
        let manager = SessionManager::new(Arc::new(TestIdentity), central.clone());
        manager.expire().await;
        assert_eq!(central.presence().await, None);
        let a = manager
            .claim(id("a"), ClaimMode::Fresh, None)
            .await
            .unwrap();
        assert_eq!(central.presence().await, Some(PresenceStatus::Offline));
        manager
            .bind(&a.session_id, &id("a"), id("11"))
            .await
            .unwrap();
        let b = manager
            .claim(id("b"), ClaimMode::Fresh, Some(id("12")))
            .await
            .unwrap();
        assert_eq!(central.presence().await, Some(PresenceStatus::Online));
        manager.release(&a.session_id, &id("a")).await.unwrap();
        assert_eq!(central.presence().await, Some(PresenceStatus::Online));
        manager.release(&b.session_id, &id("b")).await.unwrap();
        assert_eq!(central.presence().await, Some(PresenceStatus::Offline));
    }

    #[tokio::test]
    async fn concurrent_recover_claims_have_exactly_one_owner() {
        let manager = SessionManager::new(Arc::new(TestIdentity), central());
        let mut tasks = tokio::task::JoinSet::new();
        for i in 0..12 {
            let manager = manager.clone();
            tasks.spawn(async move {
                manager
                    .claim(
                        id(&format!("instance-{i}")),
                        ClaimMode::Recover,
                        Some(id("11")),
                    )
                    .await
            });
        }
        let mut successes = 0;
        while let Some(result) = tasks.join_next().await {
            match result.unwrap() {
                Ok(_) => successes += 1,
                Err(error) => assert_eq!(error, SessionError::Conflict),
            }
        }
        assert_eq!(successes, 1);
        let mut repeated = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let manager = manager.clone();
            repeated.spawn(async move {
                manager
                    .claim(id("same-instance"), ClaimMode::Fresh, None)
                    .await
                    .unwrap()
                    .session_id
            });
        }
        let mut ids = std::collections::HashSet::new();
        while let Some(result) = repeated.join_next().await {
            ids.insert(result.unwrap());
        }
        assert_eq!(ids.len(), 1);
    }

    #[tokio::test]
    async fn abandoned_session_expires_without_an_http_request() {
        let central = central();
        let mut manager = SessionManager::new(Arc::new(TestIdentity), central.clone());
        manager.lease = Duration::from_millis(30); // Internal test timing only; public leases stay at 90s.
        let session = manager
            .claim(id("a"), ClaimMode::Fresh, Some(id("11")))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let runner = tokio::spawn(manager.clone().run_expiration(cancel.clone()));
        timeout(Duration::from_secs(3), async {
            while central.presence().await != Some(PresenceStatus::Offline) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            manager.heartbeat(&session.session_id, &id("a")).await,
            Err(SessionError::Expired)
        );
        cancel.cancel();
        runner.await.unwrap();
    }

    struct DelayedIdentity {
        started: Notify,
        release: Notify,
    }
    impl IdentityProvider for DelayedIdentity {
        fn resolve<'a>(&'a self, agent: Option<&'a Identifier>) -> IdentityFuture<'a> {
            Box::pin(async move {
                if agent.is_some() {
                    self.started.notify_one();
                    self.release.notified().await;
                }
                Ok(principal(agent.map(Identifier::as_str)))
            })
        }
    }

    #[tokio::test]
    async fn late_identity_reply_cannot_resurrect_released_or_shutdown_session() {
        let provider = Arc::new(DelayedIdentity {
            started: Notify::new(),
            release: Notify::new(),
        });
        let central = central();
        let manager = SessionManager::new(provider.clone(), central.clone());
        let session = manager
            .claim(id("a"), ClaimMode::Fresh, None)
            .await
            .unwrap();
        let copy = manager.clone();
        let session_id = session.session_id.clone();
        let binding = tokio::spawn(async move { copy.bind(&session_id, &id("a"), id("11")).await });
        provider.started.notified().await;
        manager
            .release(&session.session_id, &id("a"))
            .await
            .unwrap();
        provider.release.notify_one();
        assert_eq!(binding.await.unwrap(), Err(SessionError::Expired));
        assert_eq!(central.presence().await, Some(PresenceStatus::Offline));

        let copy = manager.clone();
        let claiming = tokio::spawn(async move {
            copy.claim(id("after-shutdown"), ClaimMode::Fresh, Some(id("11")))
                .await
        });
        provider.started.notified().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        manager.clone().run_expiration(cancel).await;
        provider.release.notify_one();
        assert_eq!(claiming.await.unwrap(), Err(SessionError::Expired));
    }
}
