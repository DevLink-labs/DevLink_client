use devlink_broker::agent_profile::registration::Selection;
use std::future::Future;

#[derive(Clone, Copy, Debug)]
pub enum AgentInputError {
    Unavailable,
    Invalid,
    Cancelled,
    ReadFailed,
}
pub trait AgentInput: Send {
    fn available(&self) -> bool;
    fn interactive(&self) -> bool {
        false
    }
    fn request(&mut self) -> impl Future<Output = Result<Selection, AgentInputError>> + Send;
}
pub struct NoAgentInput;
impl AgentInput for NoAgentInput {
    fn available(&self) -> bool {
        false
    }
    async fn request(&mut self) -> Result<Selection, AgentInputError> {
        Err(AgentInputError::Unavailable)
    }
}
