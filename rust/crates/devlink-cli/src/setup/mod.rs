//! Setup orchestration. Stage adapters own business operations; the flow never prompts.
mod flow;
mod model;
mod stages;
pub mod terminal;

pub use flow::{ProgressUnavailable, StepExecutor, run};
pub use model::*;
pub use stages::SetupStages;
