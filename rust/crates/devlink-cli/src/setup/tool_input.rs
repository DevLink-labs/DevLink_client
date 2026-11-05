use devlink_broker::{sessions::ToolType, tool_discovery::ToolDetection};
use std::future::Future;

#[derive(Clone, Copy, Debug)]
pub enum ToolInputError {
    Unavailable,
    Invalid,
    Cancelled,
    ReadFailed,
}
pub trait ToolInput: Send {
    fn available(&self) -> bool;
    fn interactive(&self) -> bool {
        false
    }
    fn request(
        &mut self,
        tools: &[ToolDetection],
    ) -> impl Future<Output = Result<ToolType, ToolInputError>> + Send;
}
#[derive(Default)]
pub struct NoToolInput;
impl ToolInput for NoToolInput {
    fn available(&self) -> bool {
        false
    }
    async fn request(&mut self, _: &[ToolDetection]) -> Result<ToolType, ToolInputError> {
        Err(ToolInputError::Unavailable)
    }
}
