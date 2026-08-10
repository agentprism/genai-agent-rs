//! Generic OAuth 2.0 Device Authorization Grant (RFC 8628) polling loop.
//!
//! Faithful port of pi-ai's `packages/ai/src/auth/oauth/device-code.ts`. The
//! caller supplies a `poll` closure that performs one token request and reports
//! an [`DevicePoll`] outcome; this function owns the interval / `slow_down` /
//! deadline bookkeeping.
//!
//! Cancellation: pi threads an `AbortSignal` through. In async Rust the idiomatic
//! equivalent is dropping the returned future (each `sleep`/poll is a cancellation
//! point), so no explicit signal is required here.

use std::future::Future;
use std::time::Duration;

use tokio::time::{sleep, Instant};

use crate::error::{Error, Result};

/// Minimum poll interval (device-code.ts:5).
pub const MINIMUM_INTERVAL_MS: u64 = 1000;
/// RFC 8628 3.2 default when the server omits `interval` (device-code.ts:7).
pub const DEFAULT_POLL_INTERVAL_SECONDS: f64 = 5.0;
/// RFC 8628 3.5 `slow_down` increment (device-code.ts:9).
pub const SLOW_DOWN_INTERVAL_INCREMENT_MS: u64 = 5000;

/// Result of a single poll attempt (device-code.ts:11-16).
#[derive(Debug)]
pub enum DevicePoll<T> {
    /// Keep waiting; authorization is still pending.
    Pending,
    /// Server asked us to back off. `interval_seconds`, when present, is the new
    /// required minimum (device-code.ts:78-86).
    SlowDown {
        /// New minimum interval, if the server provided one.
        interval_seconds: Option<f64>,
    },
    /// Terminal failure; the message is surfaced verbatim.
    Failed {
        /// Human-readable failure message.
        message: String,
    },
    /// Authorization completed with a value.
    Complete(T),
}

/// Timing options for [`poll_device_code`] (device-code.ts:18-24).
///
/// The `Default` (all `None` / `false`) matches pi's defaults: a 5-second poll
/// interval, no deadline, and no wait before the first poll.
#[derive(Debug, Clone, Default)]
pub struct DevicePollOptions {
    /// Initial poll interval in seconds (defaults to 5 when `None`).
    pub interval_seconds: Option<f64>,
    /// Overall deadline in seconds (unbounded when `None`).
    pub expires_in_seconds: Option<f64>,
    /// Whether to wait one interval before the first poll.
    pub wait_before_first_poll: bool,
}

/// Run the device-code polling loop until completion, failure, or timeout.
///
/// Mirrors `pollOAuthDeviceCodeFlow` (device-code.ts:46-98) including the
/// `max(MINIMUM_INTERVAL_MS, floor(interval*1000))` clamping, the `slow_down`
/// interval escalation, the `min(interval, remaining)` sleep, and the
/// timeout-vs-slow-down-timeout error distinction.
pub async fn poll_device_code<T, F, Fut>(options: DevicePollOptions, mut poll: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<DevicePoll<T>>>,
{
    let start = Instant::now();
    // `None` deadline == JS Number.POSITIVE_INFINITY (never times out).
    let deadline = options
        .expires_in_seconds
        .map(|s| start + Duration::from_secs_f64(s.max(0.0)));

    let mut interval_ms = clamp_interval_ms(
        options
            .interval_seconds
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECONDS),
    );
    let mut slow_down_responses = 0u32;

    if options.wait_before_first_poll {
        match remaining(deadline) {
            Some(rem) if rem > Duration::ZERO => sleep(min(interval_ms, rem)).await,
            Some(_) => {} // already past the deadline
            None => sleep(Duration::from_millis(interval_ms)).await,
        }
    }

    loop {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }

        match poll().await? {
            DevicePoll::Complete(value) => return Ok(value),
            DevicePoll::Failed { message } => return Err(Error::DeviceAuth(message)),
            DevicePoll::SlowDown { interval_seconds } => {
                slow_down_responses += 1;
                interval_ms = match interval_seconds {
                    Some(s) if s.is_finite() && s > 0.0 => clamp_interval_ms(s),
                    _ => std::cmp::max(
                        MINIMUM_INTERVAL_MS,
                        interval_ms + SLOW_DOWN_INTERVAL_INCREMENT_MS,
                    ),
                };
            }
            DevicePoll::Pending => {}
        }

        // Sleep for min(interval, remaining); break if the deadline has passed.
        let sleep_for = match deadline {
            Some(d) => {
                let rem = d.saturating_duration_since(Instant::now());
                if rem.is_zero() {
                    break;
                }
                min(interval_ms, rem)
            }
            None => Duration::from_millis(interval_ms),
        };
        sleep(sleep_for).await;
    }

    Err(if slow_down_responses > 0 {
        Error::DeviceSlowDownTimeout
    } else {
        Error::DeviceTimeout
    })
}

fn clamp_interval_ms(interval_seconds: f64) -> u64 {
    std::cmp::max(
        MINIMUM_INTERVAL_MS,
        (interval_seconds * 1000.0).floor() as u64,
    )
}

fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(Instant::now()))
}

fn min(interval_ms: u64, rem: Duration) -> Duration {
    std::cmp::min(Duration::from_millis(interval_ms), rem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[tokio::test(start_paused = true)]
    async fn completes_after_pending() {
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let opts = DevicePollOptions {
            interval_seconds: Some(1.0),
            expires_in_seconds: Some(60.0),
            wait_before_first_poll: false,
        };
        let out: i32 = poll_device_code(opts, move || {
            let calls = calls2.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Ok(DevicePoll::Pending)
                } else {
                    Ok(DevicePoll::Complete(7))
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(out, 7);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_when_never_authorized() {
        let opts = DevicePollOptions {
            interval_seconds: Some(1.0),
            expires_in_seconds: Some(3.0),
            wait_before_first_poll: false,
        };
        let err = poll_device_code::<i32, _, _>(opts, || async { Ok(DevicePoll::Pending) })
            .await
            .unwrap_err();
        assert!(matches!(err, Error::DeviceTimeout), "got {err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn slow_down_produces_slow_down_timeout() {
        let opts = DevicePollOptions {
            interval_seconds: Some(1.0),
            expires_in_seconds: Some(3.0),
            wait_before_first_poll: false,
        };
        let err = poll_device_code::<i32, _, _>(opts, || async {
            Ok(DevicePoll::SlowDown {
                interval_seconds: None,
            })
        })
        .await
        .unwrap_err();
        assert!(matches!(err, Error::DeviceSlowDownTimeout), "got {err:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn failed_is_terminal() {
        let opts = DevicePollOptions {
            interval_seconds: Some(1.0),
            expires_in_seconds: Some(60.0),
            wait_before_first_poll: false,
        };
        let err = poll_device_code::<i32, _, _>(opts, || async {
            Ok(DevicePoll::Failed {
                message: "nope".into(),
            })
        })
        .await
        .unwrap_err();
        match err {
            Error::DeviceAuth(m) => assert_eq!(m, "nope"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn interval_is_clamped_to_minimum() {
        assert_eq!(clamp_interval_ms(0.0), MINIMUM_INTERVAL_MS);
        assert_eq!(clamp_interval_ms(0.5), MINIMUM_INTERVAL_MS);
        assert_eq!(clamp_interval_ms(2.0), 2000);
    }
}
