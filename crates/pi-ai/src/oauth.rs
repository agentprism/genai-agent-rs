//! Portable OAuth helpers: PKCE/state, redirect/manual arbitration, and RFC
//! 8628 device-code polling from Architecture v2 part 2 §6.1–§6.4.

use crate::{AuthError, CancellationToken, SendBoxFuture, Timestamp};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures_util::future::{Either, select};
use sha2::{Digest, Sha256};
use std::fmt;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const DEFAULT_DEVICE_INTERVAL: Duration = Duration::from_secs(5);
const MINIMUM_DEVICE_INTERVAL: Duration = Duration::from_secs(1);
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);

/// Generated PKCE S256 verifier and challenge.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PkcePair {
    /// Base64url-encoded 32-byte verifier.
    pub verifier: String,
    /// Base64url-encoded SHA-256 challenge.
    pub challenge: String,
}

/// Generates a cryptographically random RFC 7636 S256 verifier/challenge pair.
pub fn generate_pkce() -> Result<PkcePair, AuthError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| {
        AuthError::new(
            "secure_random",
            format!("failed to generate PKCE verifier: {error}"),
        )
    })?;
    Ok(pkce_from_random_bytes(bytes))
}

/// Deterministically derives the pinned Pi PKCE representation from 32 bytes.
/// This is useful for captured fixtures while [`generate_pkce`] remains the
/// production entry point.
pub fn pkce_from_random_bytes(bytes: [u8; 32]) -> PkcePair {
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkcePair {
        verifier,
        challenge,
    }
}

/// Generates the 16-byte, lowercase-hex OAuth state used by pinned Pi's Codex
/// browser flow.
pub fn generate_oauth_state() -> Result<String, AuthError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        AuthError::new(
            "secure_random",
            format!("failed to generate OAuth state: {error}"),
        )
    })?;
    Ok(oauth_state_from_random_bytes(bytes))
}

/// Deterministically renders 16 state bytes as lowercase hexadecimal.
pub fn oauth_state_from_random_bytes(bytes: [u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut state = String::with_capacity(32);
    for byte in bytes {
        state.push(char::from(HEX[usize::from(byte >> 4)]));
        state.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    state
}

/// Validates returned OAuth state without an early-exit byte comparison.
pub fn validate_oauth_state(expected: &str, actual: &str) -> Result<(), AuthError> {
    let expected = expected.as_bytes();
    let actual = actual.as_bytes();
    let mut difference = expected.len() ^ actual.len();
    let length = expected.len().max(actual.len());
    for index in 0..length {
        let left = expected.get(index).copied().unwrap_or_default();
        let right = actual.get(index).copied().unwrap_or_default();
        difference |= usize::from(left ^ right);
    }
    if difference == 0 {
        Ok(())
    } else {
        Err(AuthError::StateMismatch)
    }
}

/// Parsed manual authorization code or redirect URL.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OAuthAuthorizationInput {
    /// Authorization code, when present.
    pub code: Option<String>,
    /// Returned state, when present.
    pub state: Option<String>,
}

/// Parses the redirect URL, `code#state`, query-string, or raw-code forms
/// accepted by pinned Pi's OpenAI Codex flow.
pub fn parse_oauth_authorization_input(input: &str) -> OAuthAuthorizationInput {
    let value = input.trim();
    if value.is_empty() {
        return OAuthAuthorizationInput::default();
    }

    if let Ok(url) = Url::parse(value) {
        return OAuthAuthorizationInput {
            code: url
                .query_pairs()
                .find_map(|(name, value)| (name == "code").then(|| value.into_owned())),
            state: url
                .query_pairs()
                .find_map(|(name, value)| (name == "state").then(|| value.into_owned())),
        };
    }

    if value.contains('#') {
        let mut parts = value.split('#');
        return OAuthAuthorizationInput {
            code: parts.next().map(Into::into),
            state: parts.next().map(Into::into),
        };
    }

    if value.contains("code=") {
        let mut code = None;
        let mut state = None;
        for (name, value) in
            url::form_urlencoded::parse(value.strip_prefix('?').unwrap_or(value).as_bytes())
        {
            match name.as_ref() {
                "code" if code.is_none() => code = Some(value.into_owned()),
                "state" if state.is_none() => state = Some(value.into_owned()),
                _ => {}
            }
        }
        return OAuthAuthorizationInput { code, state };
    }

    OAuthAuthorizationInput {
        code: Some(value.into()),
        state: None,
    }
}

/// Races two independently cancellable completion paths. The first valid
/// result wins; an invalid first result leaves the other path alive. Accepting
/// a winner cancels the losing child token.
pub async fn select_first_valid<T, LeftFactory, LeftFuture, RightFactory, RightFuture>(
    left: LeftFactory,
    right: RightFactory,
    cancellation: CancellationToken,
) -> Result<T, AuthError>
where
    LeftFactory: FnOnce(CancellationToken) -> LeftFuture,
    LeftFuture: Future<Output = Result<T, AuthError>>,
    RightFactory: FnOnce(CancellationToken) -> RightFuture,
    RightFuture: Future<Output = Result<T, AuthError>>,
{
    let left_cancellation = cancellation.child();
    let right_cancellation = cancellation.child();
    let race = Box::pin(select(
        Box::pin(left(left_cancellation.clone())),
        Box::pin(right(right_cancellation.clone())),
    ));
    let cancelled = Box::pin(cancellation.cancelled());

    match select(race, cancelled).await {
        Either::Right(((), _)) => {
            left_cancellation.cancel();
            right_cancellation.cancel();
            Err(AuthError::Cancelled)
        }
        Either::Left((Either::Left((left_result, right_future)), _)) => match left_result {
            Ok(value) => {
                right_cancellation.cancel();
                Ok(value)
            }
            Err(left_error) => {
                left_cancellation.cancel();
                match await_auth_candidate(right_future, &cancellation).await {
                    Ok(value) => Ok(value),
                    Err(AuthError::Cancelled) => Err(AuthError::Cancelled),
                    Err(right_error) => Err(AuthError::NoValidCompletion {
                        first: Box::new(left_error),
                        second: Box::new(right_error),
                    }),
                }
            }
        },
        Either::Left((Either::Right((right_result, left_future)), _)) => match right_result {
            Ok(value) => {
                left_cancellation.cancel();
                Ok(value)
            }
            Err(right_error) => {
                right_cancellation.cancel();
                match await_auth_candidate(left_future, &cancellation).await {
                    Ok(value) => Ok(value),
                    Err(AuthError::Cancelled) => Err(AuthError::Cancelled),
                    Err(left_error) => Err(AuthError::NoValidCompletion {
                        first: Box::new(left_error),
                        second: Box::new(right_error),
                    }),
                }
            }
        },
    }
}

async fn await_auth_candidate<T>(
    future: impl Future<Output = Result<T, AuthError>>,
    cancellation: &CancellationToken,
) -> Result<T, AuthError> {
    let future = Box::pin(future);
    let cancelled = Box::pin(cancellation.cancelled());
    match select(future, cancelled).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(AuthError::Cancelled),
    }
}

/// One response from an RFC 8628 token polling endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OAuthDeviceCodePollResult<T> {
    /// Authorization is still pending.
    Pending,
    /// Server requested slower polling. A positive server interval replaces
    /// the current interval; otherwise five seconds are added.
    SlowDown {
        /// New server-required interval.
        interval: Option<Duration>,
    },
    /// Provider returned a terminal failure.
    Failed {
        /// Sanitized provider message.
        message: String,
    },
    /// Authorization completed.
    Complete(T),
}

/// Object-safe polling callback used by [`poll_oauth_device_code_flow`].
pub trait OAuthDeviceCodePoll<T>: Send + 'static {
    /// Performs one token-endpoint poll.
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<T>, AuthError>>;
}

/// Local-executor counterpart to [`OAuthDeviceCodePoll`].
pub trait LocalOAuthDeviceCodePoll<T>: 'static {
    /// Performs one token-endpoint poll.
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> crate::LocalBoxFuture<'_, Result<OAuthDeviceCodePollResult<T>, AuthError>>;
}

/// Clock and cancellable timer used by device-code polling.
pub trait OAuthDeviceCodeRuntime: Send + Sync + 'static {
    /// Returns current Unix time in milliseconds.
    fn now(&self) -> Timestamp;

    /// Waits for a duration or cancellation.
    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AuthError>>;
}

/// Local-executor counterpart to [`OAuthDeviceCodeRuntime`].
pub trait LocalOAuthDeviceCodeRuntime: 'static {
    /// Returns current Unix time in milliseconds.
    fn now(&self) -> Timestamp;

    /// Waits for a duration or cancellation.
    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> crate::LocalBoxFuture<'_, Result<(), AuthError>>;
}

/// System clock plus executor-neutral `futures-timer` delay.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemOAuthDeviceCodeRuntime;

impl OAuthDeviceCodeRuntime for SystemOAuthDeviceCodeRuntime {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
    }

    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            let delay = Box::pin(futures_timer::Delay::new(duration));
            let cancelled = Box::pin(cancellation.cancelled());
            match select(delay, cancelled).await {
                Either::Left(((), _)) => Ok(()),
                Either::Right(((), _)) => Err(AuthError::Cancelled),
            }
        })
    }
}

impl LocalOAuthDeviceCodeRuntime for SystemOAuthDeviceCodeRuntime {
    fn now(&self) -> Timestamp {
        OAuthDeviceCodeRuntime::now(self)
    }

    fn sleep(
        &self,
        duration: Duration,
        cancellation: CancellationToken,
    ) -> crate::LocalBoxFuture<'_, Result<(), AuthError>> {
        Box::pin(async move {
            let delay = Box::pin(futures_timer::Delay::new(duration));
            let cancelled = Box::pin(cancellation.cancelled());
            match select(delay, cancelled).await {
                Either::Left(((), _)) => Ok(()),
                Either::Right(((), _)) => Err(AuthError::Cancelled),
            }
        })
    }
}

/// Inputs to the shared RFC 8628 polling state machine.
pub struct OAuthDeviceCodePollOptions<T> {
    /// Provider interval. Omission defaults to five seconds.
    pub interval: Option<Duration>,
    /// Optional authorization deadline relative to start.
    pub expires_in: Option<Duration>,
    /// Whether to wait one interval before the initial poll.
    pub wait_before_first_poll: bool,
    /// Token-endpoint polling implementation.
    pub poll: Box<dyn OAuthDeviceCodePoll<T>>,
    /// Whole-flow cancellation.
    pub cancellation: CancellationToken,
    /// Injectable clock/timer for portable hermetic tests.
    pub runtime: Arc<dyn OAuthDeviceCodeRuntime>,
}

impl<T> fmt::Debug for OAuthDeviceCodePollOptions<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthDeviceCodePollOptions")
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .field("wait_before_first_poll", &self.wait_before_first_poll)
            .finish_non_exhaustive()
    }
}

impl<T> OAuthDeviceCodePollOptions<T> {
    /// Creates options with Pi/RFC defaults and the system runtime.
    pub fn new(poll: Box<dyn OAuthDeviceCodePoll<T>>, cancellation: CancellationToken) -> Self {
        Self {
            interval: None,
            expires_in: None,
            wait_before_first_poll: false,
            poll,
            cancellation,
            runtime: Arc::new(SystemOAuthDeviceCodeRuntime),
        }
    }
}

/// Local-executor inputs to the RFC 8628 polling state machine.
pub struct LocalOAuthDeviceCodePollOptions<T> {
    /// Provider interval. Omission defaults to five seconds.
    pub interval: Option<Duration>,
    /// Optional authorization deadline relative to start.
    pub expires_in: Option<Duration>,
    /// Whether to wait one interval before the initial poll.
    pub wait_before_first_poll: bool,
    /// Local token-endpoint polling implementation.
    pub poll: Box<dyn LocalOAuthDeviceCodePoll<T>>,
    /// Whole-flow cancellation.
    pub cancellation: CancellationToken,
    /// Injectable local clock/timer.
    pub runtime: Rc<dyn LocalOAuthDeviceCodeRuntime>,
}

impl<T> fmt::Debug for LocalOAuthDeviceCodePollOptions<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalOAuthDeviceCodePollOptions")
            .field("interval", &self.interval)
            .field("expires_in", &self.expires_in)
            .field("wait_before_first_poll", &self.wait_before_first_poll)
            .finish_non_exhaustive()
    }
}

impl<T> LocalOAuthDeviceCodePollOptions<T> {
    /// Creates local options with Pi/RFC defaults and the system runtime.
    pub fn new(
        poll: Box<dyn LocalOAuthDeviceCodePoll<T>>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            interval: None,
            expires_in: None,
            wait_before_first_poll: false,
            poll,
            cancellation,
            runtime: Rc::new(SystemOAuthDeviceCodeRuntime),
        }
    }
}

/// Executes RFC 8628 polling with pinned Pi's timing and error semantics.
pub async fn poll_oauth_device_code_flow<T: 'static>(
    mut options: OAuthDeviceCodePollOptions<T>,
) -> Result<T, AuthError> {
    let started_at = options.runtime.now().unix_millis();
    let deadline = options.expires_in.map_or(i64::MAX, |duration| {
        started_at.saturating_add(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
    });
    let mut interval = options
        .interval
        .unwrap_or(DEFAULT_DEVICE_INTERVAL)
        .max(MINIMUM_DEVICE_INTERVAL);
    let mut slow_down_responses = 0_u32;

    if options.wait_before_first_poll {
        sleep_until_next_poll(
            options.runtime.as_ref(),
            interval,
            deadline,
            options.cancellation.clone(),
        )
        .await?;
    }

    while options.runtime.now().unix_millis() < deadline {
        options
            .cancellation
            .check()
            .map_err(|_| AuthError::Cancelled)?;
        let poll = options.poll.poll(options.cancellation.clone());
        let result = await_auth_candidate(poll, &options.cancellation).await?;
        match result {
            OAuthDeviceCodePollResult::Complete(value) => return Ok(value),
            OAuthDeviceCodePollResult::Failed { message } => {
                return Err(AuthError::new("device_code_failed", message));
            }
            OAuthDeviceCodePollResult::Pending => {}
            OAuthDeviceCodePollResult::SlowDown {
                interval: server_interval,
            } => {
                slow_down_responses = slow_down_responses.saturating_add(1);
                interval = server_interval
                    .filter(|interval| !interval.is_zero())
                    .map(|interval| interval.max(MINIMUM_DEVICE_INTERVAL))
                    .unwrap_or_else(|| interval.saturating_add(SLOW_DOWN_INCREMENT));
            }
        }

        if options.runtime.now().unix_millis() >= deadline {
            break;
        }
        sleep_until_next_poll(
            options.runtime.as_ref(),
            interval,
            deadline,
            options.cancellation.clone(),
        )
        .await?;
    }

    let message = if slow_down_responses > 0 {
        "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again."
    } else {
        "Device flow timed out"
    };
    Err(AuthError::new("device_code_timeout", message))
}

/// Local-executor RFC 8628 polling with the same timing and error semantics.
pub async fn poll_local_oauth_device_code_flow<T: 'static>(
    mut options: LocalOAuthDeviceCodePollOptions<T>,
) -> Result<T, AuthError> {
    let started_at = options.runtime.now().unix_millis();
    let deadline = options.expires_in.map_or(i64::MAX, |duration| {
        started_at.saturating_add(i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
    });
    let mut interval = options
        .interval
        .unwrap_or(DEFAULT_DEVICE_INTERVAL)
        .max(MINIMUM_DEVICE_INTERVAL);
    let mut slow_down_responses = 0_u32;

    if options.wait_before_first_poll {
        sleep_until_next_local_poll(
            options.runtime.as_ref(),
            interval,
            deadline,
            options.cancellation.clone(),
        )
        .await?;
    }

    while options.runtime.now().unix_millis() < deadline {
        options
            .cancellation
            .check()
            .map_err(|_| AuthError::Cancelled)?;
        let poll = options.poll.poll(options.cancellation.clone());
        let result = await_auth_candidate(poll, &options.cancellation).await?;
        match result {
            OAuthDeviceCodePollResult::Complete(value) => return Ok(value),
            OAuthDeviceCodePollResult::Failed { message } => {
                return Err(AuthError::new("device_code_failed", message));
            }
            OAuthDeviceCodePollResult::Pending => {}
            OAuthDeviceCodePollResult::SlowDown {
                interval: server_interval,
            } => {
                slow_down_responses = slow_down_responses.saturating_add(1);
                interval = server_interval
                    .filter(|interval| !interval.is_zero())
                    .map(|interval| interval.max(MINIMUM_DEVICE_INTERVAL))
                    .unwrap_or_else(|| interval.saturating_add(SLOW_DOWN_INCREMENT));
            }
        }

        if options.runtime.now().unix_millis() >= deadline {
            break;
        }
        sleep_until_next_local_poll(
            options.runtime.as_ref(),
            interval,
            deadline,
            options.cancellation.clone(),
        )
        .await?;
    }

    let message = if slow_down_responses > 0 {
        "Device flow timed out after one or more slow_down responses. This is often caused by clock drift in WSL or VM environments. Please sync or restart the VM clock and try again."
    } else {
        "Device flow timed out"
    };
    Err(AuthError::new("device_code_timeout", message))
}

async fn sleep_until_next_poll(
    runtime: &dyn OAuthDeviceCodeRuntime,
    interval: Duration,
    deadline: i64,
    cancellation: CancellationToken,
) -> Result<(), AuthError> {
    let remaining_millis = deadline.saturating_sub(runtime.now().unix_millis());
    if remaining_millis <= 0 {
        return Ok(());
    }
    let remaining = Duration::from_millis(u64::try_from(remaining_millis).unwrap_or(u64::MAX));
    runtime.sleep(interval.min(remaining), cancellation).await
}

async fn sleep_until_next_local_poll(
    runtime: &dyn LocalOAuthDeviceCodeRuntime,
    interval: Duration,
    deadline: i64,
    cancellation: CancellationToken,
) -> Result<(), AuthError> {
    let remaining_millis = deadline.saturating_sub(runtime.now().unix_millis());
    if remaining_millis <= 0 {
        return Ok(());
    }
    let remaining = Duration::from_millis(u64::try_from(remaining_millis).unwrap_or(u64::MAX));
    runtime.sleep(interval.min(remaining), cancellation).await
}
