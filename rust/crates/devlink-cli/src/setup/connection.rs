use super::{ErrorCode, Outcome, SetupError, Step};
use devlink_broker::{
    central::ConnectionStatus,
    mcp_config::InspectionStatus,
    setup_connection::{self, Broker, Credentials, Identity, ReadError, Report, Snapshot},
    setup_mcp::{Context, Options},
};
use std::{path::Path, time::Duration};
use tokio::time::{Instant, sleep_until, timeout_at};

pub(super) fn read_error(error: ReadError) -> Box<SetupError> {
    let code = match error {
        ReadError::UnsafeFile => ErrorCode::UnsafeFile,
        ReadError::Unavailable => ErrorCode::CentralUnavailable,
        ReadError::Protocol => ErrorCode::ProtocolError,
        ReadError::Changed => ErrorCode::ProfileConflict,
        ReadError::Unsupported => ErrorCode::BrokerStartFailed,
    };
    failure(code, None)
}

/// A Host restart replaces only the generic open/restart hint; ownership or
/// other connection guidance from the observation must remain visible.
pub(super) fn merge_restart_actions(connection: &mut Vec<&'static str>, restart: &[&'static str]) {
    connection.retain(|action| *action != "open_or_restart_host");
    for action in restart {
        if !connection.contains(action) {
            connection.push(action);
        }
    }
}

fn failure(code: ErrorCode, connection: Option<Snapshot>) -> Box<SetupError> {
    let mut error = SetupError::new(code, Step::VerifyConnection, Outcome::NotApplied);
    error.connection = connection;
    Box::new(error)
}

fn terminal_failure(state: &Snapshot) -> Option<ErrorCode> {
    use ConnectionStatus::*;
    if state.broker != Broker::Running {
        return Some(ErrorCode::BrokerStartFailed);
    }
    match (state.central, state.identity) {
        (Some(AuthFailed), _) | (_, Identity::Unauthenticated) => {
            Some(ErrorCode::CentralAuthFailed)
        }
        (Some(Replaced), _) => Some(ErrorCode::ConnectionReplaced),
        (Some(ProtocolViolation), _) | (_, Identity::Mismatch) => Some(ErrorCode::ProtocolError),
        (Some(InboxFailed), _) => Some(ErrorCode::InboxFailed),
        (_, _) if state.credentials != Credentials::Valid => Some(ErrorCode::UnsafeFile),
        (Some(Connected), Identity::Verified) => match state.mcp_config {
            InspectionStatus::Missing | InspectionStatus::Conflict => {
                Some(ErrorCode::McpConfigConflict)
            }
            InspectionStatus::Unknown => Some(ErrorCode::UnsafeFile),
            InspectionStatus::Applied => None,
        },
        (Some(Disconnected), _) => Some(ErrorCode::CentralUnavailable),
        _ => None,
    }
}

pub(super) async fn verify(
    binding: &Context,
    options: &Options,
    executable: &Path,
) -> Result<Report, Box<SetupError>> {
    const VERIFY_BUDGET: Duration = Duration::from_secs(20);
    const POLL_INTERVAL: Duration = Duration::from_millis(200);
    let deadline = Instant::now() + VERIFY_BUDGET;
    let mut last = None;
    loop {
        let read =
            setup_connection::inspect(&binding.config_path, options, executable, Some(binding));
        match timeout_at(deadline, read).await {
            Ok(Ok(report)) => {
                if report.connection.setup_ready {
                    return Ok(report);
                }
                let snapshot = report.connection;
                if let Some(code) = terminal_failure(&snapshot) {
                    return Err(failure(code, Some(snapshot)));
                }
                last = Some(snapshot);
            }
            Ok(Err(ReadError::Unavailable)) => {}
            Ok(Err(error)) => {
                let mut error = read_error(error);
                error.connection = last;
                return Err(error);
            }
            Err(_) => break,
        }
        let next = Instant::now() + POLL_INTERVAL;
        if next >= deadline {
            break;
        }
        sleep_until(next).await;
    }
    let code = if last.as_ref().is_some_and(|state| {
        state.central == Some(ConnectionStatus::Connected) && state.identity == Identity::Unknown
    }) {
        ErrorCode::ProtocolError
    } else {
        ErrorCode::Timeout
    };
    Err(failure(code, last))
}

#[cfg(test)]
mod tests {
    use super::merge_restart_actions;

    #[test]
    fn host_restart_keeps_connection_owner_guidance() {
        let mut actions = vec!["inspect_connection_owner"];
        merge_restart_actions(&mut actions, &["restart_host_then_register_agent"]);
        assert_eq!(
            actions,
            [
                "inspect_connection_owner",
                "restart_host_then_register_agent"
            ]
        );

        let mut actions = vec!["open_or_restart_host"];
        merge_restart_actions(&mut actions, &["restart_host_then_register_agent"]);
        assert_eq!(actions, ["restart_host_then_register_agent"]);
    }
}
