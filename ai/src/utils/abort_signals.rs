//! Abort-signal composition ⇐ pi `src/utils/abort-signals.ts`.

use crate::types::AbortSignal;
use crate::utils::abort::{AbortController, AbortReason};
use std::sync::Arc;

pub struct CombinedAbortSignal {
    pub signal: Option<Arc<dyn AbortSignal>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl CombinedAbortSignal {
    pub fn cleanup(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Drop for CombinedAbortSignal {
    fn drop(&mut self) {
        self.cleanup();
    }
}

pub fn combine_abort_signals(signals: &[Option<Arc<dyn AbortSignal>>]) -> CombinedAbortSignal {
    let active = signals.iter().flatten().cloned().collect::<Vec<_>>();
    if active.is_empty() {
        return CombinedAbortSignal {
            signal: None,
            tasks: Vec::new(),
        };
    }
    if active.len() == 1 {
        return CombinedAbortSignal {
            signal: active.into_iter().next(),
            tasks: Vec::new(),
        };
    }

    let controller = AbortController::new();
    let mut tasks = Vec::new();
    for signal in active {
        if signal.is_aborted() {
            controller.abort(signal.reason().unwrap_or_else(AbortReason::default_abort));
            break;
        }
        let controller = controller.clone();
        tasks.push(tokio::spawn(async move {
            signal.cancelled().await;
            controller.abort(signal.reason().unwrap_or_else(AbortReason::default_abort));
        }));
    }
    CombinedAbortSignal {
        signal: Some(controller.signal()),
        tasks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::abort::{AbortController, AbortReason};

    #[tokio::test]
    async fn first_abort_reason_wins() {
        let first = AbortController::new();
        let second = AbortController::new();
        let combined = combine_abort_signals(&[Some(first.signal()), Some(second.signal())]);
        second.abort(AbortReason::new("Second", "second"));
        combined.signal.as_ref().expect("signal").cancelled().await;
        first.abort(AbortReason::new("First", "first"));
        assert_eq!(
            combined.signal.as_ref().expect("signal").reason(),
            Some(AbortReason::new("Second", "second"))
        );
    }
}
