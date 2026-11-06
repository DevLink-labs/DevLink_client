//! User-level Host hook installation for `devlink updates-hook`.
//!
//! The helper prints nothing outside the bound project, so one user-level entry per Host
//! is safe across projects. Only entries whose command runs `updates-hook --host <host>`
//! are treated as DevLink's. Other keys and events stay as their original JSON text; in
//! a rewritten event array, other groups and hooks keep their values, though key order
//! and number formatting inside that array may be normalized. Writes reuse the MCP
//! provider's lock, exact backup and digest-fenced atomic replacement. Codex additionally
//! requires a one-time interactive `/hooks` trust.
use super::{ConfigError, LIMIT, ToolType, format::Unique, storage};
use indexmap::IndexMap;
use serde::Serialize;
use serde_json::{Value, json, value::RawValue};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, ConfigError>;

pub const EVENTS: [(&str, &str); 2] = [
    ("SessionStart", "session-start"),
    ("UserPromptSubmit", "user-prompt-submit"),
];
const TIMEOUT_SECONDS: u64 = 10;
/// Characters that some Host shell would interpret in an unquoted path.
const SHELL_SPECIAL: &str = "\"'`$&|;<>()[]{}^%!*?#~,=\\";

/// Where a Host reads user-level hooks and where DevLink keeps its lock and backups.
#[derive(Clone, Debug)]
pub struct HookTarget {
    pub tool: ToolType,
    pub path: PathBuf,
    pub state_root: PathBuf,
}
impl HookTarget {
    /// Claude Code: `$CLAUDE_CONFIG_DIR/settings.json` or `~/.claude/settings.json`.
    /// Codex: `$CODEX_HOME/hooks.json` or `~/.codex/hooks.json`.
    pub fn user(tool: ToolType) -> Result<Self> {
        let (variable, default) = match tool {
            ToolType::ClaudeCode => ("CLAUDE_CONFIG_DIR", ".claude"),
            ToolType::Codex => ("CODEX_HOME", ".codex"),
        };
        let directory = match std::env::var_os(variable).filter(|v| !v.is_empty()) {
            Some(value) => PathBuf::from(value),
            None => dirs::home_dir()
                .ok_or_else(|| ConfigError::new("SETUP_UNSAFE_FILE"))?
                .join(default),
        };
        if !directory.is_absolute() {
            return Err(ConfigError::new("SETUP_INPUT_INVALID").at(&directory));
        }
        let file = match tool {
            ToolType::ClaudeCode => "settings.json",
            ToolType::Codex => "hooks.json",
        };
        Ok(Self {
            tool,
            path: directory.join(file),
            state_root: storage::state_root()?,
        })
    }
}

pub fn host_name(tool: ToolType) -> &'static str {
    match tool {
        ToolType::Codex => "codex",
        ToolType::ClaudeCode => "claude-code",
    }
}

/// The exact command DevLink installs for one event. Hosts run hook commands through
/// different shells (bash, cmd, PowerShell), so paths are written unquoted with forward
/// slashes, the form verified on real Hosts. Paths that would need quoting or escaping,
/// and UNC paths, are refused instead of guessing a shell's rules. Callers pass
/// canonical paths so the same profile always yields the same command.
pub fn command(tool: ToolType, devlink: &Path, config: &Path, event: &str) -> Result<String> {
    let path = |p: &Path| -> Result<String> {
        if !p.is_absolute() {
            return Err(ConfigError::new("SETUP_INPUT_INVALID").at(p));
        }
        let value = storage::host_path(p);
        #[cfg(windows)]
        let value = value.replace('\\', "/");
        if value.is_empty()
            || value.starts_with("//")
            || value
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || SHELL_SPECIAL.contains(c))
        {
            return Err(ConfigError::new("HOOKS_PATH_UNSUPPORTED").at(p));
        }
        Ok(value)
    };
    Ok(format!(
        "{} --config {} updates-hook --host {} --event {event}",
        path(devlink)?,
        path(config)?,
        host_name(tool)
    ))
}

/// The command for every event DevLink installs, keyed by Host event name.
pub fn wanted(tool: ToolType, devlink: &Path, config: &Path) -> Result<IndexMap<String, String>> {
    EVENTS
        .iter()
        .map(|(event, flag)| Ok(((*event).to_owned(), command(tool, devlink, config, flag)?)))
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookChange {
    Create,
    Merge,
    Unchanged,
    Conflict,
    Remove,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookPlan {
    pub tool_type: ToolType,
    pub target_path: PathBuf,
    pub change: HookChange,
    /// Commands DevLink will own after the change, per Host event.
    pub wanted: IndexMap<String, String>,
    /// Existing DevLink commands that differ from `wanted` (a conflict unless replaced).
    pub existing: IndexMap<String, Vec<String>>,
    #[serde(skip)]
    expected_digest: Option<String>,
    #[serde(skip)]
    after: Option<Vec<u8>>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookApplyResult {
    pub status: &'static str,
    pub target_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backup_path: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookStatus {
    pub tool_type: ToolType,
    pub target_path: PathBuf,
    /// Against the expected commands: `installed` (each event's DevLink entries are
    /// exactly its expected command), `stale` (every event has DevLink entries but at
    /// least one is not the expected command, e.g. another devlink path or profile),
    /// `partial` or `missing`. Without expected commands, `installed` means every event
    /// has some DevLink entry for this Host.
    pub status: &'static str,
    pub commands: IndexMap<String, Vec<String>>,
}

fn conflict() -> ConfigError {
    ConfigError::new("SETUP_MCP_CONFIG_CONFLICT")
}
/// The Host file is not JSON, has duplicate keys or an unexpected hooks shape.
fn invalid() -> ConfigError {
    ConfigError::new("HOOKS_CONFIG_INVALID")
}
fn is_devlink(tool: ToolType, command: &str) -> bool {
    let host = host_name(tool);
    let words: Vec<_> = command
        .split_whitespace()
        .skip_while(|w| *w != "updates-hook")
        .collect();
    !words.is_empty()
        && (words.windows(2).any(|w| w[0] == "--host" && w[1] == host)
            || words
                .iter()
                .any(|w| w.strip_prefix("--host=") == Some(host)))
}

/// Parsed root object plus the `hooks` object, both order-preserving and with
/// unrelated values kept as raw JSON.
struct Document {
    root: IndexMap<String, Box<RawValue>>,
    hooks: IndexMap<String, Box<RawValue>>,
}
fn parse(bytes: Option<&[u8]>) -> Result<Document> {
    let original = bytes.unwrap_or(b"{}");
    // Duplicate keys are rejected so a rewrite cannot silently drop another hook.
    let _ = serde_json::from_slice::<Unique>(original).map_err(|_| invalid())?;
    let root: IndexMap<String, Box<RawValue>> =
        serde_json::from_slice(original).map_err(|_| invalid())?;
    let hooks = match root.get("hooks") {
        Some(raw) => serde_json::from_str(raw.get()).map_err(|_| invalid())?,
        None => IndexMap::new(),
    };
    Ok(Document { root, hooks })
}
fn groups(doc: &Document, event: &str) -> Result<Vec<Value>> {
    match doc.hooks.get(event) {
        Some(raw) => {
            let value: Value = serde_json::from_str(raw.get()).map_err(|_| invalid())?;
            value.as_array().cloned().ok_or_else(invalid)
        }
        None => Ok(Vec::new()),
    }
}
fn commands(tool: ToolType, groups: &[Value]) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for group in groups {
        let Some(hooks) = group.get("hooks") else {
            continue;
        };
        for hook in hooks.as_array().ok_or_else(invalid)? {
            if let Some(command) = hook.get("command").and_then(Value::as_str)
                && is_devlink(tool, command)
            {
                found.push(command.to_owned());
            }
        }
    }
    Ok(found)
}
/// Remove DevLink hooks for this Host; drop groups left without hooks.
fn without_devlink(tool: ToolType, groups: Vec<Value>) -> Result<Vec<Value>> {
    let mut kept = Vec::new();
    for mut group in groups {
        let Some(hooks) = group.get_mut("hooks") else {
            kept.push(group);
            continue;
        };
        let list = hooks.as_array_mut().ok_or_else(invalid)?;
        let before = list.len();
        list.retain(|hook| {
            !hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| is_devlink(tool, c))
        });
        if list.is_empty() && before > 0 {
            continue;
        }
        kept.push(group);
    }
    Ok(kept)
}
/// Rewrite only the events in `events`; every other event keeps its original JSON text.
fn render(mut doc: Document, events: IndexMap<String, Vec<Value>>) -> Result<Vec<u8>> {
    // Raw values are emitted verbatim by the pretty printer, so the `hooks` object is
    // assembled with the same two-space layout: depth 1 for its keys, and rewritten
    // arrays indented to match. Preserved events keep their own original text.
    const INDENT: &str = "    ";
    for (event, groups) in events {
        if groups.is_empty() {
            doc.hooks.shift_remove(&event);
        } else {
            let pretty = serde_json::to_string_pretty(&groups).map_err(|_| invalid())?;
            let raw = pretty.replace('\n', &format!("\n{INDENT}"));
            doc.hooks
                .insert(event, RawValue::from_string(raw).map_err(|_| invalid())?);
        }
    }
    if doc.hooks.is_empty() {
        doc.root.shift_remove("hooks");
    } else {
        let mut raw = String::from("{");
        for (index, (event, value)) in doc.hooks.iter().enumerate() {
            raw.push_str(if index == 0 { "\n" } else { ",\n" });
            raw.push_str(INDENT);
            raw.push_str(&serde_json::to_string(event).map_err(|_| invalid())?);
            raw.push_str(": ");
            raw.push_str(value.get());
        }
        raw.push_str("\n  }");
        doc.root.insert(
            "hooks".into(),
            RawValue::from_string(raw).map_err(|_| invalid())?,
        );
    }
    let mut bytes = serde_json::to_vec_pretty(&doc.root).map_err(|_| invalid())?;
    bytes.push(b'\n');
    // The result must stay readable by the same size-limited reader.
    if bytes.len() > LIMIT {
        return Err(invalid());
    }
    Ok(bytes)
}
fn read(target: &HookTarget) -> Result<Option<Vec<u8>>> {
    storage::read(&target.path)
}
/// Attach the Host file path to format errors so the user knows which file to fix.
fn located<T>(target: &HookTarget, result: Result<T>) -> Result<T> {
    result.map_err(|error| {
        if error.code == "HOOKS_CONFIG_INVALID" && error.path.is_none() {
            error.at(&target.path)
        } else {
            error
        }
    })
}
fn scan(target: &HookTarget) -> Result<(Option<Vec<u8>>, Document)> {
    let before = read(target)?;
    let doc = located(target, parse(before.as_deref()))?;
    Ok((before, doc))
}

/// Report which DevLink commands each event has. With `wanted`, also tell whether they
/// are the expected ones (`stale` when present but different).
pub fn status(
    target: &HookTarget,
    wanted: Option<&IndexMap<String, String>>,
) -> Result<HookStatus> {
    let (_, doc) = scan(target)?;
    let mut found = IndexMap::new();
    for (event, _) in EVENTS {
        let groups = located(target, groups(&doc, event))?;
        found.insert(
            event.to_owned(),
            located(target, commands(target.tool, &groups))?,
        );
    }
    let present = found.values().filter(|c| !c.is_empty()).count();
    // Matches install: an event is as expected only when its DevLink entries are exactly
    // the expected command, so `installed` here means install would be unchanged.
    let expected = wanted.is_none_or(|wanted| {
        wanted.iter().all(|(event, command)| {
            found
                .get(event)
                .is_some_and(|c| !c.is_empty() && c.iter().all(|found| found == command))
        })
    });
    Ok(HookStatus {
        tool_type: target.tool,
        target_path: target.path.clone(),
        status: match present {
            0 => "missing",
            n if n == EVENTS.len() && expected => "installed",
            n if n == EVENTS.len() => "stale",
            _ => "partial",
        },
        commands: found,
    })
}

/// Plan an install. `wanted` maps each Host event to its command. With `replace`,
/// differing DevLink entries are swapped for the wanted ones instead of conflicting.
/// Events that need no change are not rewritten.
pub fn plan_install(
    target: &HookTarget,
    wanted: IndexMap<String, String>,
    replace: bool,
) -> Result<HookPlan> {
    let (before, doc) = scan(target)?;
    let mut events = IndexMap::new();
    let mut existing = IndexMap::new();
    let mut conflicted = false;
    for (event, command) in &wanted {
        let groups = located(target, groups(&doc, event))?;
        let found = located(target, commands(target.tool, &groups))?;
        let others: Vec<_> = found.iter().filter(|c| *c != command).cloned().collect();
        let satisfied = found.iter().any(|c| c == command);
        if !others.is_empty() {
            existing.insert(event.clone(), others.clone());
        }
        if satisfied && others.is_empty() {
            continue;
        }
        if !others.is_empty() && !replace {
            conflicted = true;
            continue;
        }
        let mut groups = located(target, without_devlink(target.tool, groups))?;
        groups.push(
            json!({"hooks": [{"type": "command", "command": command, "timeout": TIMEOUT_SECONDS}]}),
        );
        events.insert(event.clone(), groups);
    }
    let change = if conflicted {
        HookChange::Conflict
    } else if events.is_empty() {
        HookChange::Unchanged
    } else if before.is_none() {
        HookChange::Create
    } else {
        HookChange::Merge
    };
    let after = match change {
        HookChange::Create | HookChange::Merge => Some(located(target, render(doc, events))?),
        _ => None,
    };
    Ok(HookPlan {
        tool_type: target.tool,
        target_path: target.path.clone(),
        change,
        wanted,
        existing,
        expected_digest: storage::digest(before.as_deref()),
        after,
    })
}

/// Plan removing every DevLink entry for this Host, leaving all other hooks.
pub fn plan_uninstall(target: &HookTarget) -> Result<HookPlan> {
    let (before, doc) = scan(target)?;
    let mut events = IndexMap::new();
    let mut existing = IndexMap::new();
    for (event, _) in EVENTS {
        let groups = located(target, groups(&doc, event))?;
        let found = located(target, commands(target.tool, &groups))?;
        if !found.is_empty() {
            existing.insert(event.to_owned(), found);
            events.insert(
                event.to_owned(),
                located(target, without_devlink(target.tool, groups))?,
            );
        }
    }
    let (change, after) = if events.is_empty() {
        (HookChange::Unchanged, None)
    } else {
        (
            HookChange::Remove,
            Some(located(target, render(doc, events))?),
        )
    };
    Ok(HookPlan {
        tool_type: target.tool,
        target_path: target.path.clone(),
        change,
        wanted: IndexMap::new(),
        existing,
        expected_digest: storage::digest(before.as_deref()),
        after,
    })
}

/// Apply a plan under the per-target lock. The file must still match the planned
/// digest; the previous bytes are kept as an exact private backup first.
pub fn apply(target: &HookTarget, plan: &HookPlan) -> Result<HookApplyResult> {
    let Some(after) = plan.after.as_deref() else {
        return match plan.change {
            HookChange::Conflict => Err(conflict()),
            _ => Ok(HookApplyResult {
                status: "unchanged",
                target_path: target.path.clone(),
                backup_path: None,
            }),
        };
    };
    if plan.target_path != target.path || plan.tool_type != target.tool {
        return Err(conflict());
    }
    let state = storage::state_at(&target.state_root, &target.path)?;
    let _lock = storage::lock_at(&target.path, state.clone())?;
    let current = read(target)?;
    if storage::digest(current.as_deref()) != plan.expected_digest {
        return Err(conflict());
    }
    let backup = match current.as_deref() {
        Some(bytes) => Some(storage::backup_at(state, bytes)?),
        None => None,
    };
    storage::write_version(
        &target.path,
        after,
        current.is_none(),
        plan.expected_digest.as_deref(),
    )
    .map_err(|mut error| {
        error.backup_path = backup.clone();
        error
    })?;
    Ok(HookApplyResult {
        status: "applied",
        target_path: target.path.clone(),
        backup_path: backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials;
    use std::fs;

    fn target(root: &Path, tool: ToolType) -> HookTarget {
        credentials::set_private_permissions(root).unwrap();
        let directory = root.join(match tool {
            ToolType::Codex => ".codex",
            ToolType::ClaudeCode => ".claude",
        });
        fs::create_dir(&directory).unwrap();
        credentials::set_private_permissions(&directory).unwrap();
        HookTarget {
            tool,
            path: directory.join(match tool {
                ToolType::Codex => "hooks.json",
                ToolType::ClaudeCode => "settings.json",
            }),
            state_root: root.join("state"),
        }
    }
    fn wanted_at(tool: ToolType, root: &Path) -> IndexMap<String, String> {
        wanted(tool, &root.join("devlink.exe"), &root.join("config.json")).unwrap()
    }
    fn install(t: &HookTarget, root: &Path, replace: bool) -> Result<HookApplyResult> {
        let plan = plan_install(t, wanted_at(t.tool, root), replace)?;
        apply(t, &plan)
    }
    fn seed(t: &HookTarget, bytes: &[u8]) {
        storage::write(&t.path, bytes, !t.path.exists()).unwrap();
    }
    fn json(t: &HookTarget) -> Value {
        serde_json::from_slice(&fs::read(&t.path).unwrap()).unwrap()
    }

    #[test]
    fn command_uses_forward_slashes_and_refuses_paths_needing_quotes() {
        let root = tempfile::tempdir().unwrap();
        let value = command(
            ToolType::Codex,
            &root.path().join("devlink.exe"),
            &root.path().join("config.json"),
            "session-start",
        )
        .unwrap();
        assert!(!value.contains('\\'));
        assert!(value.ends_with("updates-hook --host codex --event session-start"));
        assert!(is_devlink(ToolType::Codex, &value));
        assert!(!is_devlink(ToolType::ClaudeCode, &value));
        let spaced = root.path().join("with space").join("devlink.exe");
        let error = command(ToolType::Codex, &spaced, &root.path().join("c.json"), "x");
        assert_eq!(error.unwrap_err().code, "HOOKS_PATH_UNSUPPORTED");
        assert_eq!(
            command(
                ToolType::Codex,
                Path::new("devlink"),
                &root.path().join("c"),
                "x"
            )
            .unwrap_err()
            .code,
            "SETUP_INPUT_INVALID"
        );
    }

    #[test]
    fn install_creates_merges_and_reuses_while_preserving_other_settings() {
        for tool in [ToolType::Codex, ToolType::ClaudeCode] {
            let root = tempfile::tempdir().unwrap();
            let t = target(root.path(), tool);
            assert_eq!(status(&t, None).unwrap().status, "missing");
            let created = install(&t, root.path(), false).unwrap();
            assert_eq!(created.status, "applied");
            assert!(created.backup_path.is_none());
            assert_eq!(status(&t, None).unwrap().status, "installed");
            assert_eq!(install(&t, root.path(), false).unwrap().status, "unchanged");

            seed(&t, r#"{"theme":"dark","big":12345678901234567890,"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"node other.cjs","timeout":5}]}],"PostToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"lint"}]}]}}"#.as_ref());
            let plan = plan_install(&t, wanted_at(tool, root.path()), false).unwrap();
            assert_eq!(plan.change, HookChange::Merge);
            let merged = apply(&t, &plan).unwrap();
            let backup = merged.backup_path.unwrap();
            assert!(
                fs::read_to_string(backup)
                    .unwrap()
                    .contains("node other.cjs")
            );
            let value = json(&t);
            assert_eq!(value["theme"], "dark");
            let text = fs::read_to_string(&t.path).unwrap();
            assert!(text.contains("12345678901234567890"));
            // The rebuilt hooks object keeps the file's indented layout.
            assert!(text.contains("\n  \"hooks\": {\n    \""), "{text}");
            assert!(text.contains("\n  }\n}"), "{text}");
            assert_eq!(
                value["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
                "lint"
            );
            let start = value["hooks"]["SessionStart"].as_array().unwrap();
            assert_eq!(start.len(), 2);
            assert_eq!(start[0]["hooks"][0]["command"], "node other.cjs");
            assert_eq!(start[1]["hooks"][0]["timeout"], 10);
            assert_eq!(status(&t, None).unwrap().status, "installed");
        }
    }

    #[test]
    fn a_different_devlink_entry_conflicts_unless_replaced() {
        let root = tempfile::tempdir().unwrap();
        let t = target(root.path(), ToolType::ClaudeCode);
        let old = "C:/old/devlink.exe --config C:/old/config.json updates-hook --host claude-code --event user-prompt-submit";
        let codex = "devlink updates-hook --host codex --event user-prompt-submit";
        seed(
            &t,
            &serde_json::to_vec(&json!({"hooks":{"UserPromptSubmit":[
                {"hooks":[{"type":"command","command":old},{"type":"command","command":"keep-me"}]},
                {"hooks":[{"type":"command","command":codex}]}
            ]}}))
            .unwrap(),
        );
        assert_eq!(status(&t, None).unwrap().status, "partial");
        let plan = plan_install(&t, wanted_at(t.tool, root.path()), false).unwrap();
        assert_eq!(plan.change, HookChange::Conflict);
        assert_eq!(plan.existing["UserPromptSubmit"], [old]);
        let before = fs::read(&t.path).unwrap();
        assert_eq!(
            apply(&t, &plan).unwrap_err().code,
            "SETUP_MCP_CONFIG_CONFLICT"
        );
        assert_eq!(fs::read(&t.path).unwrap(), before);

        install(&t, root.path(), true).unwrap();
        let value = json(&t);
        let groups = value["hooks"]["UserPromptSubmit"].as_array().unwrap();
        // The shared group keeps the unrelated hook; the other Host's entry is untouched.
        assert_eq!(groups[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(groups[0]["hooks"][0]["command"], "keep-me");
        assert_eq!(groups[1]["hooks"][0]["command"], codex);
        assert_eq!(status(&t, None).unwrap().status, "installed");
    }

    #[test]
    fn uninstall_removes_only_this_hosts_devlink_entries() {
        let root = tempfile::tempdir().unwrap();
        let t = target(root.path(), ToolType::Codex);
        seed(&t, r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"node other.cjs"}]}]}}"#.as_ref());
        install(&t, root.path(), false).unwrap();
        let plan = plan_uninstall(&t).unwrap();
        assert_eq!(plan.change, HookChange::Remove);
        apply(&t, &plan).unwrap();
        let value = json(&t);
        assert_eq!(
            value["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "node other.cjs"
        );
        assert!(value["hooks"].get("UserPromptSubmit").is_none());
        assert_eq!(status(&t, None).unwrap().status, "missing");
        assert_eq!(plan_uninstall(&t).unwrap().change, HookChange::Unchanged);
    }

    #[test]
    fn invalid_or_duplicate_json_and_concurrent_edits_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let t = target(root.path(), ToolType::ClaudeCode);
        for bytes in [
            &b"{not json"[..],
            br#"{"hooks":{},"hooks":{}}"#,
            br#"{"hooks":{"SessionStart":{}}}"#,
        ] {
            seed(&t, bytes);
            let error = plan_install(&t, wanted_at(t.tool, root.path()), false).unwrap_err();
            assert_eq!(error.code, "HOOKS_CONFIG_INVALID");
            assert_eq!(error.path.as_deref(), Some(&t.path));
            assert_eq!(status(&t, None).unwrap_err().code, "HOOKS_CONFIG_INVALID");
            assert_eq!(fs::read(&t.path).unwrap(), bytes);
        }
        seed(&t, "{}".as_ref());
        let plan = plan_install(&t, wanted_at(t.tool, root.path()), false).unwrap();
        fs::write(&t.path, r#"{"edited":true}"#).unwrap();
        assert_eq!(
            apply(&t, &plan).unwrap_err().code,
            "SETUP_MCP_CONFIG_CONFLICT"
        );
        assert_eq!(fs::read_to_string(&t.path).unwrap(), r#"{"edited":true}"#);
    }

    #[test]
    fn one_satisfied_and_one_conflicting_event_is_a_conflict_without_writing() {
        let root = tempfile::tempdir().unwrap();
        let t = target(root.path(), ToolType::Codex);
        let wanted = wanted_at(t.tool, root.path());
        let other = "C:/other/devlink.exe updates-hook --host=codex --event session-start";
        seed(
            &t,
            &serde_json::to_vec(&json!({"hooks": {
                "SessionStart": [{"hooks": [{"type": "command", "command": other}]}],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": wanted["UserPromptSubmit"]}]}]
            }}))
            .unwrap(),
        );
        let before = fs::read(&t.path).unwrap();
        // `--host=codex` is recognized as this Host's DevLink entry.
        let plan = plan_install(&t, wanted.clone(), false).unwrap();
        assert_eq!(plan.change, HookChange::Conflict);
        assert_eq!(plan.existing["SessionStart"], [other]);
        assert!(!plan.existing.contains_key("UserPromptSubmit"));
        assert!(apply(&t, &plan).is_err());
        assert_eq!(fs::read(&t.path).unwrap(), before);
        assert_eq!(status(&t, Some(&wanted)).unwrap().status, "stale");
        assert_eq!(status(&t, None).unwrap().status, "installed");
    }

    #[test]
    fn untouched_events_keep_their_original_text_and_matcher_groups_survive() {
        let root = tempfile::tempdir().unwrap();
        let t = target(root.path(), ToolType::ClaudeCode);
        let wanted = wanted_at(t.tool, root.path());
        // Key order and an exponent literal in an event DevLink does not touch.
        let untouched =
            r#"[{"matcher":"Bash","hooks":[{"type":"command","command":"lint","timeout":1e1}]}]"#;
        let stale = "C:/old/devlink.exe --config C:/old/c.json updates-hook --host claude-code --event session-start";
        let seeded = format!(
            r#"{{"hooks":{{"PostToolUse":{untouched},"SessionStart":[{{"matcher":"startup","hooks":[{{"type":"command","command":"{stale}"}},{{"type":"command","command":"keep"}}]}}],"UserPromptSubmit":[{{"hooks":[{{"type":"command","command":"{}"}}]}}]}}}}"#,
            wanted["UserPromptSubmit"]
        );
        seed(&t, seeded.as_bytes());
        install(&t, root.path(), true).unwrap();
        let text = fs::read_to_string(&t.path).unwrap();
        assert!(text.contains(untouched), "{text}");
        let value = json(&t);
        let start = value["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(start[0]["matcher"], "startup");
        assert_eq!(start[0]["hooks"].as_array().unwrap().len(), 1);
        assert_eq!(start[0]["hooks"][0]["command"], "keep");
        assert_eq!(start[1]["hooks"][0]["command"], wanted["SessionStart"]);
        assert_eq!(status(&t, Some(&wanted)).unwrap().status, "installed");
    }

    #[test]
    fn unix_backslashes_and_unc_paths_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.json");
        #[cfg(unix)]
        {
            let escaped = root.path().join("a\\b").join("devlink");
            assert_eq!(
                command(ToolType::Codex, &escaped, &config, "x")
                    .unwrap_err()
                    .code,
                "HOOKS_PATH_UNSUPPORTED"
            );
        }
        #[cfg(windows)]
        {
            let unc = Path::new(r"\\server\share\devlink.exe");
            assert_eq!(
                command(ToolType::Codex, unc, &config, "x")
                    .unwrap_err()
                    .code,
                "HOOKS_PATH_UNSUPPORTED"
            );
        }
    }
}
