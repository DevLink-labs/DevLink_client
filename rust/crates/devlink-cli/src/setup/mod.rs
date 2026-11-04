//! Setup orchestration. Stage adapters own business operations; the flow never prompts.
mod agent_input;
mod flow;
mod login_input;
mod mcp_input;
mod model;
mod stages;
mod team_input;
pub mod terminal;
mod tool_input;

pub use agent_input::{AgentInput, AgentInputError, NoAgentInput};
pub use flow::{ProgressUnavailable, StepExecutor, run};
pub use login_input::{LoginCredentials, LoginInput, LoginInputError, NoLoginInput};
pub use mcp_input::{McpInput, McpInputError, NoMcpInput};
pub use model::*;
pub use stages::SetupStages;
pub use team_input::{NoTeamInput, TeamInput, TeamInputError, parse_team_id};
pub use tool_input::{NoToolInput, ToolInput, ToolInputError};
