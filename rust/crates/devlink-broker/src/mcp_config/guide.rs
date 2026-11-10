use super::*;
use crate::{agent_profile, config, credentials, private_file, setup_mcp, teams, user_auth};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyItem {
    pub scope: &'static str,
    pub syntax: &'static str,
    pub title: &'static str,
    pub snippet: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Guide {
    pub user_id: String,
    pub team_id: String,
    pub agent_id: String,
    pub project_path: PathBuf,
    pub tool_type: ToolType,
    pub automatic_supported: bool,
    pub target_path: PathBuf,
    pub observed_at: String,
    pub availability: &'static str,
    pub file_state: &'static str,
    pub edit_kind: &'static str,
    pub copy_items: Vec<CopyItem>,
    pub reason: &'static str,
    pub next_actions: Vec<&'static str>,
}

fn fingerprint(path: &Path, input: &PlanInput) -> Result<Vec<[u8; 32]>> {
    let mut hashes = Vec::new();
    for file in [
        agent_profile::path(path),
        teams::selection_path(path),
        user_auth::session_path(path),
        crate::setup_tools::path(path),
        credentials::path(path),
    ] {
        let bytes = private_file::read_bytes(&file, 16_384)
            .map_err(|e| ConfigError::unsafe_at(&file, e))?;
        hashes.push(Sha256::digest(bytes.as_deref().unwrap_or_default()).into());
    }
    for file in [
        path,
        &input.executable.command,
        Path::new(&input.executable.args[0]),
    ] {
        credentials::verify_public_file_permissions(file, true)
            .map_err(|e| ConfigError::unsafe_at(file, e))?;
        let mut file =
            std::fs::File::open(file).map_err(|_| ConfigError::new("SETUP_TOOL_UNAVAILABLE"))?;
        let mut hash = Sha256::new();
        std::io::copy(&mut file, &mut hash)
            .map_err(|_| ConfigError::new("SETUP_TOOL_UNAVAILABLE"))?;
        hashes.push(hash.finalize().into());
    }
    Ok(hashes)
}

/// 저장된 프로필만 조회하며 프로세스·중앙 요청·파일 쓰기를 수행하지 않음
pub fn inspect(path: &Path, options: &setup_mcp::Options, executable: &Path) -> Result<Guide> {
    let binding = agent_profile::inspection_binding(path)
        .map_err(|_| ConfigError::new("SETUP_PROFILE_CONFLICT"))?
        .ok_or_else(|| ConfigError::new("SETUP_PROFILE_CONFLICT"))?;
    if !user_auth::setup_session_matches(path, &binding.origin, binding.user_id)
        .map_err(|_| ConfigError::new("SETUP_PROFILE_CONFLICT"))?
    {
        return Err(ConfigError::new("SETUP_PROFILE_CONFLICT"));
    }
    let project = PathBuf::from(&binding.project_root);
    let selected = crate::setup_tools::inspect(
        path,
        &crate::setup_tools::SelectionBinding {
            origin: binding.origin.clone(),
            user_id: binding.user_id,
            team_id: binding.team_id,
            project_root: project.clone(),
        },
    )
    .map_err(|_| ConfigError::new("SETUP_PROFILE_CONFLICT"))?;
    if selected
        .preferred()
        .is_some_and(|tool| tool.tool_type != binding.tool)
    {
        return Err(ConfigError::new("SETUP_PROFILE_CONFLICT"));
    }
    // 저장된 Node 기반 Host의 런처를 setup과 같이 MCP 런타임 후보로 사용
    let context = setup_mcp::PreflightContext {
        config_path: path.into(),
        project,
        tool_type: binding.tool,
        host_launcher: selected
            .preferred()
            .map(|tool| tool.executable.command.clone()),
        mcp_automatic_supported: false,
    };
    let mut guide = Guide {
        user_id: binding.user_id.to_string(),
        team_id: binding.team_id.to_string(),
        agent_id: binding.agent_id.to_string(),
        project_path: context.project.clone(),
        tool_type: binding.tool,
        automatic_supported: selected
            .preferred()
            .is_some_and(|tool| tool.verified_host().is_ok()),
        target_path: project_target(binding.tool, &context.project),
        observed_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .expect("UTC timestamp"),
        availability: "needs_preparation",
        file_state: "unavailable",
        edit_kind: "none",
        copy_items: vec![],
        reason: "prepare_runtime",
        next_actions: vec!["resume_setup"],
    };
    let input = match setup_mcp::runtime_input(&context, options, executable) {
        Ok(input) => input,
        Err(error) if error.code == "SETUP_TOOL_UNAVAILABLE" => return Ok(guide),
        Err(error) => return Err(error),
    };
    // 저장된 런처는 runtime_input의 명시적 옵션 검사에 포함되지 않는다.
    if [
        input.executable.command.as_path(),
        Path::new(&input.executable.args[0]),
    ]
    .into_iter()
    .any(|file| {
        matches!(std::fs::symlink_metadata(file), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    }) {
        return Ok(guide);
    }
    let before = fingerprint(path, &input)?;
    if private_file::read_bytes(&credentials::path(path), 16_384)
        .map_err(|e| ConfigError::unsafe_at(&credentials::path(path), e))?
        .is_none()
    {
        guide.reason = "prepare_credentials";
        return Ok(guide);
    }
    let normalized = storage::normalize(input.tool_type, input.clone())?;
    storage::require_credentials(&normalized.broker)?;
    let bytes = storage::read(&normalized.target)?;
    let (state, kind, items, reason) = classify(&normalized, bytes.as_deref())?;
    storage::revalidate(&normalized)?;
    if fingerprint(path, &input)? != before
        || agent_profile::inspection_binding(path)
            .ok()
            .flatten()
            .as_ref()
            != Some(&binding)
        || config::Config::load(path)
            .and_then(|c| user_auth::origin(&c))
            .ok()
            .as_ref()
            .map(|url| url.as_str())
            != Some(binding.origin.as_str())
    {
        return Err(ConfigError::new("SETUP_PROFILE_CONFLICT"));
    }
    guide.availability = "ready";
    guide.file_state = state;
    guide.edit_kind = kind;
    guide.copy_items = items;
    guide.reason = reason;
    guide.next_actions = vec![if state == "matches" {
        "check_setup_status"
    } else {
        "review_mcp_config"
    }];
    Ok(guide)
}

type Classification = (&'static str, &'static str, Vec<CopyItem>, &'static str);

pub(super) fn classify(input: &Normalized, bytes: Option<&[u8]>) -> Result<Classification> {
    let wanted = format::entry(input);
    let item = |scope, snippet| CopyItem {
        scope,
        snippet,
        syntax: if input.tool == ToolType::Codex {
            "toml"
        } else {
            "json"
        },
        title: match scope {
            "whole_file" => "새 파일 전체 설정",
            "container" => "MCP 컨테이너 항목",
            _ => "DevLink 항목",
        },
    };
    let Some(bytes) = bytes else {
        let (_, bytes) = format::merge(input.tool, None, &wanted)?;
        return Ok((
            "file_missing",
            "create_file",
            vec![item("whole_file", String::from_utf8(bytes).unwrap())],
            "create_file",
        ));
    };
    let existing = match format::configured_entry(input.tool, bytes) {
        Ok(value) => value,
        Err(_) => {
            // 문법은 유효하지만 추가 옵션의 타입이 provider 비교 범위 밖인 항목도 교체 안내
            let present = input.tool == ToolType::Codex
                && std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
                    .is_some_and(|doc| {
                        doc.get("mcp_servers")
                            .and_then(|s| s.as_table_like())
                            .is_some_and(|servers| servers.contains_key("devlink"))
                    });
            if present {
                Some(serde_json::Value::Null)
            } else {
                return Ok(("invalid", "fix_file", vec![], "fix_syntax"));
            }
        }
    };
    let json_root = match input.tool {
        ToolType::ClaudeCode => {
            let format::Unique(root) = serde_json::from_slice(bytes)
                .map_err(|_| ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"))?;
            // 자격 파일을 Host 설정으로 잘못 복사한 경우 일치 여부와 관계없이 차단
            if root.get("localToken").is_some() && root.get("brokerUrl").is_some() {
                return Err(ConfigError::new("SETUP_UNSAFE_FILE"));
            }
            Some(root)
        }
        ToolType::Codex => None,
    };
    if existing.as_ref() == Some(&wanted) {
        return Ok(("matches", "none", vec![], "already_applied"));
    }
    let state = if existing.is_some() {
        "different"
    } else {
        "entry_missing"
    };
    let kind = if existing.is_some() {
        "replace_entry"
    } else {
        "add_entry"
    };
    match json_root {
        Some(root) => {
            let (scope, value, kind) = if root.get("mcpServers").is_some() {
                ("entry", serde_json::json!({"devlink":wanted}), kind)
            } else {
                (
                    "container",
                    serde_json::json!({"mcpServers":{"devlink":wanted}}),
                    "add_container",
                )
            };
            let json = serde_json::to_string_pretty(&value).unwrap();
            let snippet = json
                .trim_start_matches('{')
                .trim_end_matches('}')
                .trim()
                .to_owned();
            Ok((
                state,
                kind,
                vec![item(scope, snippet)],
                "merge_json_property",
            ))
        }
        None => {
            let doc = std::str::from_utf8(bytes)
                .unwrap()
                .parse::<toml_edit::DocumentMut>()
                .unwrap();
            let inline = doc
                .get("mcp_servers")
                .is_some_and(|servers| servers.is_inline_table());
            let snippet = if inline {
                format!("devlink = {}", format::to_toml(&wanted)?)
            } else {
                let mut doc = toml_edit::DocumentMut::new();
                let mut table = toml_edit::Table::new();
                for (key, value) in wanted.as_object().unwrap() {
                    table.insert(key, toml_edit::Item::Value(format::to_toml(value)?));
                }
                let mut servers = toml_edit::Table::new();
                servers.set_implicit(true);
                servers.insert("devlink", toml_edit::Item::Table(table));
                doc.insert("mcp_servers", toml_edit::Item::Table(servers));
                doc.to_string()
            };
            Ok((
                state,
                kind,
                vec![item("entry", snippet)],
                if inline {
                    "edit_inline_container"
                } else {
                    "merge_toml_table"
                },
            ))
        }
    }
}
