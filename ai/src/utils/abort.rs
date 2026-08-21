//! Abort helpers ⇐ pi `src/utils/abort.ts`.

use crate::types::AbortSignal;
use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortReason {
    pub name: String,
    pub message: String,
}

impl AbortReason {
    pub fn new(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            message: message.into(),
        }
    }

    pub fn default_abort() -> Self {
        Self::new("AbortError", "The operation was aborted")
    }
}

impl fmt::Display for AbortReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AbortReason {}

#[derive(Debug)]
struct ControllerSignal {
    _sender: watch::Sender<bool>,
    aborted: watch::Receiver<bool>,
    reason: Arc<Mutex<Option<AbortReason>>>,
}

impl AbortSignal for ControllerSignal {
    fn is_aborted(&self) -> bool {
        *self.aborted.borrow()
    }

    fn cancelled(&self) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(async move {
            if self.is_aborted() {
                return;
            }
            let mut receiver = self.aborted.clone();
            while receiver.changed().await.is_ok() {
                if *receiver.borrow() {
                    return;
                }
            }
        })
    }

    fn reason(&self) -> Option<AbortReason> {
        self.reason
            .lock()
            .expect("abort reason mutex poisoned")
            .clone()
    }
}

#[derive(Debug, Clone)]
pub struct AbortController {
    sender: watch::Sender<bool>,
    reason: Arc<Mutex<Option<AbortReason>>>,
    signal: Arc<ControllerSignal>,
}

impl Default for AbortController {
    fn default() -> Self {
        Self::new()
    }
}

impl AbortController {
    pub fn new() -> Self {
        let (sender, aborted) = watch::channel(false);
        let reason = Arc::new(Mutex::new(None));
        let signal = Arc::new(ControllerSignal {
            _sender: sender.clone(),
            aborted,
            reason: reason.clone(),
        });
        Self {
            sender,
            reason,
            signal,
        }
    }

    pub fn signal(&self) -> Arc<dyn AbortSignal> {
        self.signal.clone()
    }

    pub fn abort(&self, reason: AbortReason) {
        let mut slot = self.reason.lock().expect("abort reason mutex poisoned");
        if slot.is_some() {
            return;
        }
        *slot = Some(reason);
        drop(slot);
        self.sender.send_replace(true);
    }
}

pub fn abort_reason(signal: &dyn AbortSignal) -> AbortReason {
    signal.reason().unwrap_or_else(AbortReason::default_abort)
}

pub fn operation_signal(signal: Option<Arc<dyn AbortSignal>>) -> Arc<dyn AbortSignal> {
    signal.unwrap_or_else(|| AbortController::new().signal())
}

pub async fn race_with_abort_signal<F>(
    operation: F,
    signal: Arc<dyn AbortSignal>,
) -> Result<F::Output, AbortReason>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let task = tokio::spawn(operation);
    if signal.is_aborted() {
        return Err(abort_reason(signal.as_ref()));
    }
    tokio::select! {
        biased;
        _ = signal.cancelled() => Err(abort_reason(signal.as_ref())),
        result = task => Ok(result.expect("observed operation task panicked")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn race_stops_waiting_but_observes_operation() {
        let controller = AbortController::new();
        let finished = Arc::new(AtomicBool::new(false));
        let task_finished = finished.clone();
        let signal = controller.signal();
        let operation = async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            task_finished.store(true, Ordering::SeqCst);
            7
        };
        controller.abort(AbortReason::new("CustomAbort", "stop"));
        assert_eq!(
            race_with_abort_signal(operation, signal)
                .await
                .expect_err("aborted"),
            AbortReason::new("CustomAbort", "stop")
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(finished.load(Ordering::SeqCst));
    }

    /// Pins pi `src/utils/abort.ts:10-12`: an operation-local signal remains live.
    #[tokio::test]
    async fn operation_signal_without_controller_does_not_self_abort() {
        let signal = operation_signal(None);
        assert!(!signal.is_aborted());
        assert!(
            tokio::time::timeout(Duration::from_millis(1), signal.cancelled())
                .await
                .is_err()
        );
    }
}
