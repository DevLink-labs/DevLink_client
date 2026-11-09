use super::AgentChoice;
use devlink_broker::agent_metadata::AgentPart;
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
    fn routing_metadata(
        &mut self,
        part: Option<AgentPart>,
        roles: Option<Vec<String>>,
    ) -> impl Future<Output = Result<(AgentPart, Vec<String>), AgentInputError>> + Send {
        async move { Ok((part.unwrap_or_default(), roles.unwrap_or_default())) }
    }
    fn rename(&mut self) -> impl Future<Output = Result<String, AgentInputError>> + Send {
        async { Err(AgentInputError::Unavailable) }
    }
    fn confirm_issue(
        &mut self,
        _id: u64,
    ) -> impl Future<Output = Result<(), AgentInputError>> + Send {
        async { Err(AgentInputError::Unavailable) }
    }
    fn request(
        &mut self,
        choices: &[AgentChoice],
    ) -> impl Future<Output = Result<Selection, AgentInputError>> + Send;
}
pub struct NoAgentInput;
impl AgentInput for NoAgentInput {
    fn available(&self) -> bool {
        false
    }
    async fn request(&mut self, _: &[AgentChoice]) -> Result<Selection, AgentInputError> {
        Err(AgentInputError::Unavailable)
    }
}
