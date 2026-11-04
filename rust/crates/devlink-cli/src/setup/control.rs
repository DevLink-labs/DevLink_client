use super::ErrorCode;
use std::{
    future::{Future, pending},
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::time::{Instant, sleep_until};

pub(super) struct RunControl {
    pub deadline: Option<Instant>,
    signal: Pin<Box<dyn Future<Output = ErrorCode> + Send>>,
}

pub(super) enum Attempt<T> {
    Done(T),
    Interrupted {
        reason: ErrorCode,
        started: bool,
        completed: Option<T>,
    },
}

impl Default for RunControl {
    fn default() -> Self {
        Self::new(pending())
    }
}

impl RunControl {
    pub fn new(signal: impl Future<Output = ErrorCode> + Send + 'static) -> Self {
        Self {
            deadline: None,
            signal: Box::pin(signal),
        }
    }

    pub async fn attempt<T>(
        &mut self,
        operation: impl Future<Output = T>,
        local_write: Option<&AtomicBool>,
    ) -> Attempt<T> {
        let mut started = false;
        let (reason, completed) = {
            let work = async {
                started = true;
                operation.await
            };
            let mut work = std::pin::pin!(work);
            let reason = tokio::select! {
                biased;
                reason = &mut self.signal => reason,
                () = async {
                    match self.deadline {
                        Some(deadline) if Instant::now() < deadline => sleep_until(deadline).await,
                        Some(_) => {}
                        None => pending().await,
                    }
                } => ErrorCode::Timeout,
                value = &mut work => return Attempt::Done(value),
            };
            // A bounded, already-submitted local write must finish its atomic
            // publish/rollback before the process exits. Network and input wait
            // futures are dropped instead and no following stage is started.
            let completed = if local_write.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                Some(work.await)
            } else {
                None
            };
            (reason, completed)
        };
        Attempt::Interrupted {
            reason,
            started,
            completed,
        }
    }
}

pub(super) async fn terminal_signal() -> ErrorCode {
    #[cfg(windows)]
    let result = match tokio::signal::windows::ctrl_break() {
        Ok(mut console_break) => tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = console_break.recv() => Ok(()),
        },
        Err(error) => Err(error),
    };
    #[cfg(unix)]
    let result = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut terminate) => tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        },
        Err(error) => Err(error),
    };
    #[cfg(not(any(unix, windows)))]
    let result = tokio::signal::ctrl_c().await;
    if result.is_ok() {
        ErrorCode::Cancelled
    } else {
        ErrorCode::InternalError
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_waits_for_a_submitted_local_write() {
        let (send, receive) = tokio::sync::oneshot::channel();
        let write_active = AtomicBool::new(false);
        let mut control = RunControl::new(async move {
            receive.await.unwrap();
            ErrorCode::Cancelled
        });
        let operation = async {
            write_active.store(true, Ordering::Release);
            send.send(()).unwrap();
            tokio::task::yield_now().await;
            write_active.store(false, Ordering::Release);
        };
        assert!(matches!(
            control.attempt(operation, Some(&write_active)).await,
            Attempt::Interrupted {
                reason: ErrorCode::Cancelled,
                started: true,
                completed: Some(()),
            }
        ));
        assert!(!write_active.load(Ordering::Acquire));
    }
}
