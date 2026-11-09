use super::*;
use indexmap::IndexMap;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

pub(super) fn entry(input: &Normalized) -> Value {
    let mut args = input.executable.args.clone();
    if let Some(script) = args.first_mut() {
        *script = storage::host_path(std::path::Path::new(script));
    }
    let mut value = json!({"command":storage::host_path(&input.executable.command),"args":args,
        "env":{"DEVLINK_BROKER_FILE":storage::host_path(&input.broker)}});
    if input.tool == ToolType::ClaudeCode {
        value["type"] = json!("stdio");
    }
    value
}
pub(super) fn merge(
    tool: ToolType,
    before: Option<&[u8]>,
    wanted: &Value,
) -> Result<(Change, Vec<u8>)> {
    merge_with_replacement(tool, before, wanted, false)
}

pub(super) fn merge_with_replacement(
    tool: ToolType,
    before: Option<&[u8]>,
    wanted: &Value,
    replace_devlink: bool,
) -> Result<(Change, Vec<u8>)> {
    let change = if before.is_none() {
        Change::Create
    } else {
        Change::Merge
    };
    match tool {
        ToolType::ClaudeCode => {
            let conflict = || ConfigError::new("SETUP_MCP_CONFIG_CONFLICT");
            let original = before.unwrap_or(b"{}");
            // Validate duplicates first; raw unrelated values preserve large numbers exactly.
            let _ = serde_json::from_slice::<Unique>(original).map_err(|_| conflict())?;
            let mut root: IndexMap<String, Box<RawValue>> =
                serde_json::from_slice(original).map_err(|_| conflict())?;
            if root.contains_key("localToken") && root.contains_key("brokerUrl") {
                return Err(ConfigError::new("SETUP_UNSAFE_FILE"));
            }
            let mut servers: IndexMap<String, Box<RawValue>> = match root.get("mcpServers") {
                Some(raw) => serde_json::from_str(raw.get()).map_err(|_| conflict())?,
                None => IndexMap::new(),
            };
            if let Some(existing) = servers.get("devlink") {
                let existing: Value =
                    serde_json::from_str(existing.get()).map_err(|_| conflict())?;
                if &existing == wanted || !replace_devlink {
                    return Ok((
                        if &existing == wanted {
                            Change::Unchanged
                        } else {
                            Change::Conflict
                        },
                        before.unwrap_or_default().to_vec(),
                    ));
                }
            }
            servers.insert(
                "devlink".into(),
                RawValue::from_string(serde_json::to_string(wanted).map_err(|_| conflict())?)
                    .map_err(|_| conflict())?,
            );
            root.insert(
                "mcpServers".into(),
                RawValue::from_string(serde_json::to_string(&servers).map_err(|_| conflict())?)
                    .map_err(|_| conflict())?,
            );
            let mut bytes = serde_json::to_vec_pretty(&root)
                .map_err(|_| ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"))?;
            bytes.push(b'\n');
            Ok((change, bytes))
        }
        ToolType::Codex => {
            let original = std::str::from_utf8(before.unwrap_or_default())
                .map_err(|_| ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"))?;
            let mut doc = original
                .parse::<toml_edit::DocumentMut>()
                .map_err(|_| ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"))?;
            if !doc.contains_key("mcp_servers") {
                doc["mcp_servers"] = toml_edit::Item::Table(toml_edit::Table::new());
            }
            let servers = doc["mcp_servers"]
                .as_table_like_mut()
                .ok_or_else(|| ConfigError::new("SETUP_MCP_CONFIG_CONFLICT"))?;
            if let Some(existing) = servers.get("devlink") {
                let unchanged = toml_json(existing).as_ref() == Some(wanted);
                if unchanged || !replace_devlink {
                    return Ok((
                        if unchanged {
                            Change::Unchanged
                        } else {
                            Change::Conflict
                        },
                        before.unwrap_or_default().to_vec(),
                    ));
                }
            }
            servers.insert("devlink", toml_edit::Item::Value(to_toml(wanted)?));
            Ok((change, doc.to_string().into_bytes()))
        }
    }
}
pub(super) fn configured_entry(tool: ToolType, bytes: &[u8]) -> Result<Option<Value>> {
    let conflict = || ConfigError::new("SETUP_MCP_CONFIG_CONFLICT");
    match tool {
        ToolType::ClaudeCode => {
            let Unique(root) = serde_json::from_slice(bytes).map_err(|_| conflict())?;
            let root = root.as_object().ok_or_else(conflict)?;
            root.get("mcpServers")
                .map(|servers| {
                    let servers = servers.as_object().ok_or_else(conflict)?;
                    Ok(servers.get("devlink").cloned())
                })
                .unwrap_or(Ok(None))
        }
        ToolType::Codex => {
            let doc = std::str::from_utf8(bytes)
                .map_err(|_| conflict())?
                .parse::<toml_edit::DocumentMut>()
                .map_err(|_| conflict())?;
            doc.get("mcp_servers")
                .map(|servers| {
                    let servers = servers.as_table_like().ok_or_else(conflict)?;
                    servers
                        .get("devlink")
                        .map(|entry| toml_json(entry).ok_or_else(conflict))
                        .transpose()
                })
                .unwrap_or(Ok(None))
        }
    }
}

fn to_toml(value: &Value) -> Result<toml_edit::Value> {
    Ok(match value {
        Value::String(s) => toml_edit::Value::from(s.as_str()),
        Value::Array(items) => {
            let mut a = toml_edit::Array::new();
            for item in items {
                a.push(to_toml(item)?);
            }
            toml_edit::Value::Array(a)
        }
        Value::Object(fields) => {
            let mut t = toml_edit::InlineTable::new();
            for (key, value) in fields {
                t.insert(key, to_toml(value)?);
            }
            toml_edit::Value::InlineTable(t)
        }
        _ => return Err(ConfigError::new("SETUP_INPUT_INVALID")),
    })
}
fn toml_json(item: &toml_edit::Item) -> Option<Value> {
    if let Some(table) = item.as_table_like() {
        let mut map = Map::new();
        for (key, item) in table.iter() {
            map.insert(key.into(), toml_json(item)?);
        }
        return Some(Value::Object(map));
    }
    let value = item.as_value()?;
    if let Some(s) = value.as_str() {
        return Some(json!(s));
    }
    if let Some(a) = value.as_array() {
        return Some(Value::Array(
            a.iter()
                .map(|v| toml_json(&toml_edit::Item::Value(v.clone())))
                .collect::<Option<_>>()?,
        ));
    }
    None
}

// serde_json::Value ordinarily accepts duplicate keys and discards earlier data.
// Reject duplicates recursively so merging cannot silently erase another server.
pub(super) struct Unique(pub(super) Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Strict;
        impl<'de> Visitor<'de> for Strict {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("unique JSON object keys")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Unique, M::Error> {
                let mut result = Map::new();
                while let Some((key, Unique(value))) = map.next_entry::<String, Unique>()? {
                    if result.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("Duplicate key"));
                    }
                }
                Ok(Unique(Value::Object(result)))
            }
            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut seq: S,
            ) -> std::result::Result<Unique, S::Error> {
                let mut result = Vec::new();
                while let Some(Unique(v)) = seq.next_element::<Unique>()? {
                    result.push(v);
                }
                Ok(Unique(Value::Array(result)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
        }
        deserializer.deserialize_any(Strict)
    }
}
