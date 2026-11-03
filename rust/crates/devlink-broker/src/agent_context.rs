//! Live Agent identity from the existing central Bearer context API.
use crate::{central::endpoint_url, sessions::IdentityError};
use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::Deserialize;
use std::time::Duration;

#[derive(Clone)]
pub struct ContextClient {
    client: Client,
    url: url::Url,
    authorization: HeaderValue,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentContext {
    pub agent_id: u64,
    pub owner_user_id: u64,
    pub team_id: u64,
    pub name: Option<String>,
    pub role_description: Option<String>,
    pub tool_type: Option<crate::agent_profile::Tool>,
    pub project_name: Option<String>,
}

impl ContextClient {
    pub fn new(server_url: &str, token: &str) -> anyhow::Result<Self> {
        let mut url = url::Url::parse(&endpoint_url(server_url)?)?;
        let scheme = if url.scheme() == "wss" {
            "https"
        } else {
            "http"
        };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("Invalid context origin"))?;
        url.set_path("/api/v1/agents/me");
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("Invalid Agent credential"))?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .timeout(Duration::from_secs(3))
            .build()?;
        Ok(Self {
            client,
            url,
            authorization,
        })
    }

    pub async fn get(&self) -> Result<AgentContext, IdentityError> {
        tokio::time::timeout(Duration::from_secs(3), self.exchange())
            .await
            .unwrap_or(Err(IdentityError::Unavailable))
    }

    async fn exchange(&self) -> Result<AgentContext, IdentityError> {
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
            if bytes.len() + chunk.len() > 256 * 1024 {
                return Err(IdentityError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Envelope {
            success: bool,
            data: AgentContext,
        }
        let envelope: Envelope =
            serde_json::from_slice(&bytes).map_err(|_| IdentityError::Unavailable)?;
        let ids = [
            &envelope.data.agent_id,
            &envelope.data.owner_user_id,
            &envelope.data.team_id,
        ];
        if !envelope.success || ids.into_iter().any(|id| *id == 0 || *id > i64::MAX as u64) {
            return Err(IdentityError::Unauthenticated);
        }
        Ok(envelope.data)
    }
}
