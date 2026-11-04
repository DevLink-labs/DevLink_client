use devlink_broker::mcp_config::ConfigPlan;
use std::future::Future;

#[derive(Clone, Copy, Debug)]
pub enum McpInputError {
    Unavailable,
    Cancelled,
    ReadFailed,
}
pub trait McpInput: Send {
    fn available(&self) -> bool;
    fn interactive(&self) -> bool {
        false
    }
    fn request(
        &mut self,
        plan: &ConfigPlan,
    ) -> impl Future<Output = Result<bool, McpInputError>> + Send;
}
pub struct NoMcpInput;
impl McpInput for NoMcpInput {
    fn available(&self) -> bool {
        false
    }
    async fn request(&mut self, _: &ConfigPlan) -> Result<bool, McpInputError> {
        Err(McpInputError::Unavailable)
    }
}
