//! Session-free JSON page of the local update feed for desktop notifications.
//! Scope comes from the bound agent profile and the selected team, never from caller input.
//! Reads local stores only: no central IO, no ACK, no task or read-state change, and no
//! file is written. The caller keeps its own cursor, independent of Host hooks and the
//! MCP `get_updates` cursor, so polling here never consumes another consumer's view.
use crate::{
    agent_profile,
    config::Config,
    sessions::IdentityError,
    update_hook,
    updates::{Cursor, Feed, MAX_LIMIT},
};
use serde_json::{Value, json};
use std::path::Path;

pub const SCHEMA_VERSION: u8 = 1;
pub const DEFAULT_LIMIT: usize = 20;

/// Stable failure code for callers; the message never includes paths or stored values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdatesError {
    pub code: &'static str,
}
impl UpdatesError {
    pub const INPUT_INVALID: Self = Self {
        code: "UPDATES_INPUT_INVALID",
    };
    pub const AGENT_UNBOUND: Self = Self {
        code: "UPDATES_AGENT_UNBOUND",
    };
    pub const BINDING_MISMATCH: Self = Self {
        code: "UPDATES_BINDING_MISMATCH",
    };
    pub const UNAVAILABLE: Self = Self {
        code: "LOCAL_UPDATES_UNAVAILABLE",
    };

    pub fn message(&self) -> &'static str {
        match self.code {
            "UPDATES_INPUT_INVALID" => {
                "Invalid cursor or limit. Drop the stored cursor and read again with --latest."
            }
            "UPDATES_AGENT_UNBOUND" => "No Agent is bound to this profile.",
            "UPDATES_BINDING_MISMATCH" => {
                "The bound Agent does not match the selected team or project folder, or changed while reading."
            }
            _ => "Local DevLink updates cannot be read.",
        }
    }
}
impl std::fmt::Display for UpdatesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message())
    }
}
impl std::error::Error for UpdatesError {}

/// Where a page starts.
#[derive(Clone, Copy, Debug)]
pub enum Start<'a> {
    /// After a cursor from an earlier page, or from the oldest retained entry.
    After(Option<&'a str>),
    /// At the end of the retained history: no entries, only the cursor to poll from.
    Latest,
}

/// Next page of the bound Agent's local updates, in the `get_updates` entry format plus
/// `schemaVersion` and the `scope` it was read for.
///
/// Entries are ordered by source (received tasks, sent-task events, question/answer
/// arrivals, overdue advisories), not by time. A cursor that no longer matches the
/// stores restarts from the oldest retained entry with `cursorReset: true`.
pub fn page(config_path: &Path, start: Start<'_>, limit: usize) -> Result<Value, UpdatesError> {
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(UpdatesError::INPUT_INVALID);
    }
    let cursor = match start {
        Start::After(Some(value)) => Some(Cursor::parse(value).ok_or(UpdatesError::INPUT_INVALID)?),
        _ => None,
    };
    let (binding, actor) = update_hook::bound_actor(config_path)
        .map_err(binding_error)?
        .ok_or(UpdatesError::AGENT_UNBOUND)?;
    let config = Config::load(config_path).map_err(|_| UpdatesError::UNAVAILABLE)?;
    let feed = Feed::load(config_path, &config.server_url, &actor)
        .map_err(|_| UpdatesError::UNAVAILABLE)?;
    let mut page = match start {
        Start::Latest => json!({
            "updates": [],
            "nextCursor": feed.take(None, feed.len()).next,
            "hasMore": false,
            "cursorReset": false,
            "coverage": "local",
        }),
        Start::After(_) => feed.page(cursor.as_ref(), limit),
    };
    // A profile rebound during storage IO must not label another Agent's entries.
    if agent_profile::inspection_binding(config_path)
        .map_err(binding_error)?
        .as_ref()
        != Some(&binding)
    {
        return Err(UpdatesError::BINDING_MISMATCH);
    }
    page["schemaVersion"] = json!(SCHEMA_VERSION);
    page["scope"] = json!({
        "userId": binding.user_id.to_string(),
        "teamId": binding.team_id.to_string(),
        "agentId": binding.agent_id.to_string(),
        "toolType": binding.tool,
        "projectPath": binding.project_root,
    });
    Ok(page)
}

fn binding_error(error: IdentityError) -> UpdatesError {
    match error {
        IdentityError::Forbidden => UpdatesError::BINDING_MISMATCH,
        _ => UpdatesError::UNAVAILABLE,
    }
}
