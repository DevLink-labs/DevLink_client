//! Authenticated loopback API and runtime discovery shared by CLI and future MCP client.
use crate::{
    central::ConnectionStatus,
    config::{self, state_directory},
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

pub const SERVICE: &str = "devlink-broker";
pub const API_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Runtime {
    pub api_version: u32,
    pub pid: u32,
    pub port: u16,
    pub instance_id: String,
    pub control_token: String,
}
impl Runtime {
    pub fn path(config: &Path) -> PathBuf {
        state_directory(config).join("runtime.json")
    }
    pub fn load(config: &Path) -> Result<Self> {
        crate::credentials::verify_parent_safety(&state_directory(config))?;
        let runtime_path = Self::path(config);
        crate::credentials::ensure_safe_target_if_exists(&runtime_path)?;
        let bytes = fs::read(runtime_path).context("Cannot read broker runtime metadata")?;
        let runtime: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("Invalid broker runtime metadata"))?;
        if runtime.api_version != API_VERSION
            || runtime.port == 0
            || runtime.instance_id.is_empty()
            || runtime.control_token.is_empty()
        {
            bail!("Unsupported or invalid broker runtime metadata");
        }
        Ok(runtime)
    }
    pub fn client(&self) -> Result<reqwest::Client> {
        // Ignore HTTP_PROXY: discovery endpoints must never leave this machine.
        Ok(reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(1))
            .build()?)
    }
    pub fn url(&self, endpoint: &str) -> String {
        format!("http://127.0.0.1:{}{endpoint}", self.port)
    }
    pub async fn health(&self) -> Result<Health> {
        let response = self
            .client()?
            .get(self.url("/health"))
            .bearer_auth(&self.control_token)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Broker is running but health check is unavailable"))?;
        if !response.status().is_success() {
            bail!("Broker health check was rejected");
        }
        let health: Health = response
            .json()
            .await
            .map_err(|_| anyhow::anyhow!("Invalid broker health response"))?;
        if health.service != SERVICE
            || health.api_version != API_VERSION
            || health.instance_id != self.instance_id
            || health.pid != self.pid
        {
            bail!("Broker identity mismatch; refusing to control an unknown process");
        }
        Ok(health)
    }
    pub async fn stop(&self) -> Result<()> {
        self.health().await?;
        let response = self
            .client()?
            .post(self.url("/shutdown"))
            .bearer_auth(&self.control_token)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Broker shutdown request failed"))?;
        if !response.status().is_success() {
            bail!("Broker rejected shutdown request");
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notification_rejections: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<crate::inbox::InboxDiagnostics>,
    pub service: String,
    pub api_version: u32,
    pub pid: u32,
    pub instance_id: String,
    pub central_server: ConnectionStatus,
}

/// A held OS lock is authoritative. PID files alone are unsafe because PIDs can be reused.
pub fn running(config_path: &Path) -> Result<bool> {
    let state = state_directory(config_path);
    if fs::symlink_metadata(&state).is_ok() {
        crate::credentials::verify_parent_safety(&state)?;
    }
    if !state.exists() {
        return Ok(false);
    }
    let path = state.join("broker.lock");
    match config::try_lock(&path)? {
        Some(_guard) => {
            // Remove stale discovery data while holding the same lock required by a new broker.
            crate::credentials::ensure_safe_target_if_exists(&Runtime::path(config_path))?;
            match fs::remove_file(Runtime::path(config_path)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => bail!("Cannot remove stale runtime metadata"),
            }
            Ok(false)
        }
        None => Ok(true),
    }
}

pub fn publish(config: &Path, runtime: &Runtime) -> Result<()> {
    crate::credentials::ensure_safe_parent_directory(&state_directory(config))?;
    crate::credentials::ensure_safe_target_if_exists(&Runtime::path(config))?;
    config::atomic_json(&Runtime::path(config), runtime)
}
