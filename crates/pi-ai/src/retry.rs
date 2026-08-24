//! Pi-compatible provider retry classification and cancellable establishment
//! from Architecture v2 part 2 §2.4.

use crate::{CancellationToken, LocalBoxFuture, MiddlewareError, SendBoxFuture, TransportError};
use futures_util::future::{Either, select};
use http::HeaderMap;
use std::fmt;
use std::future::Future;
use std::ops::RangeInclusive;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

/// Retry limits and backoff parameters resolved for one logical request.
#[derive(Clone, Debug, PartialEq)]
pub struct RetryPolicy {
    /// Number of retry attempts after the initial attempt.
    pub max_retries: u32,
    /// Maximum provider-requested delay. `Duration::ZERO` disables the cap.
    pub max_server_delay: Option<Duration>,
    /// Initial exponential delay.
    pub exponential_base: Duration,
    /// Maximum exponential delay.
    pub exponential_cap: Duration,
    /// Inclusive multiplier range used for downward jitter.
    pub jitter_multiplier: RangeInclusive<f64>,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            max_server_delay: Some(Duration::from_secs(60)),
            exponential_base: Duration::from_millis(500),
            exponential_cap: Duration::from_secs(8),
            jitter_multiplier: 0.75..=1.0,
        }
    }
}

/// One failed pre-stream attempt.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum AttemptFailure {
    /// HTTP transport failed before response headers were established.
    Transport {
        /// Zero-based attempt number.
        attempt: u32,
        /// Sanitized transport error.
        source: TransportError,
    },
    /// A transport attempt exceeded its configured establishment timeout.
    Timeout {
        /// Zero-based attempt number.
        attempt: u32,
        /// Configured attempt timeout.
        timeout: Duration,
    },
    /// Provider returned an HTTP response that could not establish a stream.
    Http {
        /// Zero-based attempt number.
        attempt: u32,
        /// HTTP status.
        status: u16,
        /// Raw response headers.
        headers: HeaderMap,
        /// Time at which the response was classified, used for HTTP-date delay.
        observed_at: SystemTime,
        /// Sanitized provider failure text.
        message: String,
    },
    /// Request middleware failed before transport establishment.
    Middleware {
        /// Zero-based attempt number.
        attempt: u32,
        /// Sanitized middleware error.
        source: MiddlewareError,
    },
    /// Caller cancelled the request or its backoff.
    Cancelled,
    /// A provider-requested delay exceeded policy and is rejected immediately.
    RetryDelayTooLong {
        /// Delay requested by the provider.
        requested: Duration,
        /// Maximum allowed delay.
        maximum: Duration,
        /// Original provider failure.
        source: Box<AttemptFailure>,
    },
}

impl fmt::Debug for AttemptFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { attempt, source } => formatter
                .debug_struct("AttemptFailure::Transport")
                .field("attempt", attempt)
                .field("source", source)
                .finish(),
            Self::Timeout { attempt, timeout } => formatter
                .debug_struct("AttemptFailure::Timeout")
                .field("attempt", attempt)
                .field("timeout", timeout)
                .finish(),
            Self::Http {
                attempt,
                status,
                observed_at,
                message,
                ..
            } => formatter
                .debug_struct("AttemptFailure::Http")
                .field("attempt", attempt)
                .field("status", status)
                .field("headers", &"<redacted headers>")
                .field("observed_at", observed_at)
                .field("message", message)
                .finish(),
            Self::Middleware { attempt, source } => formatter
                .debug_struct("AttemptFailure::Middleware")
                .field("attempt", attempt)
                .field("source", source)
                .finish(),
            Self::Cancelled => formatter.write_str("AttemptFailure::Cancelled"),
            Self::RetryDelayTooLong {
                requested,
                maximum,
                source,
            } => formatter
                .debug_struct("AttemptFailure::RetryDelayTooLong")
                .field("requested", requested)
                .field("maximum", maximum)
                .field("source", source)
                .finish(),
        }
    }
}

impl AttemptFailure {
    /// Creates a transport failure for an attempt.
    pub fn transport(attempt: u32, source: TransportError) -> Self {
        Self::Transport { attempt, source }
    }

    /// Creates an HTTP failure observed now.
    pub fn http(attempt: u32, status: u16, headers: HeaderMap, message: impl Into<String>) -> Self {
        Self::http_at(attempt, status, headers, SystemTime::now(), message)
    }

    /// Creates an HTTP failure with a deterministic observation time.
    pub fn http_at(
        attempt: u32,
        status: u16,
        headers: HeaderMap,
        observed_at: SystemTime,
        message: impl Into<String>,
    ) -> Self {
        Self::Http {
            attempt,
            status,
            headers,
            observed_at,
            message: message.into(),
        }
    }

    /// Returns the zero-based transport attempt when applicable.
    pub fn attempt(&self) -> Option<u32> {
        match self {
            Self::Transport { attempt, .. }
            | Self::Timeout { attempt, .. }
            | Self::Http { attempt, .. }
            | Self::Middleware { attempt, .. } => Some(*attempt),
            Self::RetryDelayTooLong { source, .. } => source.attempt(),
            Self::Cancelled => None,
        }
    }

    /// Returns the HTTP status when a response was established.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            Self::RetryDelayTooLong { source, .. } => source.status(),
            _ => None,
        }
    }

    fn headers(&self) -> Option<&HeaderMap> {
        match self {
            Self::Http { headers, .. } => Some(headers),
            Self::RetryDelayTooLong { source, .. } => source.headers(),
            _ => None,
        }
    }

    pub(crate) fn original(&self) -> &AttemptFailure {
        match self {
            Self::RetryDelayTooLong { source, .. } => source.original(),
            _ => self,
        }
    }

    fn observed_at(&self) -> SystemTime {
        match self {
            Self::Http { observed_at, .. } => *observed_at,
            Self::RetryDelayTooLong { source, .. } => source.observed_at(),
            _ => SystemTime::now(),
        }
    }
}

impl fmt::Display for AttemptFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Timeout { timeout, .. } => {
                write!(
                    formatter,
                    "request timed out after {}ms",
                    timeout.as_millis()
                )
            }
            Self::Middleware { source, .. } => fmt::Display::fmt(source, formatter),
            Self::Http {
                status, message, ..
            } => write!(
                formatter,
                "provider HTTP {status}: {}",
                if message.trim().is_empty() {
                    "provider rejected request before streaming"
                } else {
                    message
                }
            ),
            Self::Cancelled => formatter.write_str("request cancelled"),
            Self::RetryDelayTooLong {
                requested,
                maximum,
                source,
            } => write!(
                formatter,
                "server requested {}s retry delay (max: {}s): {source}",
                requested.as_secs_f64().ceil(),
                maximum.as_secs_f64().ceil()
            ),
        }
    }
}

impl std::error::Error for AttemptFailure {}

/// Retry decision for one failed pre-stream attempt.
#[derive(Clone, Debug, PartialEq)]
pub enum RetryDecision {
    /// Surface the failure immediately.
    DoNotRetry,
    /// Retry after the given delay.
    RetryAfter(Duration),
    /// Reject a provider-requested delay above the configured limit.
    RejectServerDelay {
        /// Delay requested by the provider.
        requested: Duration,
        /// Maximum allowed delay.
        maximum: Duration,
    },
}

/// Classifies a pre-stream provider failure.
pub trait RetryClassifier: Send + Sync + 'static {
    /// Returns whether and when the operation may be attempted again.
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision;

    /// Normalizes the final surfaced failure after retry classification has
    /// consumed any raw provider response text it needs.
    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        without_provider_body(failure)
    }
}

/// Single-threaded pre-stream retry classifier.
pub trait LocalRetryClassifier: 'static {
    /// Returns whether and when the operation may be attempted again.
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision;

    /// Local counterpart to [`RetryClassifier::normalize_terminal`].
    fn normalize_terminal(&self, failure: AttemptFailure) -> AttemptFailure {
        without_provider_body(failure)
    }
}

/// Source of a downward jitter multiplier.
pub trait RetryJitter: Send + Sync + 'static {
    /// Returns a value in the requested inclusive range.
    fn sample(&self, range: &RangeInclusive<f64>) -> f64;
}

/// Single-threaded source of a downward jitter multiplier.
pub trait LocalRetryJitter: 'static {
    /// Returns a value in the requested inclusive range.
    fn sample(&self, range: &RangeInclusive<f64>) -> f64;
}

/// Process-local random jitter source.
#[derive(Clone, Copy, Debug, Default)]
pub struct RandomRetryJitter;

impl RetryJitter for RandomRetryJitter {
    fn sample(&self, range: &RangeInclusive<f64>) -> f64 {
        let start = *range.start();
        let end = *range.end();
        if start >= end {
            start
        } else {
            fastrand::f64() * (end - start) + start
        }
    }
}

/// Process-local random jitter source for the local trait family.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalRandomRetryJitter;

impl LocalRetryJitter for LocalRandomRetryJitter {
    fn sample(&self, range: &RangeInclusive<f64>) -> f64 {
        let start = *range.start();
        let end = *range.end();
        if start >= end {
            start
        } else {
            fastrand::f64() * (end - start) + start
        }
    }
}

/// Pinned Pi/OpenAI/Anthropic retry classifier.
pub struct DefaultRetryClassifier {
    jitter: Box<dyn RetryJitter>,
}

impl Default for DefaultRetryClassifier {
    fn default() -> Self {
        Self::new(RandomRetryJitter)
    }
}

impl fmt::Debug for DefaultRetryClassifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DefaultRetryClassifier")
            .finish_non_exhaustive()
    }
}

impl DefaultRetryClassifier {
    /// Creates a classifier with an injected jitter source.
    pub fn new(jitter: impl RetryJitter) -> Self {
        Self {
            jitter: Box::new(jitter),
        }
    }

    fn server_delay(
        &self,
        failure: &AttemptFailure,
        policy: &RetryPolicy,
    ) -> Option<RetryDecision> {
        let headers = failure.headers()?;
        let delay = parse_retry_after_ms(headers)
            .or_else(|| parse_retry_after(headers, failure.observed_at()))?;

        if let Some(maximum) = policy.max_server_delay
            && !maximum.is_zero()
            && delay > maximum
        {
            return Some(RetryDecision::RejectServerDelay {
                requested: delay,
                maximum,
            });
        }
        Some(RetryDecision::RetryAfter(delay))
    }

    fn exponential_delay(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> Duration {
        let exponent = failure.attempt().unwrap_or(0).min(63);
        let factor = 2_u64.saturating_pow(exponent);
        let base = policy.exponential_base.mul_f64(factor as f64);
        let capped = base.min(policy.exponential_cap);
        let multiplier = self.jitter.sample(&policy.jitter_multiplier).clamp(
            *policy.jitter_multiplier.start(),
            *policy.jitter_multiplier.end(),
        );
        capped.mul_f64(multiplier)
    }
}

impl RetryClassifier for DefaultRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        let retryable = match failure {
            AttemptFailure::Transport { .. } | AttemptFailure::Timeout { .. } => true,
            AttemptFailure::Http {
                status, headers, ..
            } => match header_text(headers, "x-should-retry") {
                Some("true") => true,
                Some("false") => false,
                _ => matches!(*status, 408 | 409 | 429) || *status >= 500,
            },
            AttemptFailure::Middleware { .. }
            | AttemptFailure::Cancelled
            | AttemptFailure::RetryDelayTooLong { .. } => false,
        };
        if !retryable {
            return RetryDecision::DoNotRetry;
        }
        self.server_delay(failure, policy)
            .unwrap_or_else(|| RetryDecision::RetryAfter(self.exponential_delay(failure, policy)))
    }
}

/// Pinned Pi/OpenAI/Anthropic classifier for local provider graphs.
pub struct LocalDefaultRetryClassifier {
    jitter: Rc<dyn LocalRetryJitter>,
}

impl Default for LocalDefaultRetryClassifier {
    fn default() -> Self {
        Self::new(LocalRandomRetryJitter)
    }
}

impl fmt::Debug for LocalDefaultRetryClassifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalDefaultRetryClassifier")
            .finish_non_exhaustive()
    }
}

impl LocalDefaultRetryClassifier {
    /// Creates a local classifier with an injected local jitter source.
    pub fn new(jitter: impl LocalRetryJitter) -> Self {
        Self {
            jitter: Rc::new(jitter),
        }
    }

    fn server_delay(
        &self,
        failure: &AttemptFailure,
        policy: &RetryPolicy,
    ) -> Option<RetryDecision> {
        let headers = failure.headers()?;
        let delay = parse_retry_after_ms(headers)
            .or_else(|| parse_retry_after(headers, failure.observed_at()))?;

        if let Some(maximum) = policy.max_server_delay
            && !maximum.is_zero()
            && delay > maximum
        {
            return Some(RetryDecision::RejectServerDelay {
                requested: delay,
                maximum,
            });
        }
        Some(RetryDecision::RetryAfter(delay))
    }

    fn exponential_delay(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> Duration {
        let exponent = failure.attempt().unwrap_or(0).min(63);
        let factor = 2_u64.saturating_pow(exponent);
        let base = policy.exponential_base.mul_f64(factor as f64);
        let capped = base.min(policy.exponential_cap);
        let multiplier = self.jitter.sample(&policy.jitter_multiplier).clamp(
            *policy.jitter_multiplier.start(),
            *policy.jitter_multiplier.end(),
        );
        capped.mul_f64(multiplier)
    }
}

impl LocalRetryClassifier for LocalDefaultRetryClassifier {
    fn classify(&self, failure: &AttemptFailure, policy: &RetryPolicy) -> RetryDecision {
        let retryable = match failure {
            AttemptFailure::Transport { .. } | AttemptFailure::Timeout { .. } => true,
            AttemptFailure::Http {
                status, headers, ..
            } => match header_text(headers, "x-should-retry") {
                Some("true") => true,
                Some("false") => false,
                _ => matches!(*status, 408 | 409 | 429) || *status >= 500,
            },
            AttemptFailure::Middleware { .. }
            | AttemptFailure::Cancelled
            | AttemptFailure::RetryDelayTooLong { .. } => false,
        };
        if !retryable {
            return RetryDecision::DoNotRetry;
        }
        self.server_delay(failure, policy)
            .unwrap_or_else(|| RetryDecision::RetryAfter(self.exponential_delay(failure, policy)))
    }
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn parse_nonnegative_duration(value: &str, scale: f64) -> Option<Duration> {
    let parsed = value.parse::<f64>().ok()?;
    if parsed.is_nan() {
        return None;
    }
    if parsed <= 0.0 {
        return Some(Duration::ZERO);
    }
    if !parsed.is_finite() {
        return Some(Duration::MAX);
    }
    Some(Duration::from_secs_f64(parsed * scale))
}

fn parse_retry_after_ms(headers: &HeaderMap) -> Option<Duration> {
    parse_nonnegative_duration(header_text(headers, "retry-after-ms")?, 0.001)
}

fn parse_retry_after(headers: &HeaderMap, observed_at: SystemTime) -> Option<Duration> {
    let value = header_text(headers, "retry-after")?;
    if let Some(duration) = parse_nonnegative_duration(value, 1.0) {
        return Some(duration);
    }
    let requested_at = httpdate::parse_http_date(value).ok()?;
    Some(
        requested_at
            .duration_since(observed_at)
            .unwrap_or(Duration::ZERO),
    )
}

/// Executor-neutral delay used by the retry loop.
pub trait RetrySleeper: Send + Sync + 'static {
    /// Waits for a duration. Cancellation is enforced by the caller even if a
    /// custom sleeper ignores the token.
    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AttemptFailure>>;
}

/// Single-threaded retry timer abstraction.
pub trait LocalRetrySleeper: 'static {
    /// Waits for a duration. Cancellation is enforced by the caller.
    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), AttemptFailure>>;
}

/// Default executor-neutral timer backed by `futures-timer`.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultRetrySleeper;

impl RetrySleeper for DefaultRetrySleeper {
    fn sleep(
        &self,
        duration: Duration,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AttemptFailure>> {
        Box::pin(async move {
            futures_timer::Delay::new(duration).await;
            Ok(())
        })
    }
}

/// Default local executor-neutral timer backed by `futures-timer`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalDefaultRetrySleeper;

impl LocalRetrySleeper for LocalDefaultRetrySleeper {
    fn sleep(
        &self,
        duration: Duration,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<(), AttemptFailure>> {
        Box::pin(async move {
            futures_timer::Delay::new(duration).await;
            Ok(())
        })
    }
}

/// Establishes an operation with Pi-compatible retry policy and a portable
/// cancellable timer.
pub async fn establish_with_retry<T, F, Fut>(
    policy: &RetryPolicy,
    classifier: &dyn RetryClassifier,
    cancellation: &CancellationToken,
    attempt: F,
) -> Result<T, AttemptFailure>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AttemptFailure>>,
{
    establish_with_retry_and_sleeper(
        policy,
        classifier,
        &DefaultRetrySleeper,
        cancellation,
        attempt,
    )
    .await
}

/// Retry loop variant with an injected sleeper for hermetic tests and hosts
/// with their own clock implementation.
pub async fn establish_with_retry_and_sleeper<T, F, Fut>(
    policy: &RetryPolicy,
    classifier: &dyn RetryClassifier,
    sleeper: &dyn RetrySleeper,
    cancellation: &CancellationToken,
    mut attempt: F,
) -> Result<T, AttemptFailure>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AttemptFailure>>,
{
    let mut retry_index = 0;

    loop {
        cancellation
            .check()
            .map_err(|_| AttemptFailure::Cancelled)?;

        let attempt_result = cancellable(attempt(retry_index), cancellation).await;
        let error = match attempt_result {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        cancellation
            .check()
            .map_err(|_| AttemptFailure::Cancelled)?;
        if retry_index >= policy.max_retries {
            return Err(classifier.normalize_terminal(error));
        }

        let delay = match classifier.classify(&error, policy) {
            RetryDecision::DoNotRetry => return Err(classifier.normalize_terminal(error)),
            RetryDecision::RetryAfter(delay) => delay,
            RetryDecision::RejectServerDelay { requested, maximum } => {
                return Err(AttemptFailure::RetryDelayTooLong {
                    requested,
                    maximum,
                    source: Box::new(classifier.normalize_terminal(error)),
                });
            }
        };

        cancellable(sleeper.sleep(delay, cancellation.clone()), cancellation).await?;
        retry_index += 1;
    }
}

/// Local retry loop variant that permits `Rc`-backed classifiers, sleepers,
/// attempt closures, and futures.
pub async fn establish_with_retry_local<T, F, Fut>(
    policy: &RetryPolicy,
    classifier: &dyn LocalRetryClassifier,
    cancellation: &CancellationToken,
    attempt: F,
) -> Result<T, AttemptFailure>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AttemptFailure>>,
{
    establish_with_retry_and_local_sleeper(
        policy,
        classifier,
        &LocalDefaultRetrySleeper,
        cancellation,
        attempt,
    )
    .await
}

/// Local retry loop variant with an injected local sleeper.
pub async fn establish_with_retry_and_local_sleeper<T, F, Fut>(
    policy: &RetryPolicy,
    classifier: &dyn LocalRetryClassifier,
    sleeper: &dyn LocalRetrySleeper,
    cancellation: &CancellationToken,
    mut attempt: F,
) -> Result<T, AttemptFailure>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, AttemptFailure>>,
{
    let mut retry_index = 0;

    loop {
        cancellation
            .check()
            .map_err(|_| AttemptFailure::Cancelled)?;

        let attempt_result = cancellable(attempt(retry_index), cancellation).await;
        let error = match attempt_result {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };

        cancellation
            .check()
            .map_err(|_| AttemptFailure::Cancelled)?;
        if retry_index >= policy.max_retries {
            return Err(classifier.normalize_terminal(error));
        }

        let delay = match classifier.classify(&error, policy) {
            RetryDecision::DoNotRetry => return Err(classifier.normalize_terminal(error)),
            RetryDecision::RetryAfter(delay) => delay,
            RetryDecision::RejectServerDelay { requested, maximum } => {
                return Err(AttemptFailure::RetryDelayTooLong {
                    requested,
                    maximum,
                    source: Box::new(classifier.normalize_terminal(error)),
                });
            }
        };

        cancellable(sleeper.sleep(delay, cancellation.clone()), cancellation).await?;
        retry_index += 1;
    }
}

fn without_provider_body(failure: AttemptFailure) -> AttemptFailure {
    match failure {
        AttemptFailure::Http {
            attempt,
            status,
            headers,
            observed_at,
            ..
        } => AttemptFailure::http_at(
            attempt,
            status,
            headers,
            observed_at,
            "provider rejected request before streaming",
        ),
        AttemptFailure::RetryDelayTooLong {
            requested,
            maximum,
            source,
        } => AttemptFailure::RetryDelayTooLong {
            requested,
            maximum,
            source: Box::new(without_provider_body(*source)),
        },
        other => other,
    }
}

async fn cancellable<T>(
    future: impl Future<Output = Result<T, AttemptFailure>>,
    cancellation: &CancellationToken,
) -> Result<T, AttemptFailure> {
    let future = Box::pin(future);
    let cancelled = Box::pin(cancellation.cancelled());
    match select(future, cancelled).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(AttemptFailure::Cancelled),
    }
}
