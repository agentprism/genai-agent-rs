//! Abortable provider-request retries ⇐ pi `src/utils/provider-retry.ts`.

use crate::types::AbortSignal;
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

#[derive(Clone, Default)]
pub struct ProviderRetryOptions {
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub signal: Option<Arc<dyn AbortSignal>>,
}

impl fmt::Debug for ProviderRetryOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRetryOptions")
            .field("max_retries", &self.max_retries)
            .field("max_retry_delay_ms", &self.max_retry_delay_ms)
            .field("signal", &self.signal.is_some())
            .finish()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderErrorMetadata {
    pub status: Option<u16>,
    pub headers: BTreeMap<String, String>,
}

pub trait ProviderRetryClassify {
    fn provider_error_metadata(&self) -> Option<&ProviderErrorMetadata>;
    fn provider_error_message(&self) -> String;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderRetryError<E> {
    Original(E),
    Abort,
    ServerDelay {
        requested_seconds: String,
        maximum_seconds: String,
        provider_message: String,
    },
}

impl<E> ProviderRetryError<E> {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Abort => "AbortError",
            Self::Original(_) | Self::ServerDelay { .. } => "Error",
        }
    }
}

impl<E: fmt::Display> fmt::Display for ProviderRetryError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Original(error) => error.fmt(formatter),
            Self::Abort => formatter.write_str("Request aborted"),
            Self::ServerDelay {
                requested_seconds,
                maximum_seconds,
                provider_message,
            } => write!(
                formatter,
                "Server requested {requested_seconds}s retry delay (max: {maximum_seconds}s). {provider_message}"
            ),
        }
    }
}

impl<E> std::error::Error for ProviderRetryError<E> where E: std::error::Error + 'static {}

pub async fn retry_provider_request<T, E, F, Fut>(
    mut request: F,
    options: ProviderRetryOptions,
) -> Result<T, ProviderRetryError<E>>
where
    E: ProviderRetryClassify,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let max_retries = options.max_retries.unwrap_or(0);
    let mut retries_remaining = max_retries;

    loop {
        match request().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if options
                    .signal
                    .as_ref()
                    .is_some_and(|signal| signal.is_aborted())
                {
                    return Err(ProviderRetryError::Abort);
                }

                let Some(metadata) = error.provider_error_metadata() else {
                    return Err(ProviderRetryError::Original(error));
                };
                if retries_remaining == 0 || !is_retryable_provider_error(metadata) {
                    return Err(ProviderRetryError::Original(error));
                }

                let retry_index = max_retries - retries_remaining;
                retries_remaining -= 1;
                let delay = get_retry_delay(
                    metadata,
                    retry_index,
                    options.max_retry_delay_ms,
                    &error.provider_error_message(),
                )?;
                abortable_sleep(delay, options.signal.as_deref()).await?;
            }
        }
    }
}

fn header<'a>(metadata: &'a ProviderErrorMetadata, name: &str) -> Option<&'a str> {
    metadata
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn is_retryable_provider_error(metadata: &ProviderErrorMetadata) -> bool {
    match header(metadata, "x-should-retry") {
        Some("true") => return true,
        Some("false") => return false,
        Some(_) | None => {}
    }
    metadata
        .status
        .is_none_or(|status| matches!(status, 408 | 409 | 429) || status >= 500)
}

fn get_retry_delay<E>(
    metadata: &ProviderErrorMetadata,
    retry_index: u32,
    max_retry_delay_ms: Option<u64>,
    provider_message: &str,
) -> Result<Duration, ProviderRetryError<E>> {
    if let Some(value) = header(metadata, "retry-after-ms").and_then(parse_js_float) {
        return validate_server_retry_delay(value, max_retry_delay_ms, provider_message);
    }

    if let Some(value) = header(metadata, "retry-after").filter(|value| !value.is_empty()) {
        let delay_ms = parse_js_float(value).map_or_else(
            || {
                httpdate::parse_http_date(value)
                    .ok()
                    .and_then(|date| date.duration_since(SystemTime::now()).ok())
                    .map_or(0.0, |duration| duration.as_secs_f64() * 1_000.0)
            },
            |seconds| seconds * 1_000.0,
        );
        return validate_server_retry_delay(delay_ms, max_retry_delay_ms, provider_message);
    }

    let exponent = i32::try_from(retry_index).unwrap_or(i32::MAX);
    let exponential_ms = (0.5 * 2_f64.powi(exponent)).min(8.0) * 1_000.0;
    let jitter = 1.0 - pseudo_random_fraction() * 0.25;
    Ok(duration_from_js_timeout(exponential_ms * jitter))
}

fn validate_server_retry_delay<E>(
    delay_ms: f64,
    max_retry_delay_ms: Option<u64>,
    provider_message: &str,
) -> Result<Duration, ProviderRetryError<E>> {
    let maximum = max_retry_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    if maximum > 0 && delay_ms > maximum as f64 {
        return Err(ProviderRetryError::ServerDelay {
            requested_seconds: ceil_seconds(delay_ms),
            maximum_seconds: ceil_seconds(maximum as f64),
            provider_message: provider_message.to_owned(),
        });
    }
    Ok(duration_from_js_timeout(delay_ms))
}

fn ceil_seconds(milliseconds: f64) -> String {
    if milliseconds == f64::INFINITY {
        "Infinity".to_owned()
    } else if milliseconds == f64::NEG_INFINITY || milliseconds.is_nan() {
        "0".to_owned()
    } else {
        format!("{:.0}", (milliseconds / 1_000.0).ceil())
    }
}

fn duration_from_js_timeout(milliseconds: f64) -> Duration {
    if milliseconds == f64::INFINITY {
        Duration::from_millis(1)
    } else if !milliseconds.is_finite() || milliseconds <= 0.0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64((milliseconds / 1_000.0).min(Duration::MAX.as_secs_f64()))
    }
}

fn parse_js_float(value: &str) -> Option<f64> {
    let trimmed = value.trim_start();
    let bytes = trimmed.as_bytes();
    let mut end = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    if trimmed[end..].starts_with("Infinity") {
        return Some(if bytes.first() == Some(&b'-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        });
    }
    let mut digits = 0;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
        digits += 1;
    }
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let exponent_start = end;
        end += 1;
        if matches!(bytes.get(end), Some(b'+' | b'-')) {
            end += 1;
        }
        let digits_start = end;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == digits_start {
            end = exponent_start;
        }
    }
    trimmed[..end].parse().ok()
}

fn pseudo_random_fraction() -> f64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0x9e37_79b9_7f4a_7c15, |duration| {
            u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
        });
    let mut current = STATE.load(Ordering::Relaxed);
    if current == 0 {
        let initialized = seed | 1;
        current = match STATE.compare_exchange(0, initialized, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => initialized,
            Err(observed) => observed,
        };
    }
    loop {
        let mut next = current;
        next ^= next << 13;
        next ^= next >> 7;
        next ^= next << 17;
        match STATE.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => {
                let upper = u32::try_from(next >> 32).expect("upper half fits u32");
                return f64::from(upper) / (f64::from(u32::MAX) + 1.0);
            }
            Err(observed) => current = observed,
        }
    }
}

async fn abortable_sleep<E>(
    duration: Duration,
    signal: Option<&dyn AbortSignal>,
) -> Result<(), ProviderRetryError<E>> {
    let Some(signal) = signal else {
        tokio::time::sleep(duration).await;
        return Ok(());
    };
    if signal.is_aborted() {
        return Err(ProviderRetryError::Abort);
    }
    tokio::select! {
        () = tokio::time::sleep(duration) => Ok(()),
        () = signal.cancelled() => Err(ProviderRetryError::Abort),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::BoxFuture;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::sync::Notify;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestError {
        message: String,
        metadata: Option<ProviderErrorMetadata>,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(&self.message)
        }
    }

    impl std::error::Error for TestError {}

    impl ProviderRetryClassify for TestError {
        fn provider_error_metadata(&self) -> Option<&ProviderErrorMetadata> {
            self.metadata.as_ref()
        }

        fn provider_error_message(&self) -> String {
            self.message.clone()
        }
    }

    fn provider_error(status: Option<u16>, headers: &[(&str, &str)]) -> TestError {
        TestError {
            message: format!("Provider error: {status:?}"),
            metadata: Some(ProviderErrorMetadata {
                status,
                headers: headers
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            }),
        }
    }

    #[derive(Default)]
    struct TestSignal {
        aborted: AtomicBool,
        notify: Notify,
    }

    impl TestSignal {
        fn abort(&self) {
            self.aborted.store(true, Ordering::SeqCst);
            self.notify.notify_waiters();
        }
    }

    impl AbortSignal for TestSignal {
        fn is_aborted(&self) -> bool {
            self.aborted.load(Ordering::SeqCst)
        }

        fn cancelled(&self) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                while !self.is_aborted() {
                    self.notify.notified().await;
                }
            })
        }
    }

    /// Ports pi `test/provider-retry.test.ts:16-30`.
    #[tokio::test(start_paused = true)]
    async fn retries_retryable_provider_errors() {
        let calls = Arc::new(AtomicUsize::new(0));
        let request_calls = Arc::clone(&calls);
        let task = tokio::spawn(async move {
            retry_provider_request(
                move || {
                    let call = request_calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            Err(provider_error(Some(429), &[("retry-after-ms", "1000")]))
                        } else {
                            Ok("ok")
                        }
                    }
                },
                ProviderRetryOptions {
                    max_retries: Some(1),
                    ..ProviderRetryOptions::default()
                },
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(999)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(task.await.expect("task"), Ok("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Ports pi `test/provider-retry.test.ts:32-38`.
    #[tokio::test]
    async fn provider_can_mark_error_non_retryable() {
        let calls = AtomicUsize::new(0);
        let result = retry_provider_request(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(provider_error(Some(429), &[("x-should-retry", "false")])) }
            },
            ProviderRetryOptions {
                max_retries: Some(2),
                ..ProviderRetryOptions::default()
            },
        )
        .await;
        assert!(matches!(result, Err(ProviderRetryError::Original(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Derived from pi `src/utils/provider-retry.ts:105-123` default and backoff policy.
    #[tokio::test]
    async fn defaults_to_zero_retries_and_uses_jittered_capped_backoff() {
        let calls = AtomicUsize::new(0);
        let result = retry_provider_request(
            || {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err::<(), _>(provider_error(Some(429), &[])) }
            },
            ProviderRetryOptions::default(),
        )
        .await;
        assert!(matches!(result, Err(ProviderRetryError::Original(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let metadata = ProviderErrorMetadata {
            status: Some(429),
            headers: BTreeMap::new(),
        };
        let first = get_retry_delay::<TestError>(&metadata, 0, None, "error").expect("delay");
        assert!((Duration::from_millis(375)..=Duration::from_millis(500)).contains(&first));
        let empty_header = ProviderErrorMetadata {
            status: Some(429),
            headers: BTreeMap::from([("retry-after".to_owned(), String::new())]),
        };
        let empty = get_retry_delay::<TestError>(&empty_header, 0, None, "error").expect("delay");
        assert!((Duration::from_millis(375)..=Duration::from_millis(500)).contains(&empty));
        let capped = get_retry_delay::<TestError>(&metadata, 10, None, "error").expect("delay");
        assert!((Duration::from_secs(6)..=Duration::from_secs(8)).contains(&capped));
    }

    /// Ports pi `test/provider-retry.test.ts:40-47`.
    #[tokio::test]
    async fn rejects_server_delay_above_cap() {
        let result = retry_provider_request(
            || async { Err::<(), _>(provider_error(Some(429), &[("retry-after", "277403")])) },
            ProviderRetryOptions {
                max_retries: Some(1),
                max_retry_delay_ms: Some(1_000),
                signal: None,
            },
        )
        .await;
        assert_eq!(
            result.expect_err("delay must fail").to_string(),
            "Server requested 277403s retry delay (max: 1s). Provider error: Some(429)"
        );
    }

    /// Ports pi `test/provider-retry.test.ts:49-63`.
    #[tokio::test(start_paused = true)]
    async fn zero_disables_server_delay_cap() {
        let calls = Arc::new(AtomicUsize::new(0));
        let request_calls = Arc::clone(&calls);
        let task = tokio::spawn(async move {
            retry_provider_request(
                move || {
                    let call = request_calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            Err(provider_error(Some(429), &[("retry-after", "2")]))
                        } else {
                            Ok("ok")
                        }
                    }
                },
                ProviderRetryOptions {
                    max_retries: Some(1),
                    max_retry_delay_ms: Some(0),
                    signal: None,
                },
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1_999)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_millis(1)).await;
        assert_eq!(task.await.expect("task"), Ok("ok"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// Ports pi `test/provider-retry.test.ts:65-80`.
    #[tokio::test(start_paused = true)]
    async fn aborts_provider_requested_delay() {
        let signal = Arc::new(TestSignal::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let request_calls = Arc::clone(&calls);
        let request_signal: Arc<dyn AbortSignal> = signal.clone();
        let task = tokio::spawn(async move {
            retry_provider_request(
                move || {
                    request_calls.fetch_add(1, Ordering::SeqCst);
                    async { Err::<(), _>(provider_error(Some(429), &[("retry-after", "277403")])) }
                },
                ProviderRetryOptions {
                    max_retries: Some(2),
                    max_retry_delay_ms: Some(0),
                    signal: Some(request_signal),
                },
            )
            .await
        });
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        signal.abort();
        let error = task.await.expect("task").expect_err("aborted");
        assert_eq!(error.name(), "AbortError");
        assert_eq!(error.to_string(), "Request aborted");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Derived from pi `src/utils/provider-retry.ts:22-66` for untested policy branches.
    #[tokio::test(start_paused = true)]
    async fn honors_header_override_http_date_and_malformed_retry_after() {
        assert!(is_retryable_provider_error(&ProviderErrorMetadata {
            status: Some(400),
            headers: BTreeMap::from([("X-Should-Retry".to_owned(), "true".to_owned())]),
        }));
        assert!(!is_retryable_provider_error(&ProviderErrorMetadata {
            status: Some(400),
            headers: BTreeMap::new(),
        }));
        assert!(is_retryable_provider_error(&ProviderErrorMetadata {
            status: None,
            headers: BTreeMap::new(),
        }));
        for status in [408, 409, 429, 500, 599] {
            assert!(is_retryable_provider_error(&ProviderErrorMetadata {
                status: Some(status),
                headers: BTreeMap::new(),
            }));
        }
        assert!(!is_retryable_provider_error(&ProviderErrorMetadata {
            status: Some(499),
            headers: BTreeMap::new(),
        }));
        assert_eq!(parse_js_float("  +1000.5ms"), Some(1000.5));
        assert_eq!(parse_js_float("Infinity"), Some(f64::INFINITY));

        let malformed = get_retry_delay::<TestError>(
            &ProviderErrorMetadata {
                status: Some(429),
                headers: BTreeMap::from([("retry-after".to_owned(), "not-a-date".to_owned())]),
            },
            0,
            None,
            "error",
        )
        .expect("malformed is immediate");
        assert_eq!(malformed, Duration::ZERO);

        let past = httpdate::fmt_http_date(SystemTime::now() - Duration::from_secs(1));
        let parsed = get_retry_delay::<TestError>(
            &ProviderErrorMetadata {
                status: Some(429),
                headers: BTreeMap::from([("retry-after".to_owned(), past)]),
            },
            0,
            None,
            "error",
        )
        .expect("date");
        assert_eq!(parsed, Duration::ZERO);

        let future = httpdate::fmt_http_date(SystemTime::now() + Duration::from_secs(120));
        let parsed = get_retry_delay::<TestError>(
            &ProviderErrorMetadata {
                status: Some(429),
                headers: BTreeMap::from([("retry-after".to_owned(), future)]),
            },
            0,
            Some(0),
            "error",
        )
        .expect("future date");
        assert!(parsed >= Duration::from_secs(118));

        let default_cap = get_retry_delay::<TestError>(
            &ProviderErrorMetadata {
                status: Some(429),
                headers: BTreeMap::from([("retry-after".to_owned(), "61".to_owned())]),
            },
            0,
            None,
            "error",
        )
        .expect_err("the default cap is sixty seconds");
        assert_eq!(
            default_cap.to_string(),
            "Server requested 61s retry delay (max: 60s). error"
        );

        let infinite = get_retry_delay::<TestError>(
            &ProviderErrorMetadata {
                status: Some(429),
                headers: BTreeMap::from([("retry-after-ms".to_owned(), "Infinity".to_owned())]),
            },
            0,
            None,
            "error",
        )
        .expect_err("infinite server delays exceed the cap");
        assert_eq!(
            infinite.to_string(),
            "Server requested Infinitys retry delay (max: 60s). error"
        );
    }
}
