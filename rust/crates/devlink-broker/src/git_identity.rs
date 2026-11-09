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
    user_id: u64,
    hash: [u8; 32],
    checked_at: Instant,
}

#[derive(Clone, Default)]
pub struct IdentityCache(Arc<RwLock<Option<Snapshot>>>);

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
        self.clear().await;
        if let Some((user_id, hash)) = client.comparison().await? {
            *self.0.write().await = Some(Snapshot {
                user_id,
                hash,
                checked_at: Instant::now(),
            });
        }
        Ok(())
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
                .filter(|s| s.checked_at.elapsed() < REFRESH_INTERVAL && Some(s.hash) == hash)
                .map(|s| s.user_id.to_string()),
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
            user_id: 9223372036854775807,
            hash: decode_hash(&email_hash("user@example.com")).unwrap(),
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
}
