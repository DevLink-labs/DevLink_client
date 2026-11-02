//! Provider demonstration, not the product setup CLI (-111 owns that integration).
use devlink_broker::mcp_config::{
    ConfigError, InspectionInput, McpConfig, PlanInput, VerifiedHost,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io::{Read, Write};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    host_version: String,
    input: PlanInput,
}
fn run(request: Request, mode: &str) -> Result<Value, ConfigError> {
    let mut provider = McpConfig::new(VerifiedHost::new(
        request.input.tool_type,
        &request.host_version,
    )?);
    let inspect = InspectionInput {
        tool_type: request.input.tool_type,
        target_path: request.input.target_path.clone(),
        broker_file: request.input.broker_file.clone(),
        executable: request.input.executable.clone(),
    };
    if mode == "--inspect" {
        return Ok(json!({"inspection":provider.inspect(inspect)?}));
    }
    let plan = provider.plan(request.input)?;
    if mode == "--apply" {
        let applied = provider.apply(&plan.plan_id, plan.expected_digest.as_deref())?;
        let inspection = provider.inspect(inspect);
        Ok(match inspection {
            Ok(value) => json!({"plan":plan,"apply":applied,"inspection":value}),
            Err(error) => json!({"plan":plan,"apply":applied,"inspectionError":error}),
        })
    } else {
        Ok(json!({"plan":plan}))
    }
}
fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mode = args.first().map(String::as_str).unwrap_or("--plan");
    let invalid = json!({"error":{"code":"SETUP_INPUT_INVALID","outcome":"not_applied","rollback":"not_needed"}});
    let mut bytes = Vec::new();
    let request = if args.len() <= 1
        && ["--plan", "--apply", "--inspect"].contains(&mode)
        && std::io::stdin()
            .take(131073)
            .read_to_end(&mut bytes)
            .is_ok()
        && bytes.len() <= 131072
    {
        serde_json::from_slice::<Request>(&bytes).ok()
    } else {
        None
    };
    let (value, code) = match request {
        Some(request) => match run(request, mode) {
            Ok(value) => (value, 0),
            Err(error) => (json!({"error":error}), 1),
        },
        None => (invalid, 2),
    };
    let mut stdout = std::io::stdout().lock();
    if serde_json::to_writer(&mut stdout, &value).is_err() || stdout.write_all(b"\n").is_err() {
        std::process::exit(1);
    }
    std::process::exit(code);
}
