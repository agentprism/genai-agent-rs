//! Abortable sleep ⇐ pi `src/utils/sleep.ts`.

use crate::types::AbortSignal;
use crate::utils::abort::{AbortReason, abort_reason};
use std::sync::Arc;
use std::time::Duration;

pub(crate) fn duration_from_js_timeout(milliseconds: f64) -> Duration {
    if !milliseconds.is_finite() || milliseconds <= 0.0 || milliseconds > f64::from(i32::MAX) {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(milliseconds / 1_000.0)
    }
}

pub async fn sleep(milliseconds: f64, signal: Arc<dyn AbortSignal>) -> Result<(), AbortReason> {
    if signal.is_aborted() {
        return Err(abort_reason(signal.as_ref()));
    }
    let duration = duration_from_js_timeout(milliseconds);
    tokio::select! {
        biased;
        _ = signal.cancelled() => Err(abort_reason(signal.as_ref())),
        _ = tokio::time::sleep(duration) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::abort::{AbortController, AbortReason};

    /// Pins pi `src/utils/sleep.ts:1-13` pre-abort and zero-delay behavior.
    #[tokio::test]
    async fn resolves_timer_and_rejects_with_signal_reason() {
        sleep(0.0, AbortController::new().signal())
            .await
            .expect("zero timer");
        let controller = AbortController::new();
        controller.abort(AbortReason::new("CustomAbort", "stop"));
        assert_eq!(
            sleep(10_000.0, controller.signal())
                .await
                .expect_err("aborted"),
            AbortReason::new("CustomAbort", "stop")
        );
    }
}
