use agentprism_ai::*;
use futures_executor::block_on;
use futures_util::future::ready;
use std::{cell::Cell, cell::RefCell, time::Duration};

fn assistant(reason: AssistantFinishReason, text: &str, error: Option<&str>) -> AssistantMessage {
    let scope = ReplayScope {
        provider: ProviderId::new("faux"),
        api: ApiId::new("faux"),
        requested_model: ModelId::new("faux-model"),
        produced_by_model: ModelId::new("faux-model"),
        protocol_revision: None,
    };
    AssistantMessage {
        id: MessageId::new("assistant-retry"),
        provider: scope.provider.clone(),
        api: scope.api.clone(),
        requested_model: scope.requested_model.clone(),
        response_model: None,
        response_id: None,
        deferred: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text {
                id: ContentBlockId::new("text"),
                text: text.into(),
            }]
        },
        replay: ReplayEnvelope::new(scope),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason,
            raw_provider_reason: None,
            error: error.map(|message| PublicError {
                code: "provider_error".into(),
                message: message.into(),
                retryable: false,
                provider_code: None,
                status: None,
                request_id: None,
            }),
        },
        timestamp: Timestamp::from_unix_millis(1),
    }
}

fn policy(max_retries: u32, base_delay: Duration) -> AssistantRetryPolicy {
    AssistantRetryPolicy {
        enabled: true,
        max_retries,
        base_delay,
    }
}

// Architecture v2 part 2 §2.4 correction/§10 parity manifest; Pi basis:
// packages/ai/test/retry.test.ts, "provider retry classification".
#[test]
fn assistant_retry_classification_matches_pi() {
    let retryable = [
        "An error occurred while processing your request. You can retry your request, or contact support.",
        r#"{"message":"The system encountered an unexpected error during processing. Try your request again."}"#,
        "ResourceExhausted: Worker local total request limit reached (288/48)",
        "The socket connection was closed unexpectedly.",
        "Error: exceeded request buffer limit while retrying upstream",
        "The pending stream has been canceled (caused by: getaddrinfo ENOTFOUND example.com)",
        "connect ENOTFOUND api.example.com",
        "EAI_AGAIN api.example.com",
        "getaddrinfo failed for api.example.com",
        "OpenAI Responses stream ended before a terminal response event",
        "overloaded_error",
        "524 status code (no body)",
    ];
    for message in retryable {
        assert!(
            is_retryable_assistant_error(&assistant(
                AssistantFinishReason::Error,
                "",
                Some(message),
            )),
            "expected retryable: {message}"
        );
    }

    for message in [
        "429 quota exceeded",
        "insufficient_quota",
        "billing failure",
    ] {
        assert!(!is_retryable_assistant_error(&assistant(
            AssistantFinishReason::Error,
            "",
            Some(message),
        )));
    }
    assert!(!is_retryable_assistant_error(&assistant(
        AssistantFinishReason::Stop,
        "not an error",
        None,
    )));
}

// Architecture v2 part 2 §2.4 correction/§10 parity manifest; Pi basis:
// packages/ai/test/retry.test.ts, immediate success, abort, quota, and disabled policy.
#[test]
fn assistant_retry_immediate_terminal_paths_match_pi() {
    block_on(async {
        for (response, configured) in [
            (
                assistant(AssistantFinishReason::Stop, "ok", None),
                Some(policy(3, Duration::ZERO)),
            ),
            (
                assistant(AssistantFinishReason::Aborted, "", None),
                Some(policy(3, Duration::ZERO)),
            ),
            (
                assistant(AssistantFinishReason::Error, "", Some("insufficient_quota")),
                Some(policy(3, Duration::ZERO)),
            ),
            (
                assistant(AssistantFinishReason::Error, "", Some("terminated")),
                Some(AssistantRetryPolicy {
                    enabled: false,
                    max_retries: 3,
                    base_delay: Duration::ZERO,
                }),
            ),
        ] {
            let calls = Cell::new(0);
            let events = RefCell::new(Vec::new());
            let output = retry_assistant_call_observed(
                || {
                    calls.set(calls.get() + 1);
                    ready(response.clone())
                },
                configured.as_ref(),
                &CancellationToken::new(),
                |event| {
                    events.borrow_mut().push(event);
                    ready(())
                },
            )
            .await;
            assert_eq!(output.finish.reason, response.finish.reason);
            assert_eq!(calls.get(), 1);
            assert!(events.borrow().is_empty());
        }
    });
}

// Architecture v2 part 2 §2.4 correction/§10 parity manifest; Pi basis:
// packages/ai/test/retry.test.ts, exhausted retries and callback counts.
#[test]
fn assistant_retry_exhaustion_and_callbacks_match_pi() {
    block_on(async {
        let calls = Cell::new(0);
        let events = RefCell::new(Vec::new());
        let configured = policy(3, Duration::ZERO);
        let output = retry_assistant_call_observed(
            || {
                calls.set(calls.get() + 1);
                ready(assistant(
                    AssistantFinishReason::Error,
                    "",
                    Some("terminated"),
                ))
            },
            Some(&configured),
            &CancellationToken::new(),
            |event| {
                events.borrow_mut().push(event);
                ready(())
            },
        )
        .await;
        assert_eq!(output.finish.reason, AssistantFinishReason::Error);
        assert_eq!(calls.get(), 4);
        assert_eq!(
            events
                .borrow()
                .iter()
                .filter(|event| matches!(event, AssistantRetryEvent::Scheduled { .. }))
                .count(),
            3
        );
        assert_eq!(
            events.borrow().last(),
            Some(&AssistantRetryEvent::Finished {
                success: false,
                attempt: 3,
                final_error: Some("terminated".into()),
            })
        );
    });
}

// Architecture v2 part 2 §2.4 correction/§10 parity manifest; Pi basis:
// packages/ai/test/retry.test.ts, successful and aborted retried calls.
#[test]
fn assistant_retry_success_and_retried_abort_callbacks_match_pi() {
    block_on(async {
        let calls = Cell::new(0);
        let events = RefCell::new(Vec::new());
        let configured = policy(3, Duration::ZERO);
        let recovered = retry_assistant_call_observed(
            || {
                let next = calls.get() + 1;
                calls.set(next);
                ready(if next < 3 {
                    assistant(AssistantFinishReason::Error, "", Some("terminated"))
                } else {
                    assistant(AssistantFinishReason::Stop, "recovered", None)
                })
            },
            Some(&configured),
            &CancellationToken::new(),
            |event| {
                events.borrow_mut().push(event);
                ready(())
            },
        )
        .await;
        assert_eq!(recovered.finish.reason, AssistantFinishReason::Stop);
        assert_eq!(calls.get(), 3);
        assert_eq!(
            events.borrow().last(),
            Some(&AssistantRetryEvent::Finished {
                success: true,
                attempt: 2,
                final_error: None,
            })
        );

        let calls = Cell::new(0);
        let events = RefCell::new(Vec::new());
        let aborted = retry_assistant_call_observed(
            || {
                let next = calls.get() + 1;
                calls.set(next);
                ready(if next == 1 {
                    assistant(AssistantFinishReason::Error, "", Some("terminated"))
                } else {
                    assistant(AssistantFinishReason::Aborted, "", None)
                })
            },
            Some(&configured),
            &CancellationToken::new(),
            |event| {
                events.borrow_mut().push(event);
                ready(())
            },
        )
        .await;
        assert_eq!(aborted.finish.reason, AssistantFinishReason::Aborted);
        assert_eq!(calls.get(), 2);
        assert_eq!(
            events.borrow().last(),
            Some(&AssistantRetryEvent::Finished {
                success: false,
                attempt: 1,
                final_error: None,
            })
        );
    });
}

// Architecture v2 part 2 §2.4 correction/§10 parity manifest; Pi basis:
// packages/ai/test/retry.test.ts, retry scheduling/start ordering and exponential delay.
#[test]
fn assistant_retry_attempt_start_follows_backoff_match_pi() {
    block_on(async {
        let calls = Cell::new(0);
        let events = RefCell::new(Vec::new());
        let configured = policy(3, Duration::ZERO);
        let _ = retry_assistant_call_observed(
            || {
                let next = calls.get() + 1;
                calls.set(next);
                events.borrow_mut().push(AssistantRetryEvent::Scheduled {
                    attempt: 100 + next,
                    max_attempts: 0,
                    delay: Duration::ZERO,
                    error_message: "produce".into(),
                });
                ready(if next < 3 {
                    assistant(AssistantFinishReason::Error, "", Some("terminated"))
                } else {
                    assistant(AssistantFinishReason::Stop, "recovered", None)
                })
            },
            Some(&configured),
            &CancellationToken::new(),
            |event| {
                events.borrow_mut().push(event);
                ready(())
            },
        )
        .await;
        let tags = events
            .borrow()
            .iter()
            .map(|event| match event {
                AssistantRetryEvent::Scheduled { attempt, .. } if *attempt >= 100 => "produce",
                AssistantRetryEvent::Scheduled { .. } => "retry",
                AssistantRetryEvent::AttemptStarted => "attempt-start",
                AssistantRetryEvent::Finished { .. } => "finished",
            })
            .collect::<Vec<_>>();
        assert_eq!(
            tags,
            [
                "produce",
                "retry",
                "attempt-start",
                "produce",
                "retry",
                "attempt-start",
                "produce",
                "finished",
            ]
        );
    });
}

// Architecture v2 part 2 §2.4 correction/§10 parity manifest; Pi basis:
// packages/ai/test/retry.test.ts, cancellation during retry backoff.
#[test]
fn assistant_retry_backoff_cancellation_returns_aborted_match_pi() {
    block_on(async {
        let calls = Cell::new(0);
        let events = RefCell::new(Vec::new());
        let cancellation = CancellationToken::new();
        let cancel_from_observer = cancellation.clone();
        let configured = policy(5, Duration::from_secs(10_000));
        let output = retry_assistant_call_observed(
            || {
                calls.set(calls.get() + 1);
                ready(assistant(
                    AssistantFinishReason::Error,
                    "",
                    Some("terminated"),
                ))
            },
            Some(&configured),
            &cancellation,
            |event| {
                if matches!(event, AssistantRetryEvent::Scheduled { .. }) {
                    cancel_from_observer.cancel();
                }
                events.borrow_mut().push(event);
                ready(())
            },
        )
        .await;
        assert_eq!(output.finish.reason, AssistantFinishReason::Aborted);
        assert!(output.finish.error.is_none());
        assert_eq!(calls.get(), 1);
        assert_eq!(
            events.borrow().last(),
            Some(&AssistantRetryEvent::Finished {
                success: false,
                attempt: 1,
                final_error: Some("terminated".into()),
            })
        );
    });
}

// Architecture v2 part 2 §2.4 correction/§10 parity manifest; Pi basis:
// packages/ai/src/utils/provider-retry.ts `abortableSleep`, whose explicit
// pre-sleep abort check precedes even a zero-duration timer.
#[test]
fn assistant_retry_pre_cancelled_zero_backoff_returns_aborted_match_pi() {
    block_on(async {
        let calls = Cell::new(0);
        let events = RefCell::new(Vec::new());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let configured = policy(1, Duration::ZERO);
        let output = retry_assistant_call_observed(
            || {
                calls.set(calls.get() + 1);
                ready(assistant(
                    AssistantFinishReason::Error,
                    "",
                    Some("terminated"),
                ))
            },
            Some(&configured),
            &cancellation,
            |event| {
                events.borrow_mut().push(event);
                ready(())
            },
        )
        .await;

        assert_eq!(output.finish.reason, AssistantFinishReason::Aborted);
        assert!(output.finish.error.is_none());
        assert_eq!(calls.get(), 1, "no retried call may start");
        assert_eq!(
            events.into_inner(),
            [
                AssistantRetryEvent::Scheduled {
                    attempt: 1,
                    max_attempts: 1,
                    delay: Duration::ZERO,
                    error_message: "terminated".into(),
                },
                AssistantRetryEvent::Finished {
                    success: false,
                    attempt: 1,
                    final_error: Some("terminated".into()),
                },
            ]
        );
    });
}
