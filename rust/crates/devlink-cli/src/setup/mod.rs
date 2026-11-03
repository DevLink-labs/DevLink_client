//! Setup orchestration. Stage adapters own business operations; the flow never prompts.
mod flow;
mod login_input;
mod model;
mod stages;
mod team_input;
pub mod terminal;

pub use flow::{ProgressUnavailable, StepExecutor, run};
pub use login_input::{LoginCredentials, LoginInput, LoginInputError, NoLoginInput};
pub use model::*;
pub use stages::SetupStages;
pub use team_input::{NoTeamInput, TeamInput, TeamInputError, parse_team_id};
