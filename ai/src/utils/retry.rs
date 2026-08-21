//! Assistant retry policy ⇐ pi `src/utils/retry.ts`.

use crate::types::{AbortSignal, AssistantMessage, StopReason};
use crate::utils::sleep::duration_from_js_timeout;
use futures::future::BoxFuture;
use regex::Regex;
use std::future::Future;
use std::sync::{Arc, OnceLock};

fn non_retryable_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(&format!(
            "(?i){}",
            [
                "GoUsageLimitError",
                "FreeUsageLimitError",
                "Monthly usage limit reached",
                "available balance",
                "insufficient_quota",
                "out of budget",
                "quota exceeded",
                "billing",
            ]
            .join("|")
        ))
        .expect("static retry regex")
    })
}

fn retryable_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(&format!(
            "(?i){}",
            [
                "overloaded",
                "rate.?limit",
                "too many requests",
                "429",
                "500",
                "502",
                "503",
                "504",
                "524",
                "service.?unavailable",
                "server.?error",
                "internal.?error",
                "provider.?returned.?error",
                "exceeded request buffer limit while retrying upstream",
                "network.?error",
                "connection.?error",
                "connection.?refused",
                "connection.?lost",
                "other side closed",
                "fetch failed",
                "getaddrinfo",
                "ENOTFOUND",
                "EAI_AGAIN",
                "upstream.?connect",
                "reset before headers",
                "socket hang up",
                "socket connection was closed",
                "timed? out",
                "timeout",
                "terminated",
                "websocket.?closed",
                "websocket.?error",
                "ended without",
                "stream ended before message_stop",
                "stream ended before a terminal response event",
                "http2 request did not get a response",
                "retry delay",
                "you can retry your request",
                "try your request again",
                "please retry your request",
                "ResourceExhausted",
            ]
            .join("|")
        ))
        .expect("static retry regex")
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RetryPolicy {
    pub enabled: bool,
    pub max_retries: f64,
    pub base_delay_ms: f64,
}

pub trait RetryCallbacks: Send + Sync {
    fn on_retry_scheduled(
        &self,
        _attempt: u64,
        _max_attempts: f64,
        _delay_ms: f64,
        _error_message: &str,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn on_retry_attempt_start(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    fn on_retry_finished(
        &self,
        _success: bool,
        _attempt: u64,
        _final_error: Option<&str>,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error) = message.error_message.as_deref() else {
        return false;
    };
    !non_retryable_pattern().is_match(error) && retryable_pattern().is_match(error)
}

async fn retry_sleep(milliseconds: f64, signal: Option<&dyn AbortSignal>) -> bool {
    if signal.is_some_and(AbortSignal::is_aborted) {
        return false;
    }
    let duration = duration_from_js_timeout(milliseconds);
    if let Some(signal) = signal {
        tokio::select! {
            biased;
            _ = signal.cancelled() => false,
            _ = tokio::time::sleep(duration) => true,
        }
    } else {
        tokio::time::sleep(duration).await;
        true
    }
}

pub async fn retry_assistant_call<F, Fut>(
    mut produce: F,
    policy: Option<RetryPolicy>,
    signal: Option<Arc<dyn AbortSignal>>,
    callbacks: Option<&dyn RetryCallbacks>,
) -> AssistantMessage
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AssistantMessage>,
{
    let max_attempts = policy
        .filter(|policy| policy.enabled)
        .map_or(0.0, |policy| policy.max_retries);
    let mut attempt = 0_u64;
    let mut last_retry: Option<(u64, String)> = None;
    loop {
        let mut response = produce().await;
        if response.stop_reason == StopReason::Aborted {
            if let (Some(callbacks), Some((attempt, _))) = (callbacks, &last_retry) {
                callbacks.on_retry_finished(false, *attempt, None).await;
            }
            return response;
        }
        if response.stop_reason != StopReason::Error {
            if let (Some(callbacks), Some((attempt, _))) = (callbacks, &last_retry) {
                callbacks.on_retry_finished(true, *attempt, None).await;
            }
            return response;
        }
        if attempt as f64 >= max_attempts || !is_retryable_assistant_error(&response) {
            if let (Some(callbacks), Some((attempt, _))) = (callbacks, &last_retry) {
                callbacks
                    .on_retry_finished(false, *attempt, response.error_message.as_deref())
                    .await;
            }
            return response;
        }

        attempt += 1;
        let error = response
            .error_message
            .clone()
            .unwrap_or_else(|| "Unknown error".to_owned());
        last_retry = Some((attempt, error.clone()));
        let delay_ms = policy.expect("retry policy enabled").base_delay_ms
            * 2_f64.powf(attempt.saturating_sub(1) as f64);
        if let Some(callbacks) = callbacks {
            callbacks
                .on_retry_scheduled(attempt, max_attempts, delay_ms, &error)
                .await;
        }
        if !retry_sleep(delay_ms, signal.as_deref()).await {
            if let Some(callbacks) = callbacks {
                callbacks
                    .on_retry_finished(false, attempt, Some(&error))
                    .await;
            }
            response.stop_reason = StopReason::Aborted;
            response.error_message = None;
            return response;
        }
        if let Some(callbacks) = callbacks {
            callbacks.on_retry_attempt_start().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::abort::{AbortController, AbortReason};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn response(reason: StopReason, error: Option<&str>) -> AssistantMessage {
        let mut message = AssistantMessage::pending("test", "test", "test", 1);
        message.stop_reason = reason;
        message.error_message = error.map(str::to_owned);
        message
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl RetryCallbacks for Recorder {
        fn on_retry_scheduled(&self, attempt: u64, _: f64, _: f64, _: &str) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                self.0
                    .lock()
                    .expect("events")
                    .push(format!("retry:{attempt}"));
            })
        }

        fn on_retry_attempt_start(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                self.0
                    .lock()
                    .expect("events")
                    .push("attempt-start".to_owned());
            })
        }

        fn on_retry_finished(
            &self,
            success: bool,
            attempt: u64,
            final_error: Option<&str>,
        ) -> BoxFuture<'_, ()> {
            let final_error = final_error.map(str::to_owned);
            Box::pin(async move {
                self.0.lock().expect("events").push(format!(
                    "finished:{success}:{attempt}:{}",
                    final_error.unwrap_or_default()
                ));
            })
        }
    }

    /// Ports pi `test/retry.test.ts:16-89`.
    #[test]
    fn ports_provider_retry_classification_matrix() {
        for error in [
            "You can retry your request",
            "Try your request again",
            "ResourceExhausted: request limit reached",
            "The socket connection was closed unexpectedly",
            "exceeded request buffer limit while retrying upstream",
            "getaddrinfo ENOTFOUND api.example.com",
            "EAI_AGAIN api.example.com",
            "OpenAI Responses stream ended before a terminal response event",
            "overloaded_error",
            "524 status code (no body)",
        ] {
            assert!(is_retryable_assistant_error(&response(
                StopReason::Error,
                Some(error)
            )));
        }
        for error in [
            "429 quota exceeded",
            "insufficient_quota",
            "billing disabled",
        ] {
            assert!(!is_retryable_assistant_error(&response(
                StopReason::Error,
                Some(error)
            )));
        }
        assert!(!is_retryable_assistant_error(&response(
            StopReason::Stop,
            None
        )));
    }

    /// Ports pi `test/retry.test.ts:92-226`.
    #[tokio::test]
    async fn ports_retry_loop_callbacks_success_exhaustion_and_abort() {
        let enabled = Some(RetryPolicy {
            enabled: true,
            max_retries: 3.0,
            base_delay_ms: 0.0,
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let produce_calls = calls.clone();
        let recorder = Recorder::default();
        let recovered = retry_assistant_call(
            move || {
                let call = produce_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call < 2 {
                        response(StopReason::Error, Some("terminated"))
                    } else {
                        response(StopReason::Stop, None)
                    }
                }
            },
            enabled,
            None,
            Some(&recorder),
        )
        .await;
        assert_eq!(recovered.stop_reason, StopReason::Stop);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            *recorder.0.lock().expect("events"),
            [
                "retry:1",
                "attempt-start",
                "retry:2",
                "attempt-start",
                "finished:true:2:",
            ]
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let produce_calls = calls.clone();
        let final_error = retry_assistant_call(
            move || {
                produce_calls.fetch_add(1, Ordering::SeqCst);
                async { response(StopReason::Error, Some("terminated")) }
            },
            enabled,
            None,
            None,
        )
        .await;
        assert_eq!(final_error.stop_reason, StopReason::Error);
        assert_eq!(calls.load(Ordering::SeqCst), 4);

        let controller = AbortController::new();
        let aborter = controller.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            aborter.abort(AbortReason::default_abort());
        });
        let aborted = retry_assistant_call(
            || async { response(StopReason::Error, Some("terminated")) },
            Some(RetryPolicy {
                enabled: true,
                max_retries: 5.0,
                base_delay_ms: 10_000.0,
            }),
            Some(controller.signal()),
            None,
        )
        .await;
        assert_eq!(aborted.stop_reason, StopReason::Aborted);
        assert_eq!(aborted.error_message, None);

        let non_retryable = retry_assistant_call(
            || async { response(StopReason::Error, Some("insufficient_quota")) },
            enabled,
            None,
            None,
        )
        .await;
        assert_eq!(non_retryable.stop_reason, StopReason::Error);
    }
}
