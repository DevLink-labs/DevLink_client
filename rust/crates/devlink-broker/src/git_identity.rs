use crate::{agent_context::ContextClient, sessions::IdentityError, user_auth::UserClient};
use anyhow::{Result, ensure};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::{
    sync::RwLock,
    time::{Instant, MissedTickBehavior},
};
use tokio_util::sync::CancellationToken;

const USER_PATH: &str = "/api/v1/auth/users/me/git-identity";
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistrationStatus {
    pub registered: bool,
    pub registered_at: Option<String>,
}

impl RegistrationStatus {
    fn validate(self) -> Result<Self> {
        let valid = match (self.registered, self.registered_at.as_deref()) {
            (false, None) => true,
            (true, Some(value)) => {
                time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                    .is_ok()
            }
            _ => false,
        };
        ensure!(valid, "Invalid Git identity response");
        Ok(self)
    }
}

fn normalize_email(email: &str) -> String {
    email.trim_matches(|c| c <= '\u{20}').to_lowercase()
}

pub fn email_hash(email: &str) -> String {
    format!("{:x}", Sha256::digest(normalize_email(email).as_bytes()))
}

pub async fn register(path: &Path, email: &str) -> Result<RegistrationStatus> {
    let normalized = normalize_email(email);
    ensure!(
        normalized.encode_utf16().count() <= 255
            && normalized
                .rsplit_once('@')
                .is_some_and(|(local, domain)| !local.is_empty() && !domain.is_empty())
            && !normalized
                .chars()
                .any(|c| c.is_whitespace() || c.is_control()),
        "Invalid Git email"
    );
    let status: RegistrationStatus = UserClient::authenticated(path)
        .await?
        .put(USER_PATH, serde_json::json!({"gitEmail":normalized}))
        .await?;
    let status = status.validate()?;
    ensure!(
        status.registered,
        "Git email registration not confirmed; query status before retrying"
    );
    Ok(status)
}

pub async fn status(path: &Path) -> Result<RegistrationStatus> {
    UserClient::authenticated(path)
        .await?
        .get::<RegistrationStatus>(USER_PATH)
        .await?
        .validate()
}

pub async fn delete(path: &Path) -> Result<RegistrationStatus> {
    let status = UserClient::authenticated(path)
        .await?
        .delete::<RegistrationStatus>(USER_PATH)
        .await?
        .validate()?;
    ensure!(
        !status.registered,
        "Git email deletion not confirmed; query status before retrying"
    );
    Ok(status)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Comparison {
    registered: bool,
    git_email_hash: Option<String>,
    hash_algorithm: String,
    normalization: String,
}

pub struct IdentityClient {
    context: ContextClient,
    client: Client,
    url: url::Url,
    authorization: HeaderValue,
}

impl IdentityClient {
    pub fn new(server_url: &str, token: &str) -> Result<Self> {
        let mut url = crate::user_auth::origin(&crate::config::Config {
            server_url: server_url.to_owned(),
            ..Default::default()
        })?;
        url.set_path("/api/v1/agents/me/git-identity");
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("Invalid Agent credential"))?;
        authorization.set_sensitive(true);
        Ok(Self {
            context: ContextClient::new(server_url, token)?,
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .referer(false)
                .timeout(Duration::from_secs(3))
                .build()?,
            url,
            authorization,
        })
    }

    async fn comparison(&self) -> Result<Option<(u64, [u8; 32])>, IdentityError> {
        let identity = self.context.get().await?;
        let mut response = self
            .client
            .get(self.url.clone())
            .header(AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|_| IdentityError::Unavailable)?;
        if matches!(response.status().as_u16(), 401 | 403) {
            return Err(IdentityError::Unauthenticated);
        }
        if response.status().as_u16() != 200 {
            return Err(IdentityError::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| IdentityError::Unavailable)?
        {
            if bytes.len() + chunk.len() > 4096 {
                return Err(IdentityError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Envelope {
            success: bool,
            data: Comparison,
        }
        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| IdentityError::Unavailable)?;
        let data = envelope.data;
        if !envelope.success
            || data.hash_algorithm != "SHA-256"
            || data.normalization != "TRIM_LOWERCASE"
        {
            return Err(IdentityError::Unavailable);
        }
        match (data.registered, data.git_email_hash) {
            (false, None) => Ok(None),
            (true, Some(hash)) => decode_hash(&hash)
                .map(|hash| Some((identity.owner_user_id, hash)))
                .ok_or(IdentityError::Unavailable),
            _ => Err(IdentityError::Unavailable),
        }
    }
}

fn decode_hash(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut hash = [0; 32];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |b: u8| {
            if b.is_ascii_digit() {
                b - b'0'
            } else {
                b - b'a' + 10
            }
        };
        hash[index] = digit(pair[0]) * 16 + digit(pair[1]);
    }
    Some(hash)
}

struct Snapshot {
    /// `None` after a successful check that found no registered Git email.
    identity: Option<(u64, [u8; 32])>,
    checked_at: Instant,
}

#[derive(Clone, Default)]
pub struct IdentityCache(Arc<RwLock<Option<Snapshot>>>);

/// What a fresh comparison says about the bound user's Git email.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdentityState {
    /// Not checked yet, or the last check is older than the refresh interval.
    Unknown,
    Unregistered,
    /// Opaque fingerprint of the user ID and email hash; never the email or its hash.
    Registered(String),
}

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuthorAttribution {
    pub author_user_id: Option<String>,
}

impl IdentityCache {
    pub async fn clear(&self) {
        *self.0.write().await = None;
    }

    pub async fn refresh(&self, client: &IdentityClient) -> Result<(), IdentityError> {
        match client.comparison().await {
            Ok(comparison) => {
                // Publish only a complete result, without blocking readers on HTTP.
                *self.0.write().await = Some(Snapshot {
                    identity: comparison,
                    checked_at: Instant::now(),
                });
                Ok(())
            }
            Err(error) => {
                if matches!(
                    error,
                    IdentityError::Unauthenticated | IdentityError::Forbidden
                ) {
                    self.clear().await;
                }
                // Unavailable includes transport/protocol failures: retain the last
                // verified snapshot with its original expiry, never renew it here.
                Err(error)
            }
        }
    }

    pub async fn attribute(&self, author_email: &str) -> AuthorAttribution {
        self.attribute_hash(&email_hash(author_email)).await
    }

    pub async fn attribute_hash(&self, author_email_hash: &str) -> AuthorAttribution {
        let hash = decode_hash(author_email_hash);
        let snapshot = self.0.read().await;
        AuthorAttribution {
            author_user_id: snapshot
                .as_ref()
                .filter(|s| s.checked_at.elapsed() < REFRESH_INTERVAL)
                .and_then(|s| s.identity)
                .filter(|(_, stored)| Some(*stored) == hash)
                .map(|(user_id, _)| user_id.to_string()),
        }
    }

    /// Publishes a fresh check: `Some((user ID, email))` or unregistered.
    #[cfg(test)]
    pub(crate) async fn set_for_test(&self, identity: Option<(u64, &str)>) {
        *self.0.write().await = Some(Snapshot {
            identity: identity
                .map(|(user_id, email)| (user_id, decode_hash(&email_hash(email)).expect("hash"))),
            checked_at: Instant::now(),
        });
    }

    pub async fn state(&self) -> IdentityState {
        self.check()
            .await
            .map_or(IdentityState::Unknown, |check| check.state())
    }

    /// The current fresh check, kept as is so a long collection attributes every commit and
    /// records its state with the same result even if the cache expires or is cleared meanwhile.
    pub async fn check(&self) -> Option<IdentityCheck> {
        self.0
            .read()
            .await
            .as_ref()
            .filter(|s| s.checked_at.elapsed() < REFRESH_INTERVAL)
            .map(|s| IdentityCheck(s.identity))
    }
}

/// One fresh comparison result taken from [`IdentityCache::check`].
#[derive(Clone, Copy)]
pub struct IdentityCheck(Option<(u64, [u8; 32])>);

impl IdentityCheck {
    pub fn attribute(&self, author_email: &str) -> AuthorAttribution {
        let hash = decode_hash(&email_hash(author_email));
        AuthorAttribution {
            author_user_id: self
                .0
                .filter(|(_, stored)| Some(*stored) == hash)
                .map(|(user_id, _)| user_id.to_string()),
        }
    }

    pub fn state(&self) -> IdentityState {
        match self.0 {
            None => IdentityState::Unregistered,
            Some((user_id, hash)) => {
                let mut digest = Sha256::new();
                digest.update(b"devlink-git-identity\0");
                digest.update(user_id.to_be_bytes());
                digest.update(hash);
                IdentityState::Registered(format!("{:x}", digest.finalize()))
            }
        }
    }
}

pub async fn run(client: IdentityClient, cache: IdentityCache, cancel: CancellationToken) {
    let mut ticks = tokio::time::interval(REFRESH_INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = ticks.tick() => {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = cache.refresh(&client) => {},
                }
            }
        }
    }
    cache.clear().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        response::{IntoResponse, Response},
        routing::get,
    };
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
    use tokio::sync::{Notify, Semaphore};

    struct Replies {
        context_status: AtomicU16,
        comparison_status: AtomicU16,
        registered: AtomicBool,
        gated: bool,
        context_entered: Notify,
        comparison_entered: Notify,
        context_release: Semaphore,
        comparison_release: Semaphore,
    }
    struct Fixture {
        client: Arc<IdentityClient>,
        replies: Arc<Replies>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
    async fn context(State(replies): State<Arc<Replies>>) -> Response {
        replies.context_entered.notify_one();
        if replies.gated {
            replies.context_release.acquire().await.unwrap().forget();
        }
        let status = StatusCode::from_u16(replies.context_status.load(Ordering::SeqCst)).unwrap();
        if status != StatusCode::OK {
            return status.into_response();
        }
        Json(serde_json::json!({"success":true,"data":{"agentId":12,"ownerUserId":52,"teamId":71}}))
            .into_response()
    }
    async fn comparison(State(replies): State<Arc<Replies>>) -> Response {
        replies.comparison_entered.notify_one();
        if replies.gated {
            replies.comparison_release.acquire().await.unwrap().forget();
        }
        let status =
            StatusCode::from_u16(replies.comparison_status.load(Ordering::SeqCst)).unwrap();
        if status != StatusCode::OK {
            return status.into_response();
        }
        let registered = replies.registered.load(Ordering::SeqCst);
        Json(serde_json::json!({"success":true,"data":{
            "registered":registered,"gitEmailHash":registered.then(|| email_hash("new@example.test")),
            "hashAlgorithm":"SHA-256","normalization":"TRIM_LOWERCASE"
        }})).into_response()
    }
    impl Fixture {
        async fn new(gated: bool) -> Self {
            let replies = Arc::new(Replies {
                context_status: AtomicU16::new(200),
                comparison_status: AtomicU16::new(200),
                registered: AtomicBool::new(true),
                gated,
                context_entered: Notify::new(),
                comparison_entered: Notify::new(),
                context_release: Semaphore::new(0),
                comparison_release: Semaphore::new(0),
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = Router::new()
                .route("/api/v1/agents/me", get(context))
                .route("/api/v1/agents/me/git-identity", get(comparison))
                .with_state(replies.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let client = Arc::new(
                IdentityClient::new(&format!("ws://{address}"), "synthetic-agent-token").unwrap(),
            );
            Self {
                client,
                replies,
                task,
            }
        }
    }
    async fn seeded(age: Duration) -> IdentityCache {
        let cache = IdentityCache::default();
        *cache.0.write().await = Some(Snapshot {
            identity: Some((51, decode_hash(&email_hash("old@example.test")).unwrap())),
            checked_at: Instant::now() - age,
        });
        cache
    }

    #[tokio::test]
    async fn refresh_keeps_attribution_while_both_requests_wait_then_replaces_it() {
        let f = Fixture::new(true).await;
        let cache = seeded(Duration::ZERO).await;
        let client = f.client.clone();
        let refreshing = cache.clone();
        let task = tokio::spawn(async move { refreshing.refresh(&client).await });
        for (entered, release) in [
            (&f.replies.context_entered, &f.replies.context_release),
            (&f.replies.comparison_entered, &f.replies.comparison_release),
        ] {
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            let current =
                tokio::time::timeout(Duration::from_secs(1), cache.attribute("old@example.test"))
                    .await
                    .unwrap();
            let replacement = cache.attribute("new@example.test").await;
            // Release before asserting so failures do not leave a blocked refresh.
            release.add_permits(1);
            assert_eq!(current.author_user_id.as_deref(), Some("51"));
            assert_eq!(replacement.author_user_id, None);
        }
        task.await.unwrap().unwrap();
        assert_eq!(
            cache.attribute("old@example.test").await.author_user_id,
            None
        );
        assert_eq!(
            cache
                .attribute("new@example.test")
                .await
                .author_user_id
                .as_deref(),
            Some("52")
        );
    }

    #[tokio::test]
    async fn transient_failure_at_either_request_preserves_only_the_original_ttl() {
        for fail_context in [true, false] {
            let f = Fixture::new(false).await;
            let cache = seeded(Duration::from_secs(20)).await;
            if fail_context {
                f.replies.context_status.store(503, Ordering::SeqCst);
            } else {
                f.replies.comparison_status.store(503, Ordering::SeqCst);
            }
            assert_eq!(
                cache.refresh(&f.client).await,
                Err(IdentityError::Unavailable)
            );
            assert_eq!(
                cache
                    .attribute("old@example.test")
                    .await
                    .author_user_id
                    .as_deref(),
                Some("51")
            );
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(10)).await;
            assert_eq!(
                cache.attribute("old@example.test").await.author_user_id,
                None
            );
            tokio::time::resume();
        }
    }

    #[tokio::test]
    async fn confirmed_auth_denials_at_either_request_and_unregistration_clear_immediately() {
        for (context_status, comparison_status, registered) in [
            (401, 200, true),
            (403, 200, true),
            (200, 401, true),
            (200, 403, true),
            (200, 200, false),
        ] {
            let f = Fixture::new(false).await;
            let cache = seeded(Duration::ZERO).await;
            f.replies
                .context_status
                .store(context_status, Ordering::SeqCst);
            f.replies
                .comparison_status
                .store(comparison_status, Ordering::SeqCst);
            f.replies.registered.store(registered, Ordering::SeqCst);
            let result = cache.refresh(&f.client).await;
            if registered {
                assert_eq!(result, Err(IdentityError::Unauthenticated));
            } else {
                result.unwrap();
            }
            assert_eq!(
                cache.attribute("old@example.test").await.author_user_id,
                None
            );
        }
    }

    #[test]
    fn hashing_matches_the_server_vector_and_java_trim() {
        assert_eq!(
            email_hash("  User@Example.COM \t"),
            "b4c9a289323b21a01c3e940f150eb9b8c542587f1abfd8f0e1cc1ffc5e475514"
        );
        assert_eq!(
            email_hash("\u{1c}USER@EXAMPLE.COM"),
            email_hash("user@example.com")
        );
        assert_ne!(
            email_hash("\u{a0}user@example.com"),
            email_hash("user@example.com")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn attribution_expires_and_serializes_only_the_optional_user_id() {
        let cache = IdentityCache::default();
        *cache.0.write().await = Some(Snapshot {
            identity: Some((
                9223372036854775807,
                decode_hash(&email_hash("user@example.com")).unwrap(),
            )),
            checked_at: Instant::now(),
        });
        assert_eq!(
            serde_json::to_value(cache.attribute("USER@example.com").await).unwrap(),
            serde_json::json!({"authorUserId":"9223372036854775807"})
        );
        assert_eq!(
            cache.attribute("another@example.com").await.author_user_id,
            None
        );
        assert_eq!(cache.attribute_hash("invalid").await.author_user_id, None);
        tokio::time::advance(REFRESH_INTERVAL).await;
        assert_eq!(
            cache.attribute("user@example.com").await.author_user_id,
            None
        );
    }

    #[tokio::test(start_paused = true)]
    async fn state_separates_unknown_unregistered_and_registered_without_the_email() {
        let cache = IdentityCache::default();
        assert_eq!(cache.state().await, IdentityState::Unknown);
        cache.set_for_test(None).await;
        assert_eq!(cache.state().await, IdentityState::Unregistered);
        cache.set_for_test(Some((51, "user@example.com"))).await;
        let IdentityState::Registered(first) = cache.state().await else {
            panic!("registered state expected");
        };
        assert!(!first.contains(&email_hash("user@example.com")));
        assert!(!first.contains("user@example.com"));
        cache.set_for_test(Some((51, "other@example.com"))).await;
        assert_ne!(cache.state().await, IdentityState::Registered(first));
        tokio::time::advance(REFRESH_INTERVAL).await;
        assert_eq!(cache.state().await, IdentityState::Unknown);
    }

    #[tokio::test(start_paused = true)]
    async fn a_taken_check_keeps_one_result_after_the_cache_expires_or_clears() {
        let cache = IdentityCache::default();
        assert!(cache.check().await.is_none());
        cache.set_for_test(Some((51, "user@example.com"))).await;
        let check = cache.check().await.unwrap();
        let state = check.state();
        tokio::time::advance(REFRESH_INTERVAL).await;
        cache.clear().await;
        assert_eq!(
            cache.attribute("user@example.com").await.author_user_id,
            None
        );
        assert_eq!(
            check
                .attribute(" User@Example.com ")
                .author_user_id
                .as_deref(),
            Some("51")
        );
        assert_eq!(check.attribute("other@example.com").author_user_id, None);
        assert_eq!(check.state(), state);
    }
}
