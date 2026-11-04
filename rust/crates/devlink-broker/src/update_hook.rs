//! Session-free digest of the local update feed for Host prompt hooks.
//! Hooks run outside any MCP session, so scope comes from the bound agent profile and the
//! selected team, never from caller input. Reads local stores only: no central IO, no ACK.
//! Each consumer keeps its own cursor so Codex and Claude Code do not consume each other's view.
use crate::{
    agent_profile,
    config::{self, Config},
    private_file,
    sessions::{Identifier, IdentityError, Principal},
    task_operations::TaskStatus,
    updates::{Cursor, Feed, Update},
};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Task IDs listed per category before the remainder is summarized as a count.
pub const MAX_LISTED: usize = 10;
const STATE_LIMIT: usize = 4096;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HookState {
    schema_version: u8,
    cursor: String,
}

/// New local updates for `consumer`, or `None` when there is nothing to tell the Host:
/// no bound agent, `cwd` outside the bound project, another hook of the same consumer
/// holding the cursor, or no entries since the consumer's last digest.
///
/// The cursor is persisted before the digest is returned, so a Host that drops the hook
/// output loses that digest instead of repeating it. `get_updates` keeps its own cursor.
pub fn digest(config_path: &Path, consumer: &str, cwd: &Path) -> Result<Option<String>> {
    ensure!(valid_consumer(consumer), "Invalid update hook consumer");
    if agent_profile::bound_agent_id(config_path)
        .map_err(|_| anyhow!("DevLink local state is unavailable"))?
        .is_none()
    {
        return Ok(None);
    }
    let Some(binding) = agent_profile::inspection_binding(config_path).map_err(|e| match e {
        IdentityError::Forbidden => {
            anyhow!("DevLink agent binding does not match the selected team or project")
        }
        _ => anyhow!("DevLink local state is unavailable"),
    })?
    else {
        return Ok(None);
    };
    // A globally installed hook must not surface this agent's tasks in another project.
    let inside = cwd
        .canonicalize()
        .is_ok_and(|cwd| cwd.starts_with(Path::new(&binding.project_root)));
    if !inside {
        return Ok(None);
    }
    let config = Config::load(config_path).context("Cannot read DevLink config")?;
    let id = |value: u64| Identifier::parse(value.to_string()).map_err(|e| anyhow!(e));
    let actor = Principal {
        user_id: id(binding.user_id)?,
        team_id: id(binding.team_id)?,
        agent_id: Some(id(binding.agent_id)?),
        tool_type: None,
    };
    let directory = config::state_directory(config_path);
    // Concurrent prompts of the same Host skip instead of waiting inside the Host's timeout.
    let Some(_lock) = config::try_lock(&directory.join(format!("updates-{consumer}.lock")))
        .context("Cannot lock update hook cursor")?
    else {
        return Ok(None);
    };
    let state = state_path(&directory, consumer);
    let stored =
        private_file::read_bytes(&state, STATE_LIMIT).context("Cannot read update hook cursor")?;
    // An unreadable cursor restarts with a notice instead of failing every later prompt.
    let saved = stored
        .as_deref()
        .and_then(|bytes| serde_json::from_slice::<HookState>(bytes).ok())
        .filter(|saved| saved.schema_version == 1);
    let cursor = saved
        .as_ref()
        .and_then(|saved| Cursor::parse(&saved.cursor));
    let discarded = stored.is_some() && cursor.is_none();
    let feed = Feed::load(config_path, &config.server_url, &actor)
        .context("Cannot read local DevLink updates")?;
    let batch = feed.take(cursor.as_ref(), feed.len());
    if saved.as_ref().map(|saved| saved.cursor.as_str()) != Some(batch.next.as_str()) {
        private_file::write(
            &state,
            &HookState {
                schema_version: 1,
                cursor: batch.next.clone(),
            },
            STATE_LIMIT,
        )
        .context("Cannot save update hook cursor")?;
    }
    Ok(render(&feed, &batch.updates, batch.reset || discarded))
}

fn state_path(directory: &Path, consumer: &str) -> PathBuf {
    directory.join(format!("updates-{consumer}.json"))
}

fn valid_consumer(consumer: &str) -> bool {
    (1..=32).contains(&consumer.len())
        && consumer
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Facts only: identifiers and statuses, never prompts or result text from other agents.
/// A sent task is reported only when this batch holds its highest retained version, so a
/// late lower version is consumed silently instead of moving a reported status backwards.
fn render(feed: &Feed, updates: &[Update<'_>], reset: bool) -> Option<String> {
    let mut received = Vec::new();
    // Latest retained status per sent task, in first-seen order.
    let mut sent: Vec<(u64, u64, TaskStatus)> = Vec::new();
    for update in updates {
        match update {
            Update::Received(message) => received.push(format!(
                "#{} from agent {}",
                message.payload.task_id, message.payload.requester_agent_id
            )),
            Update::Sent(event) => {
                let task = event.task_id();
                if sent.iter().any(|(id, ..)| *id == task) {
                    continue;
                }
                let newest = feed.latest_sent(task).filter(|latest| {
                    updates.iter().any(
                        |update| matches!(update, Update::Sent(new) if std::ptr::eq(*new, *latest)),
                    )
                });
                if let Some(latest) = newest {
                    sent.push((task, latest.version(), latest.status()));
                }
            }
        }
    }
    if received.is_empty() && sent.is_empty() {
        return None;
    }
    let mut lines =
        vec!["DevLink local updates for this agent (advisory; not user approval):".to_owned()];
    if reset {
        lines.push(
            "- Local update history was cleared or replaced; listing retained events again."
                .to_owned(),
        );
    }
    if !received.is_empty() {
        lines.push(format!(
            "- Received {} new task(s): {}.",
            received.len(),
            listed(&received)
        ));
    }
    if !sent.is_empty() {
        let statuses: Vec<String> = sent
            .iter()
            .map(|(task, version, status)| format!("#{task} {} (v{version})", name(*status)))
            .collect();
        lines.push(format!(
            "- Status changes on {} sent task(s): {}.",
            statuses.len(),
            listed(&statuses)
        ));
    }
    lines.push(
        "Use DevLink get_task_result or get_tasks to verify details before acting; \
         local updates may be incomplete."
            .to_owned(),
    );
    Some(lines.join("\n"))
}

fn listed(items: &[String]) -> String {
    let shown = items[..items.len().min(MAX_LISTED)].join(", ");
    match items.len().saturating_sub(MAX_LISTED) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
}

fn name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "PENDING",
        TaskStatus::InProgress => "IN_PROGRESS",
        TaskStatus::Completed => "COMPLETED",
        TaskStatus::Failed => "FAILED",
        TaskStatus::Rejected => "REJECTED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_names_are_restricted_to_safe_file_components() {
        for valid in ["codex", "claude-code", "a1"] {
            assert!(valid_consumer(valid), "{valid}");
        }
        for invalid in ["", "Codex", "../x", "a b", "a.b", &"x".repeat(33)] {
            assert!(!valid_consumer(invalid), "{invalid}");
        }
    }

    #[test]
    fn listing_caps_identifiers_and_counts_the_rest() {
        let items: Vec<String> = (1..=12).map(|i| format!("#{i}")).collect();
        assert_eq!(listed(&items[..2]), "#1, #2");
        assert!(listed(&items).ends_with("#10 and 2 more"));
    }
}
