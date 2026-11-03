use super::TeamChoice;
use std::future::Future;

#[derive(Clone, Copy, Debug)]
pub enum TeamInputError {
    Unavailable,
    Invalid,
    Cancelled,
    ReadFailed,
}

/// Canonical positive decimal IDs preserve the server's full signed-long range.
pub fn parse_team_id(value: &str) -> Result<u64, &'static str> {
    if value.is_empty() || value.starts_with('0') || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err("Use a canonical positive team ID");
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id <= i64::MAX as u64)
        .ok_or("Use a canonical positive team ID")
}

pub trait TeamInput: Send {
    fn available(&self) -> bool;
    fn interactive(&self) -> bool {
        false
    }
    fn request(
        &mut self,
        choices: &[TeamChoice],
    ) -> impl Future<Output = Result<u64, TeamInputError>> + Send;
}

#[derive(Default)]
pub struct NoTeamInput;
impl TeamInput for NoTeamInput {
    fn available(&self) -> bool {
        false
    }
    async fn request(&mut self, _: &[TeamChoice]) -> Result<u64, TeamInputError> {
        Err(TeamInputError::Unavailable)
    }
}
