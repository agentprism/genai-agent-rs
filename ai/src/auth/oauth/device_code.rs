use super::now_millis;
use crate::auth::types::AuthError;
use crate::types::AbortSignal;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

const CANCEL_MESSAGE: &str = "Login cancelled";
const TIMEOUT_MESSAGE: &str = "Device flow timed out";
const SLOW_DOWN_TIMEOUT_MESSAGE: &str = "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again.";
const MINIMUM_INTERVAL_MS: f64 = 1_000.0;
const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
const SLOW_DOWN_INTERVAL_INCREMENT_MS: f64 = 5_000.0;

#[derive(Debug, Clone, PartialEq)]
pub enum OAuthDeviceCodePollResult<T> {
    Pending,
    SlowDown { interval_seconds: Option<f64> },
    Failed { message: String },
    Complete(T),
}

#[derive(Clone)]
pub struct OAuthDeviceCodePollOptions {
    pub interval_seconds: Option<f64>,
    pub expires_in_seconds: Option<f64>,
    pub wait_before_first_poll: bool,
    pub signal: Arc<dyn AbortSignal>,
}

pub async fn abortable_sleep(
    milliseconds: f64,
    signal: Arc<dyn AbortSignal>,
    cancel_message: &str,
) -> Result<(), AuthError> {
    if signal.is_aborted() {
        return Err(AuthError::new(cancel_message));
    }
    let milliseconds = if milliseconds.is_finite() && milliseconds > 0.0 {
        milliseconds
    } else {
        0.0
    };
    tokio::select! {
        biased;
        _ = signal.cancelled() => Err(AuthError::new(cancel_message)),
        _ = tokio::time::sleep(Duration::from_secs_f64(milliseconds / 1_000.0)) => Ok(()),
    }
}

pub async fn poll_oauth_device_code_flow<T, Poll, PollFuture>(
    options: OAuthDeviceCodePollOptions,
    mut poll: Poll,
) -> Result<T, AuthError>
where
    Poll: FnMut() -> PollFuture,
    PollFuture: Future<Output = Result<OAuthDeviceCodePollResult<T>, AuthError>>,
{
    let deadline = options
        .expires_in_seconds
        .map(|seconds| now_millis() + seconds * 1_000.0);
    let initial_interval = (options
        .interval_seconds
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS)
        * 1_000.0)
        .floor();
    let mut interval_ms = if initial_interval.is_nan() {
        0.0
    } else {
        MINIMUM_INTERVAL_MS.max(initial_interval)
    };
    let mut slow_down_responses = 0_u64;

    if options.wait_before_first_poll {
        let remaining_ms = deadline
            .map(|deadline| deadline - now_millis())
            .unwrap_or(f64::INFINITY);
        if remaining_ms > 0.0 {
            abortable_sleep(
                interval_ms.min(remaining_ms),
                options.signal.clone(),
                CANCEL_MESSAGE,
            )
            .await?;
        }
    }

    while deadline.is_none_or(|deadline| now_millis() < deadline) {
        if options.signal.is_aborted() {
            return Err(AuthError::new(CANCEL_MESSAGE));
        }

        match poll().await? {
            OAuthDeviceCodePollResult::Complete(value) => return Ok(value),
            OAuthDeviceCodePollResult::Failed { message } => return Err(AuthError::new(message)),
            OAuthDeviceCodePollResult::Pending => {}
            OAuthDeviceCodePollResult::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                interval_ms = match interval_seconds {
                    Some(seconds) if seconds.is_finite() && seconds > 0.0 => {
                        MINIMUM_INTERVAL_MS.max((seconds * 1_000.0).floor())
                    }
                    _ => MINIMUM_INTERVAL_MS.max(interval_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS),
                };
            }
        }

        let remaining_ms = deadline
            .map(|deadline| deadline - now_millis())
            .unwrap_or(f64::INFINITY);
        if remaining_ms <= 0.0 {
            break;
        }
        abortable_sleep(
            interval_ms.min(remaining_ms),
            options.signal.clone(),
            CANCEL_MESSAGE,
        )
        .await?;
    }

    Err(AuthError::new(if slow_down_responses > 0 {
        SLOW_DOWN_TIMEOUT_MESSAGE
    } else {
        TIMEOUT_MESSAGE
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::abort::{AbortController, AbortReason};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    fn options(interval_seconds: f64, wait_before_first_poll: bool) -> OAuthDeviceCodePollOptions {
        OAuthDeviceCodePollOptions {
            interval_seconds: Some(interval_seconds),
            expires_in_seconds: Some(30.0),
            wait_before_first_poll,
            signal: AbortController::new().signal(),
        }
    }

    /// Ports pi `test/oauth-device-code.test.ts:11`.
    #[tokio::test(start_paused = true)]
    async fn polls_immediately_then_at_the_requested_interval() {
        let count = Arc::new(Mutex::new(0_u32));
        let poll_count = count.clone();
        let task = tokio::spawn(async move {
            poll_oauth_device_code_flow(options(2.0, false), move || {
                let count = poll_count.clone();
                async move {
                    let mut count = count.lock().expect("count");
                    *count += 1;
                    Ok(if *count == 1 {
                        OAuthDeviceCodePollResult::Pending
                    } else {
                        OAuthDeviceCodePollResult::Complete("token")
                    })
                }
            })
            .await
        });
        tokio::task::yield_now().await;
        assert_eq!(*count.lock().expect("count"), 1);
        tokio::time::advance(Duration::from_millis(1_999)).await;
        assert_eq!(*count.lock().expect("count"), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(task.await.expect("task").expect("poll"), "token");
        assert_eq!(*count.lock().expect("count"), 2);
    }

    /// Ports pi `test/oauth-device-code.test.ts:44`.
    #[tokio::test(start_paused = true)]
    async fn can_wait_before_the_first_poll() {
        let count = Arc::new(Mutex::new(0_u32));
        let poll_count = count.clone();
        let task = tokio::spawn(async move {
            poll_oauth_device_code_flow(options(2.0, true), move || {
                let count = poll_count.clone();
                async move {
                    *count.lock().expect("count") += 1;
                    Ok(OAuthDeviceCodePollResult::Complete("token"))
                }
            })
            .await
        });
        tokio::time::advance(Duration::from_millis(1_999)).await;
        assert_eq!(*count.lock().expect("count"), 0);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(task.await.expect("task").expect("poll"), "token");
    }

    /// Ports pi `test/oauth-device-code.test.ts:68` and `:98`.
    #[tokio::test(start_paused = true)]
    async fn slow_down_adds_five_seconds_or_honors_the_server_interval() {
        for (server_interval, expected_delay) in [(None, 7_000), (Some(30.0), 30_000)] {
            let replies = Arc::new(Mutex::new(VecDeque::from([
                OAuthDeviceCodePollResult::SlowDown {
                    interval_seconds: server_interval,
                },
                OAuthDeviceCodePollResult::Complete("token"),
            ])));
            let poll_replies = replies.clone();
            let task = tokio::spawn(async move {
                poll_oauth_device_code_flow(options(2.0, false), move || {
                    let replies = poll_replies.clone();
                    async move { Ok(replies.lock().expect("replies").pop_front().expect("reply")) }
                })
                .await
            });
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(expected_delay - 1)).await;
            assert_eq!(replies.lock().expect("replies").len(), 1);
            tokio::time::advance(Duration::from_millis(1)).await;
            assert_eq!(task.await.expect("task").expect("poll"), "token");
        }
    }

    /// Ports pi `test/oauth-device-code.test.ts:131`.
    #[tokio::test]
    async fn cancels_an_in_flight_wait() {
        let controller = AbortController::new();
        let signal = controller.signal();
        let task = tokio::spawn(async move {
            poll_oauth_device_code_flow(
                OAuthDeviceCodePollOptions {
                    interval_seconds: Some(5.0),
                    expires_in_seconds: Some(30.0),
                    wait_before_first_poll: false,
                    signal,
                },
                || async { Ok::<_, AuthError>(OAuthDeviceCodePollResult::<()>::Pending) },
            )
            .await
        });
        tokio::task::yield_now().await;
        controller.abort(AbortReason::default_abort());
        assert_eq!(
            task.await.expect("task").expect_err("cancelled").message,
            "Login cancelled"
        );
    }

    /// Ports pi `test/github-copilot-oauth.test.ts:653`.
    #[tokio::test]
    async fn timeout_after_slow_down_uses_the_clock_drift_diagnostic() {
        let error = poll_oauth_device_code_flow(
            OAuthDeviceCodePollOptions {
                interval_seconds: Some(1.0),
                expires_in_seconds: Some(0.01),
                wait_before_first_poll: false,
                signal: AbortController::new().signal(),
            },
            || async {
                Ok::<_, AuthError>(OAuthDeviceCodePollResult::<()>::SlowDown {
                    interval_seconds: None,
                })
            },
        )
        .await
        .expect_err("timeout");
        assert_eq!(error.message, SLOW_DOWN_TIMEOUT_MESSAGE);
    }
}
