//! Strict v1 input for unattended setup. Values from this file never enter diagnostics.
use super::{
    ErrorCode, InputKind, InputSource, Outcome, RequiredInput, SetupError, Step, parse_team_id,
};
use devlink_broker::{
    agent_profile::registration::Selection,
    sessions::ToolType,
    setup_mcp::{Mode, Options},
    setup_project::ProjectOptions,
};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor, value::MapAccessDeserializer},
};
use std::{
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

const LIMIT: usize = 64 * 1024;

// Missing fields are aggregated after parsing. An explicit null still has to
// deserialize as T and is rejected. Struct deserialization also rejects duplicates.
struct Field<T>(Option<T>);
impl<T> Default for Field<T> {
    fn default() -> Self {
        Self(None)
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        T::deserialize(de).map(|value| Self(Some(value)))
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Input {
    schema_version: Field<u32>,
    server_url: Field<String>,
    #[serde(deserialize_with = "object")]
    user: User,
    #[serde(deserialize_with = "object")]
    team: Team,
    #[serde(deserialize_with = "object")]
    project: Project,
    #[serde(deserialize_with = "object")]
    tool: Tool,
    #[serde(deserialize_with = "object")]
    agent: Agent,
    #[serde(deserialize_with = "object")]
    mcp: Mcp,
    timeout_ms: Field<u64>,
}

fn object<'de, D: Deserializer<'de>, T: Deserialize<'de>>(de: D) -> Result<T, D::Error> {
    struct Object<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Object<T> {
        type Value = T;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(MapAccessDeserializer::new(map))
        }
    }
    de.deserialize_map(Object(std::marker::PhantomData))
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct User {
    mode: Field<UserMode>,
    email: Field<String>,
}
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum UserMode {
    Session,
    Login,
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Team {
    id: Field<Id>,
}
struct Id(u64);
impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let value = String::deserialize(de)?;
        parse_team_id(&value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Project {
    path: Field<PathBuf>,
    name: Field<String>,
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct Tool {
    #[serde(rename = "type")]
    kind: Field<Host>,
}
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Host {
    Codex,
    ClaudeCode,
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Agent {
    action: Field<Action>,
    id: Field<Id>,
    name: Field<String>,
    role_description: Field<String>,
    issue_token: Field<bool>,
}
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Create,
    Use,
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Mcp {
    mode: Field<McpMode>,
    target_path: Field<PathBuf>,
}
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum McpMode {
    Apply,
    Manual,
}

pub(super) struct Plan {
    pub server_url: Option<String>,
    pub login_email: Option<String>,
    pub team: u64,
    pub project: ProjectOptions,
    pub tool: ToolType,
    pub agent: Selection,
    pub issue_token: bool,
    pub mcp: Options,
    pub timeout: Duration,
}

pub(super) fn invalid() -> Box<SetupError> {
    Box::new(SetupError::new(
        ErrorCode::InputInvalid,
        Step::Validate,
        Outcome::NotApplied,
    ))
}

pub(super) fn required(field: &'static str) -> RequiredInput {
    let (kind, source) = match field {
        "password" => (InputKind::Secret, InputSource::HiddenPromptOrStdin),
        "team.id" => (InputKind::Selection, InputSource::VerifiedTeamList),
        "agent.id" => (InputKind::Selection, InputSource::OwnedAgentList),
        "tool.type" => (InputKind::Selection, InputSource::DetectedToolList),
        "user.mode" | "agent.action" | "mcp.mode" => (InputKind::Selection, InputSource::Input),
        _ => (InputKind::Value, InputSource::Input),
    };
    RequiredInput {
        field,
        kind,
        secret: field == "password",
        source,
    }
}

pub(super) fn missing(fields: impl IntoIterator<Item = &'static str>) -> Box<SetupError> {
    let mut error = SetupError::new(
        ErrorCode::InputRequired,
        Step::Validate,
        Outcome::NotApplied,
    );
    error.required_inputs = fields.into_iter().map(required).collect();
    Box::new(error)
}

pub(super) fn load(path: &Path, password_stdin: bool) -> Result<Plan, Box<SetupError>> {
    if path == Path::new("-")
        || !fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && m.len() <= LIMIT as u64)
    {
        return Err(invalid());
    }
    let file = File::open(path).map_err(|_| invalid())?;
    if !file
        .metadata()
        .is_ok_and(|m| m.is_file() && m.len() <= LIMIT as u64)
    {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() > LIMIT {
        return Err(invalid());
    }
    // Windows PowerShell 5.1 writes UTF-8 files with a signature. Accept only
    // one leading BOM; the JSON body itself remains strict UTF-8.
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&bytes);
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let input: Input = object(&mut deserializer).map_err(|_| invalid())?;
    deserializer.end().map_err(|_| invalid())?;
    input.validate(password_stdin)
}

fn bad_text(value: &Option<String>, limit: usize, empty_allowed: bool) -> bool {
    value.as_ref().is_some_and(|text| {
        (!empty_allowed && text.trim().is_empty()) || text.encode_utf16().count() > limit
    })
}

impl Input {
    fn validate(self, password_stdin: bool) -> Result<Plan, Box<SetupError>> {
        let login = self.user.mode.0 == Some(UserMode::Login);
        let create = self.agent.action.0 == Some(Action::Create);
        let use_agent = self.agent.action.0 == Some(Action::Use);
        let apply = self.mcp.mode.0 == Some(McpMode::Apply);
        if self.schema_version.0.is_some_and(|v| v != 1)
            || self
                .timeout_ms
                .0
                .is_some_and(|ms| !(30_000..=300_000).contains(&ms))
            || (self.user.mode.0 == Some(UserMode::Session)
                && (self.user.email.0.is_some() || password_stdin))
            || (create && (self.agent.id.0.is_some() || self.agent.issue_token.0.is_some()))
            || (use_agent
                && (self.project.name.0.is_some()
                    || self.agent.name.0.is_some()
                    || self.agent.role_description.0.is_some()))
            || (self.mcp.mode.0 == Some(McpMode::Manual) && self.mcp.target_path.0.is_some())
            || self
                .project
                .path
                .0
                .as_ref()
                .is_some_and(|p| !p.is_absolute())
            || self
                .mcp
                .target_path
                .0
                .as_ref()
                .is_some_and(|p| !p.is_absolute())
            || self
                .user
                .email
                .0
                .as_ref()
                .is_some_and(|v| v.trim().is_empty() || v.len() > 320)
            || bad_text(&self.project.name.0, 120, false)
            || bad_text(&self.agent.name.0, 100, false)
            || bad_text(&self.agent.role_description.0, 255, true)
        {
            return Err(invalid());
        }
        if let Some(url) = &self.server_url.0 {
            let config = devlink_broker::config::Config {
                server_url: url.clone(),
                ..Default::default()
            };
            devlink_broker::user_auth::origin(&config).map_err(|_| invalid())?;
        }
        let fields = [
            ("schemaVersion", self.schema_version.0.is_none()),
            ("user.mode", self.user.mode.0.is_none()),
            ("user.email", login && self.user.email.0.is_none()),
            ("password", login && !password_stdin),
            ("team.id", self.team.id.0.is_none()),
            ("project.path", self.project.path.0.is_none()),
            ("project.name", create && self.project.name.0.is_none()),
            ("tool.type", self.tool.kind.0.is_none()),
            ("agent.action", self.agent.action.0.is_none()),
            ("agent.id", use_agent && self.agent.id.0.is_none()),
            ("agent.name", create && self.agent.name.0.is_none()),
            ("mcp.mode", self.mcp.mode.0.is_none()),
            ("mcp.targetPath", apply && self.mcp.target_path.0.is_none()),
        ];
        if fields.iter().any(|(_, absent)| *absent) {
            return Err(missing(
                fields
                    .into_iter()
                    .filter_map(|(field, absent)| absent.then_some(field)),
            ));
        }

        Ok(Plan {
            server_url: self.server_url.0,
            login_email: self.user.email.0,
            team: self.team.id.0.ok_or_else(invalid)?.0,
            project: ProjectOptions {
                path: self.project.path.0,
                project_name: self.project.name.0,
                agent_name: self.agent.name.0,
                role_description: self.agent.role_description.0,
            },
            tool: match self.tool.kind.0.ok_or_else(invalid)? {
                Host::Codex => ToolType::Codex,
                Host::ClaudeCode => ToolType::ClaudeCode,
            },
            agent: if create {
                Selection::Create
            } else {
                Selection::Use(self.agent.id.0.ok_or_else(invalid)?.0)
            },
            issue_token: self.agent.issue_token.0.unwrap_or(false),
            mcp: Options {
                mode: if apply { Mode::Apply } else { Mode::Manual },
                target: self.mcp.target_path.0,
                apply,
                ..Default::default()
            },
            timeout: Duration::from_millis(self.timeout_ms.0.unwrap_or(60_000)),
        })
    }
}
