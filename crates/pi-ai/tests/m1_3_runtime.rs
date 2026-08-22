use futures_core::Stream;
use futures_util::task::noop_waker;
use pi_ai::*;
use serde_json::{json, value::RawValue};
use static_assertions::{assert_impl_all, assert_not_impl_any};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll, Wake, Waker};

assert_impl_all!(ScriptedRuntime: ModelRuntime, LocalModelRuntime, Send, Sync);
assert_impl_all!(CancellationToken: Clone, Send, Sync);

struct AnthropicMessages;

impl ApiFamily for AnthropicMessages {
    const API_ID: &'static str = "anthropic-messages";

    type Compat = ();
    type ModelConfig = ();
    type FullOptions = ();
    type OptionsPatch = u32;
    type WireRequest = Vec<u8>;

    fn resolve_compat(
        _effective_base_url: &url::Url,
        _model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(())
    }

    fn lower_simple(
        _context: SimpleLoweringContext<'_, Self>,
        _simple: &SimpleGenerationOptions,
        _patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        Ok(())
    }

    fn encode(
        _context: EncodeContext<'_, Self>,
        _options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        Ok(Vec::new())
    }
}

fn request(provider: &str, model: &str) -> ModelRequest {
    ModelRequest {
        model: ModelRef::new(provider, model),
        context: Context::new(Some("system".into())),
        options: SimpleGenerationOptions::default(),
    }
}

fn ready<T>(future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    let waker = noop_waker();
    let mut context = TaskContext::from_waker(&waker);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("fixture future unexpectedly pending"),
    }
}

fn next_event<S>(stream: &mut S) -> Option<AssistantEvent>
where
    S: Stream<Item = AssistantEvent> + Unpin,
{
    let waker = noop_waker();
    let mut context = TaskContext::from_waker(&waker);
    match Pin::new(stream).poll_next(&mut context) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("fixture stream unexpectedly pending"),
    }
}

fn run(response: ScriptedResponse, request: ModelRequest) -> Vec<AssistantEvent> {
    let runtime = ScriptedRuntime::builder().response(response).build();
    let mut stream = ready(ModelRuntime::stream(
        &runtime,
        request,
        CancellationToken::new(),
    ))
    .unwrap();
    let mut events = Vec::new();
    while let Some(event) = next_event(&mut stream) {
        events.push(event);
    }
    events
}

fn assemble(events: &[AssistantEvent]) -> AssistantMessage {
    let mut assembler = AssistantAssembler::new();
    for event in events {
        assembler.apply(event).unwrap();
    }
    assembler
        .snapshot()
        .terminal_message
        .expect("script must terminate")
        .clone()
}

fn finish(reason: AssistantFinishReason) -> AssistantFinish {
    AssistantFinish {
        reason,
        raw_provider_reason: None,
        error: None,
    }
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_tokens: input,
        output_tokens: output,
        reasoning_tokens: Some(output / 2),
        cache_read_tokens: Some(3),
        cache_write_tokens: Some(4),
        source: UsageSource::ProviderReported,
    }
}

fn failure() -> PublicError {
    PublicError {
        code: "transport".into(),
        message: "connection closed".into(),
        retryable: true,
        provider_code: Some("upstream_reset".into()),
        status: Some(503),
        request_id: Some("request-7".into()),
    }
}

fn start(provider: &str, api: &str, model: &str) -> AssistantEvent {
    AssistantEvent::MessageStarted {
        message_id: MessageId::new("message-1"),
        provider: ProviderId::new(provider),
        api: ApiId::new(api),
        model: ModelId::new(model),
    }
}

#[test]
fn model_runtime_is_a_narrow_arc_capability_and_local_runtime_accepts_rc_state() {
    let runtime: Arc<dyn ModelRuntime> = Arc::new(
        ScriptedRuntime::builder()
            .response(text_response("hello"))
            .build(),
    );
    let mut stream = ready(runtime.stream(request("fake", "model"), CancellationToken::new()))
        .expect("scripted stream starts");
    assert!(matches!(
        next_event(&mut stream),
        Some(AssistantEvent::MessageStarted { .. })
    ));

    struct RcRuntime(Rc<()>);
    impl LocalModelRuntime for RcRuntime {
        fn stream(
            &self,
            _request: ModelRequest,
            _cancellation: CancellationToken,
        ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, RequestStartError>> {
            let state = Rc::clone(&self.0);
            Box::pin(async move {
                drop(state);
                Err(RequestStartError::new(
                    RequestStartErrorKind::RuntimeUnavailable,
                    "local fixture",
                ))
            })
        }
    }
    assert_not_impl_any!(RcRuntime: Send, Sync);
    let local: Box<dyn LocalModelRuntime> = Box::new(RcRuntime(Rc::new(())));
    assert!(ready(local.stream(request("local", "model"), CancellationToken::new())).is_err());
}

#[test]
fn cancellation_child_propagates_from_parent() {
    let parent = CancellationToken::new();
    let child = parent.child();
    let grandchild = child.child();
    child.cancel();
    assert!(child.is_cancelled());
    assert!(grandchild.is_cancelled());
    assert!(!parent.is_cancelled());

    let sibling = parent.child();
    parent.cancel();
    assert!(sibling.is_cancelled());
    assert!(parent.child().is_cancelled());
}

struct WakeCounter(AtomicUsize);

impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn cancellation_wakes_waiter() {
    let token = CancellationToken::new();
    let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    let mut context = TaskContext::from_waker(&waker);
    let mut cancelled = Box::pin(token.cancelled());

    assert!(cancelled.as_mut().poll(&mut context).is_pending());
    token.cancel();
    assert_eq!(counter.0.load(Ordering::SeqCst), 1);
    assert!(cancelled.as_mut().poll(&mut context).is_ready());
}

#[test]
fn cancellation_before_scripted_stream_yields_cancelled_record() {
    // Pi basis: packages/ai/test/abort.test.ts `testImmediateAbort`; Part 2 §2.1.
    let runtime = ScriptedRuntime::builder()
        .response(text_response("must not be emitted"))
        .build();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let mut stream = ready(ModelRuntime::stream(
        &runtime,
        request("fake", "model"),
        cancellation,
    ))
    .unwrap();

    assert!(matches!(
        next_event(&mut stream),
        Some(AssistantEvent::MessageStarted { .. })
    ));
    let terminal = next_event(&mut stream).unwrap();
    let AssistantEvent::Cancelled { message } = terminal else {
        panic!("expected cancelled terminal")
    };
    assert!(message.content.is_empty());
    assert_eq!(message.finish.reason, AssistantFinishReason::Aborted);
    assert_eq!(message.finish.error.unwrap().code, "cancelled");
    assert!(next_event(&mut stream).is_none());
}

#[test]
fn cancellation_during_scripted_stream_yields_cancelled_record() {
    // Pi basis: packages/ai/test/abort.test.ts `testAbortSignal`; Part 2 §2.1.
    let runtime = ScriptedRuntime::builder()
        .response(text_response("partial output"))
        .build();
    let cancellation = CancellationToken::new();
    let mut stream = ready(ModelRuntime::stream(
        &runtime,
        request("fake", "model"),
        cancellation.clone(),
    ))
    .unwrap();
    assert!(matches!(
        next_event(&mut stream),
        Some(AssistantEvent::MessageStarted { .. })
    ));
    assert!(matches!(
        next_event(&mut stream),
        Some(AssistantEvent::ContentBlockStarted { .. })
    ));
    assert!(matches!(
        next_event(&mut stream),
        Some(AssistantEvent::TextDelta { .. })
    ));
    cancellation.cancel();

    let AssistantEvent::Cancelled { message } = next_event(&mut stream).unwrap() else {
        panic!("expected cancelled terminal")
    };
    assert_eq!(message.finish.reason, AssistantFinishReason::Aborted);
    assert_eq!(
        message.content,
        vec![ContentBlock::Text {
            id: ContentBlockId::new("scripted-block-0"),
            text: "partial output".into(),
        }]
    );
}

#[test]
fn stream_start_precedes_content() {
    // §10.1; Pi basis: packages/ai/src/types.ts and provider stream implementations.
    let events = run(text_response("hello"), request("fake", "model"));
    assert!(matches!(events[0], AssistantEvent::MessageStarted { .. }));
    let first_content = events
        .iter()
        .position(|event| {
            matches!(
                event,
                AssistantEvent::ContentBlockStarted { .. }
                    | AssistantEvent::TextDelta { .. }
                    | AssistantEvent::ThinkingDelta { .. }
                    | AssistantEvent::ToolCallMetadata { .. }
                    | AssistantEvent::ToolArgumentsDelta { .. }
            )
        })
        .unwrap();
    assert!(first_content > 0);
}

#[test]
fn stream_exactly_one_terminal() {
    // §10.1; Pi basis: packages/ai/src/types.ts: AssistantMessageEvent.
    for response in [
        text_response("ok"),
        ScriptedResponse::failure(failure()),
        ScriptedResponse::cancellation(CancellationReason::new("aborted")),
    ] {
        let events = run(response, request("fake", "model"));
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
    }
}

#[test]
fn stream_no_event_after_terminal() {
    // §10.1; Pi basis: packages/ai/src/utils/event-stream.ts.
    let runtime = ScriptedRuntime::builder()
        .response(text_response("ok"))
        .build();
    let mut stream = ready(ModelRuntime::stream(
        &runtime,
        request("fake", "model"),
        CancellationToken::new(),
    ))
    .unwrap();
    while !next_event(&mut stream).unwrap().is_terminal() {}
    assert!(next_event(&mut stream).is_none());
    assert!(next_event(&mut stream).is_none());
}

#[test]
fn stream_failure_is_terminal_message() {
    // §10.1; Pi basis: anthropic-messages.ts and openai-completions.ts catch paths.
    let events = run(
        text_response("partial").failing(failure()),
        request("fake", "model"),
    );
    let message = assemble(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(message.finish.error.unwrap().code, "transport");
    assert_eq!(message.content.len(), 1);
}

#[test]
fn stream_cancellation_is_terminal_message() {
    // §10.1; Pi basis: packages/ai/test/abort.test.ts.
    let events = run(
        text_response("partial").cancelling(CancellationReason::new("Request was aborted")),
        request("fake", "model"),
    );
    let message = assemble(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Aborted);
    assert_eq!(message.finish.error.unwrap().code, "cancelled");
    assert_eq!(message.content.len(), 1);
}

#[test]
fn stream_partial_identity_is_stable() {
    // §10.1; Rust strengthening of Pi's shared mutable partial identity.
    let events = run(text_response("hello"), request("fake", "model"));
    let started_id = match &events[0] {
        AssistantEvent::MessageStarted { message_id, .. } => message_id,
        _ => unreachable!(),
    };
    assert_eq!(assemble(&events).id, *started_id);
}

#[test]
fn stream_response_id_is_preserved() {
    // §10.1; Pi basis: types.ts and OpenAI response stream implementations.
    let events = run(
        text_response("hello").with_response_metadata(Some("response-1".into()), None),
        request("fake", "model"),
    );
    let restored: AssistantMessage =
        serde_json::from_slice(&serde_json::to_vec(&assemble(&events)).unwrap()).unwrap();
    assert_eq!(restored.response_id.as_deref(), Some("response-1"));
}

#[test]
fn stream_response_model_is_preserved() {
    // §10.1; Pi basis: types.ts and openai-completions.ts.
    let events = run(
        text_response("hello").with_response_metadata(None, Some(ModelId::new("concrete-model"))),
        request("fake", "requested-model"),
    );
    assert_eq!(
        assemble(&events).response_model,
        Some(ModelId::new("concrete-model"))
    );
}

#[test]
fn stream_usage_is_cumulative() {
    // §10.1; Pi basis: provider usage handlers overwrite cumulative values.
    let events = run(
        text_response("hello")
            .with_usage(usage(2, 3))
            .with_usage(usage(7, 11)),
        request("fake", "model"),
    );
    assert_eq!(assemble(&events).usage, usage(7, 11));
}

#[test]
fn stream_tool_json_scratch_not_persisted() {
    // §10.1; Pi basis: anthropic/openai tool-call finalization paths.
    let events = run(
        tool_call_response("read_file", json!({ "path": "README.md" })),
        request("fake", "model"),
    );
    let bytes = serde_json::to_vec(&assemble(&events)).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(!text.contains("partialJson"));
    assert!(!text.contains("arguments_scratch"));
}

#[test]
fn stream_binary_scratch_not_persisted() {
    // §10.1; Pi basis: bedrock-converse-stream.ts redactedChunks finalization.
    let item = ScriptedReplayItem {
        id: ReplayItemId::new("redacted-0"),
        ordinal: 0,
        target: ScriptedReplayTarget::ContentBlock(0),
        kind: ReplayKind::new("bedrock.converse.redacted-reasoning"),
        applicability: ReplayApplicability::ExactProviderApiModel,
        payload: OpaquePayload::Bytes(vec![1, 2, 3, 4]),
    };
    let events = run(
        text_response("[Reasoning redacted]")
            .with_api("bedrock-converse-stream")
            .with_replay_item(item),
        request("amazon-bedrock", "model"),
    );
    let text = serde_json::to_string(&assemble(&events)).unwrap();
    assert!(text.contains("bytes_base64"));
    assert!(!text.contains("redactedChunks"));
}

#[test]
fn stream_missing_provider_terminal_fails() {
    // §10.1; Pi basis: OpenAI Responses and Anthropic premature-end checks.
    let response = ScriptedResponse::events([start("fake", "scripted", "model")]);
    let events = run(response, request("fake", "model"));
    let message = assemble(&events);
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(
        message.finish.error.unwrap().code,
        "missing_provider_terminal"
    );
}

#[test]
fn stream_error_sanitizes_secrets() {
    // §10.1; Pi basis: packages/ai/src/utils/error-body.ts and
    // packages/ai/test/provider-error-body-regression.test.ts preserve useful
    // provider-body reasons. Part 2 §10.1 adds native public-error hardening.
    let mut request = request("fake", "model");
    request.options.headers.insert(
        "Authorization".into(),
        Some("Bearer request-header-secret".into()),
    );
    request
        .options
        .headers
        .insert("x-api-key".into(), Some("api-header-secret".into()));
    let response = ScriptedResponse::failure(PublicError {
        code: "provider_rejected".into(),
        message: concat!(
            "403 blocked by gateway WAF; ",
            "Authorization: Bearer request-header-secret; ",
            "x-api-key=api-header-secret; ",
            r#"body={"access_token":"body-secret","detail":"policy denied"}"#,
        )
        .into(),
        retryable: false,
        provider_code: Some("permission_denied".into()),
        status: Some(403),
        request_id: Some("request-403".into()),
    });

    let error = assemble(&run(response, request)).finish.error.unwrap();
    assert!(error.message.contains("403 blocked by gateway WAF"));
    assert!(error.message.contains("policy denied"));
    assert!(error.message.contains("[REDACTED]"));
    for secret in ["request-header-secret", "api-header-secret", "body-secret"] {
        assert!(!error.message.contains(secret));
    }
}

#[test]
fn stream_unicode_matches_pi() {
    // §10.1; Pi basis: packages/ai/src/utils/sanitize-unicode.ts and
    // packages/ai/test/unicode-surrogate.test.ts. Pi removes lone UTF-16
    // surrogates and preserves valid pairs before request encoding.
    let utf16 = [
        b'A' as u16,
        0xD83D,
        b'B' as u16,
        0xDC00,
        b'C' as u16,
        0xD83D,
        0xDE48,
        b'D' as u16,
        0xD83D,
    ];
    let sanitized = sanitize_utf16_surrogates(&utf16);
    let message = assemble(&run(text_response(sanitized), request("fake", "model")));

    assert_eq!(
        message.content,
        vec![ContentBlock::Text {
            id: ContentBlockId::new("scripted-block-0"),
            text: "ABC🙈D".into(),
        }]
    );
}

#[test]
fn anthropic_signature_fragments_append_in_order() {
    // §10.2; Pi basis: anthropic-messages.ts signature_delta handling.
    let block = ContentBlockId::new("thinking-0");
    let item = ReplayItemId::new("signature-0");
    let response = ScriptedResponse::completed_events(
        [
            start("anthropic", "anthropic-messages", "claude-test"),
            AssistantEvent::ContentBlockStarted {
                block_id: block.clone(),
                content_index: 0,
                kind: ContentBlockKind::Thinking,
            },
            AssistantEvent::ReplayItemStarted {
                item_id: item.clone(),
                ordinal: 0,
                target: ReplayTarget::ContentBlock(block.clone()),
                kind: ReplayKind::new("anthropic.messages.thinking-signature"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ThinkingDelta {
                block_id: block.clone(),
                delta: "thinking".into(),
            },
            AssistantEvent::ReplayData {
                item_id: item.clone(),
                operation: ReplayDataOperation::AppendUtf8("first".into()),
            },
            AssistantEvent::ReplayData {
                item_id: item.clone(),
                operation: ReplayDataOperation::AppendUtf8("-second".into()),
            },
            AssistantEvent::ReplayItemFinished {
                item_id: item.clone(),
            },
            AssistantEvent::ContentBlockFinished { block_id: block },
        ],
        finish(AssistantFinishReason::Stop),
    )
    .unwrap();
    let message = assemble(&run(response, request("anthropic", "claude-test")));
    assert_eq!(
        message.content,
        vec![ContentBlock::Thinking {
            id: ContentBlockId::new("thinking-0"),
            text: "thinking".into(),
            redacted: false,
            replay_item: Some(ReplayItemId::new("signature-0")),
        }]
    );
    assert_eq!(message.replay.items[0].as_utf8(), Some("first-second"));
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-0"))
    );
}

#[test]
fn scripted_anthropic_redacted_event_sequence_retains_exact_data() {
    // Architecture §1.4; Pi basis: anthropic-messages.ts redacted_thinking
    // content_block_start and turn-two conversion.
    let block = ContentBlockId::new("thinking-redacted");
    let item = ReplayItemId::new("redacted-signature");
    let response = ScriptedResponse::completed_events(
        [
            start("anthropic", "anthropic-messages", "claude-test"),
            AssistantEvent::ContentBlockStarted {
                block_id: block.clone(),
                content_index: 0,
                kind: ContentBlockKind::Thinking,
            },
            AssistantEvent::ThinkingDelta {
                block_id: block.clone(),
                delta: "[Reasoning redacted]".into(),
            },
            AssistantEvent::ReplayItemStarted {
                item_id: item.clone(),
                ordinal: 0,
                target: ReplayTarget::ContentBlock(block.clone()),
                kind: ReplayKind::new("anthropic.messages.redacted-thinking"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ReplayData {
                item_id: item.clone(),
                operation: ReplayDataOperation::ReplaceUtf8("<opaque-data>".into()),
            },
            AssistantEvent::ReplayItemFinished {
                item_id: item.clone(),
            },
            AssistantEvent::ContentBlockFinished {
                block_id: block.clone(),
            },
        ],
        finish(AssistantFinishReason::Stop),
    )
    .unwrap();

    let message = assemble(&run(response, request("anthropic", "claude-test")));
    assert_eq!(
        message.content,
        vec![ContentBlock::Thinking {
            id: block.clone(),
            text: "[Reasoning redacted]".into(),
            redacted: true,
            replay_item: Some(item.clone()),
        }]
    );
    assert_eq!(message.replay.items.len(), 1);
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(block)
    );
    assert_eq!(message.replay.items[0].as_utf8(), Some("<opaque-data>"));
}

#[test]
fn openai_chat_reasoning_details_preserve_array_order() {
    // §10.2; Pi basis: openai-completions.ts reasoning_details handling.
    let block = ContentBlockId::new("thinking-0");
    let mut events = vec![
        start("openrouter", "openai-completions", "reasoning-model"),
        AssistantEvent::ContentBlockStarted {
            block_id: block.clone(),
            content_index: 0,
            kind: ContentBlockKind::Thinking,
        },
        AssistantEvent::ThinkingDelta {
            block_id: block.clone(),
            delta: "visible reasoning".into(),
        },
    ];
    for (ordinal, id, payload) in [
        (
            0,
            "rs-1",
            r#"{"type":"reasoning.encrypted","id":"rs_1","data":"opaque-A"}"#,
        ),
        (
            1,
            "rs-2",
            r#"{"type":"reasoning.summary","id":"rs_2","summary":"..."}"#,
        ),
    ] {
        let item_id = ReplayItemId::new(id);
        events.extend([
            AssistantEvent::ReplayItemStarted {
                item_id: item_id.clone(),
                ordinal,
                target: ReplayTarget::ContentBlock(block.clone()),
                kind: ReplayKind::new("openai.chat.reasoning-detail"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ReplayData {
                item_id: item_id.clone(),
                operation: ReplayDataOperation::ReplaceJsonBytes(payload.as_bytes().to_vec()),
            },
            AssistantEvent::ReplayItemFinished { item_id },
        ]);
    }
    events.push(AssistantEvent::ContentBlockFinished { block_id: block });
    let response =
        ScriptedResponse::completed_events(events, finish(AssistantFinishReason::Stop)).unwrap();
    let message = assemble(&run(response, request("openrouter", "reasoning-model")));
    assert_eq!(
        message.content,
        vec![ContentBlock::Thinking {
            id: ContentBlockId::new("thinking-0"),
            text: "visible reasoning".into(),
            redacted: false,
            replay_item: Some(ReplayItemId::new("rs-1")),
        }]
    );
    assert_eq!(message.replay.items[0].id.as_str(), "rs-1");
    assert_eq!(message.replay.items[1].id.as_str(), "rs-2");
    assert_eq!(
        message.replay.items[0].json_bytes().unwrap(),
        r#"{"type":"reasoning.encrypted","id":"rs_1","data":"opaque-A"}"#.as_bytes()
    );
}

#[test]
fn responses_output_items_preserve_global_order() {
    // §10.2; Pi basis: openai-responses-shared.ts output_index slots.
    let thinking_block = ContentBlockId::new("thinking-0");
    let text_block = ContentBlockId::new("text-1");
    let tool_block = ContentBlockId::new("tool-2");
    let call_id = ToolCallId::new("call_123");
    let reasoning_item = ReplayItemId::new("replay-0");
    let message_item = ReplayItemId::new("replay-1");
    let function_item = ReplayItemId::new("replay-2");
    let reasoning_json = br#"{"id":"rs_123","type":"reasoning","encrypted_content":"opaque"}"#;
    let message_json = br#"{"id":"msg_123","phase":"final_answer","block_id":"text-1"}"#;
    let function_json =
        br#"{"call_id":"call_123","item_id":"fc_456","namespace":null,"type":"function_call"}"#;
    let response = ScriptedResponse::completed_events(
        [
            start("openai", "openai-responses", "gpt-test"),
            AssistantEvent::ReplayItemStarted {
                item_id: reasoning_item.clone(),
                ordinal: 0,
                target: ReplayTarget::ProviderOutputItem { output_index: 0 },
                kind: ReplayKind::new("openai.responses.reasoning-item"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ContentBlockStarted {
                block_id: thinking_block.clone(),
                content_index: 0,
                kind: ContentBlockKind::Thinking,
            },
            AssistantEvent::ThinkingDelta {
                block_id: thinking_block.clone(),
                delta: "Inspecting the request...".into(),
            },
            AssistantEvent::ReplayData {
                item_id: reasoning_item.clone(),
                operation: ReplayDataOperation::ReplaceJsonBytes(reasoning_json.to_vec()),
            },
            AssistantEvent::ReplayItemFinished {
                item_id: reasoning_item,
            },
            AssistantEvent::ContentBlockFinished {
                block_id: thinking_block.clone(),
            },
            AssistantEvent::ReplayItemStarted {
                item_id: message_item.clone(),
                ordinal: 1,
                target: ReplayTarget::ProviderOutputItem { output_index: 1 },
                kind: ReplayKind::new("openai.responses.message-identity"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ContentBlockStarted {
                block_id: text_block.clone(),
                content_index: 1,
                kind: ContentBlockKind::Text,
            },
            AssistantEvent::TextDelta {
                block_id: text_block.clone(),
                delta: "I found the issue.".into(),
            },
            AssistantEvent::ReplayData {
                item_id: message_item.clone(),
                operation: ReplayDataOperation::ReplaceJsonBytes(message_json.to_vec()),
            },
            AssistantEvent::ReplayItemFinished {
                item_id: message_item,
            },
            AssistantEvent::ContentBlockFinished {
                block_id: text_block.clone(),
            },
            AssistantEvent::ReplayItemStarted {
                item_id: function_item.clone(),
                ordinal: 2,
                target: ReplayTarget::ProviderOutputItem { output_index: 2 },
                kind: ReplayKind::new("openai.responses.function-call-identity"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ContentBlockStarted {
                block_id: tool_block.clone(),
                content_index: 2,
                kind: ContentBlockKind::ToolCall,
            },
            AssistantEvent::ToolCallMetadata {
                block_id: tool_block.clone(),
                call_id: call_id.clone(),
                name: Some("read_file".into()),
            },
            AssistantEvent::ToolArgumentsDelta {
                block_id: tool_block.clone(),
                delta: r#"{"path":"README.md"}"#.into(),
            },
            AssistantEvent::ReplayData {
                item_id: function_item.clone(),
                operation: ReplayDataOperation::ReplaceJsonBytes(function_json.to_vec()),
            },
            AssistantEvent::ReplayItemFinished {
                item_id: function_item,
            },
            AssistantEvent::ContentBlockFinished {
                block_id: tool_block.clone(),
            },
        ],
        finish(AssistantFinishReason::ToolUse),
    )
    .unwrap();
    let message = assemble(&run(response, request("openai", "gpt-test")));
    assert_eq!(
        message.content,
        vec![
            ContentBlock::Thinking {
                id: thinking_block,
                text: "Inspecting the request...".into(),
                redacted: false,
                replay_item: None,
            },
            ContentBlock::Text {
                id: text_block,
                text: "I found the issue.".into(),
            },
            ContentBlock::ToolCall {
                id: tool_block,
                call: ToolCall {
                    id: call_id,
                    name: "read_file".into(),
                    arguments: json!({ "path": "README.md" }),
                },
            },
        ]
    );
    assert_eq!(
        message
            .replay
            .items
            .iter()
            .map(|item| item.ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(
        message.replay.items[0].json_bytes().unwrap(),
        reasoning_json
    );
    assert_eq!(message.replay.items[1].json_bytes().unwrap(), message_json);
    assert_eq!(message.replay.items[2].json_bytes().unwrap(), function_json);
}

#[test]
fn bedrock_redacted_chunks_concatenate_as_bytes() {
    // §10.2; Pi basis: bedrock-converse-stream.ts redactedContent buffering.
    let block = ContentBlockId::new("thinking-0");
    let item = ReplayItemId::new("redacted-0");
    let response = ScriptedResponse::completed_events(
        [
            start("amazon-bedrock", "bedrock-converse-stream", "bedrock-model"),
            AssistantEvent::ContentBlockStarted {
                block_id: block.clone(),
                content_index: 0,
                kind: ContentBlockKind::Thinking,
            },
            AssistantEvent::ThinkingDelta {
                block_id: block.clone(),
                delta: "[Reasoning redacted]".into(),
            },
            AssistantEvent::ReplayItemStarted {
                item_id: item.clone(),
                ordinal: 0,
                target: ReplayTarget::ContentBlock(block.clone()),
                kind: ReplayKind::new("bedrock.converse.redacted-reasoning"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ReplayData {
                item_id: item.clone(),
                operation: ReplayDataOperation::AppendBytes(vec![1, 2]),
            },
            AssistantEvent::ReplayData {
                item_id: item.clone(),
                operation: ReplayDataOperation::AppendBytes(vec![3, 4]),
            },
            AssistantEvent::ReplayItemFinished {
                item_id: item.clone(),
            },
            AssistantEvent::ContentBlockFinished { block_id: block },
        ],
        finish(AssistantFinishReason::Stop),
    )
    .unwrap();
    let message = assemble(&run(response, request("amazon-bedrock", "bedrock-model")));
    assert_eq!(
        message.content,
        vec![ContentBlock::Thinking {
            id: ContentBlockId::new("thinking-0"),
            text: "[Reasoning redacted]".into(),
            redacted: true,
            replay_item: Some(ReplayItemId::new("redacted-0")),
        }]
    );
    assert_eq!(
        message.replay.items[0].as_bytes(),
        Some([1, 2, 3, 4].as_slice())
    );
}

#[test]
fn google_signature_never_moves_between_parts() {
    // §10.2; Pi basis: google-shared.ts thoughtSignature target rules.
    let text_block = ContentBlockId::new("text-0");
    let tool_block = ContentBlockId::new("tool-1");
    let call_id = ToolCallId::new("call-1");
    let text_item = ReplayItemId::new("signature-text");
    let tool_item = ReplayItemId::new("signature-tool");
    let response = ScriptedResponse::completed_events(
        [
            start("google", "google-generative-ai", "gemini-test"),
            AssistantEvent::ContentBlockStarted {
                block_id: text_block.clone(),
                content_index: 0,
                kind: ContentBlockKind::Text,
            },
            AssistantEvent::ReplayItemStarted {
                item_id: text_item.clone(),
                ordinal: 0,
                target: ReplayTarget::ContentBlock(text_block.clone()),
                kind: ReplayKind::new("google.genai.thought-signature"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ReplayData {
                item_id: text_item.clone(),
                operation: ReplayDataOperation::ReplaceUtf8("dGV4dA==".into()),
            },
            AssistantEvent::ReplayItemFinished { item_id: text_item },
            AssistantEvent::ContentBlockFinished {
                block_id: text_block.clone(),
            },
            AssistantEvent::ContentBlockStarted {
                block_id: tool_block.clone(),
                content_index: 1,
                kind: ContentBlockKind::ToolCall,
            },
            AssistantEvent::ReplayItemStarted {
                item_id: tool_item.clone(),
                ordinal: 1,
                target: ReplayTarget::ToolCall(call_id.clone()),
                kind: ReplayKind::new("google.genai.thought-signature"),
                applicability: ReplayApplicability::ExactProviderApiModel,
            },
            AssistantEvent::ReplayData {
                item_id: tool_item.clone(),
                operation: ReplayDataOperation::ReplaceUtf8("dG9vbA==".into()),
            },
            AssistantEvent::ReplayItemFinished { item_id: tool_item },
            AssistantEvent::ToolCallMetadata {
                block_id: tool_block.clone(),
                call_id: call_id.clone(),
                name: Some("read_file".into()),
            },
            AssistantEvent::ToolArgumentsDelta {
                block_id: tool_block.clone(),
                delta: r#"{"path":"README.md"}"#.into(),
            },
            AssistantEvent::ContentBlockFinished {
                block_id: tool_block,
            },
        ],
        finish(AssistantFinishReason::ToolUse),
    )
    .unwrap();
    let message = assemble(&run(response, request("google", "gemini-test")));
    assert_eq!(
        message.content,
        vec![
            ContentBlock::Text {
                id: text_block.clone(),
                text: String::new(),
            },
            ContentBlock::ToolCall {
                id: ContentBlockId::new("tool-1"),
                call: ToolCall {
                    id: call_id.clone(),
                    name: "read_file".into(),
                    arguments: json!({ "path": "README.md" }),
                },
            },
        ]
    );
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(text_block)
    );
    assert_eq!(
        message.replay.items[1].target,
        ReplayTarget::ToolCall(call_id)
    );
}

#[test]
fn simple_typed_and_erased_patch_conflict() {
    // §10.5; architecture basis: Part 2 §3.3.
    let erased = ErasedApiOptionsPatch {
        api: ApiId::new("anthropic-messages"),
        schema_version: 1,
        value: RawValue::from_string(r#"{"thinkingDisplay":"summarized"}"#.into()).unwrap(),
    };
    let result = ApiOptionsInput::<AnthropicMessages>::from_sources(Some(7_u32), Some(erased));
    assert!(matches!(
        result,
        Err(LoweringError::ConflictingApiOptions { api })
            if api == ApiId::new("anthropic-messages")
    ));
}

#[test]
fn simple_unknown_api_patch_rejected() {
    // §10.5; architecture basis: Part 2 §3.3.
    let erased = ErasedApiOptionsPatch {
        api: ApiId::new("openai-completions"),
        schema_version: 1,
        value: RawValue::from_string("{}".into()).unwrap(),
    };
    let result = ApiOptionsInput::<AnthropicMessages>::from_sources(None, Some(erased));
    assert!(matches!(
        result,
        Err(LoweringError::UnknownApiOptions { expected, actual })
            if expected == ApiId::new(AnthropicMessages::API_ID)
                && actual == ApiId::new("openai-completions")
    ));
}

#[test]
fn simple_api_patch_validation_uses_family_constant() {
    // §10.5; architecture basis: Part 2 §3.3. The expected API cannot be
    // supplied by the caller and is always derived from A::API_ID.
    let erased = ErasedApiOptionsPatch {
        api: ApiId::new("openai-completions"),
        schema_version: 1,
        value: RawValue::from_string("{}".into()).unwrap(),
    };

    assert!(matches!(
        ApiOptionsInput::<AnthropicMessages>::from_sources(None, Some(erased)),
        Err(LoweringError::UnknownApiOptions { expected, actual })
            if expected == ApiId::new("anthropic-messages")
                && actual == ApiId::new("openai-completions")
    ));
}

#[test]
fn reasoning_xhigh_clamps_in_pi_mode() {
    // §10.5; Pi basis: simple-options.ts clampReasoning.
    assert_eq!(
        ReasoningLevel::Xhigh
            .resolve_extended(false, false, ReasoningFallback::Clamp)
            .unwrap(),
        ReasoningLevel::High
    );
    assert_eq!(
        ReasoningLevel::Max
            .resolve_extended(false, false, ReasoningFallback::Clamp)
            .unwrap(),
        ReasoningLevel::High
    );
}

#[test]
fn reasoning_xhigh_rejects_in_strict_mode() {
    // §10.5; architecture basis: Part 2 §3.7 strict policy.
    assert_eq!(
        ReasoningLevel::Xhigh.resolve_extended(false, false, ReasoningFallback::Strict),
        Err(LoweringError::UnsupportedReasoningLevel {
            requested: ReasoningLevel::Xhigh
        })
    );
}

#[test]
fn thinking_budget_defaults_match_pi() {
    // §10.5; Pi basis: simple-options.ts DEFAULT_THINKING_BUDGETS.
    let budgets = ThinkingBudgets::default();
    assert_eq!(budgets.budget_for(ReasoningLevel::Minimal), Some(1_024));
    assert_eq!(budgets.budget_for(ReasoningLevel::Low), Some(2_048));
    assert_eq!(budgets.budget_for(ReasoningLevel::Medium), Some(8_192));
    assert_eq!(budgets.budget_for(ReasoningLevel::High), Some(16_384));
    assert_eq!(budgets.budget_for(ReasoningLevel::Xhigh), Some(16_384));
}

#[test]
fn simple_options_and_erased_patch_round_trip() {
    let options = SimpleGenerationOptions {
        max_output_tokens: Some(2_048),
        temperature: Some(0.25),
        top_p: Some(0.9),
        stop: vec!["STOP".into()],
        reasoning: Some(ReasoningLevel::High),
        seed: Some(42),
        session_id: Some("session-1".into()),
        cache_retention: Some(CacheRetention::Long),
        tool_choice: Some(ToolChoice::None),
        api_options: Some(ErasedApiOptionsPatch {
            api: ApiId::new("anthropic-messages"),
            schema_version: 1,
            value: RawValue::from_string(r#"{"thinkingDisplay":"summarized"}"#.into()).unwrap(),
        }),
        ..SimpleGenerationOptions::default()
    };
    let value = serde_json::to_value(&options).unwrap();
    assert_eq!(value["api_options"]["schemaVersion"], 1);
    let restored: SimpleGenerationOptions = serde_json::from_value(value).unwrap();
    assert_eq!(restored, options);
}

#[test]
fn simple_optional_planning_fields_preserve_omission_and_explicit_values() {
    // Architecture basis: Part 2 §3.4; Pi basis: types.ts SimpleStreamOptions
    // and simple-options.ts/buildBaseOptions.
    let omitted = SimpleGenerationOptions::default();
    assert_eq!(omitted.cache_retention, None);
    assert_eq!(omitted.tool_choice, None);

    let explicit = SimpleGenerationOptions {
        cache_retention: Some(CacheRetention::Short),
        tool_choice: Some(ToolChoice::Auto),
        ..SimpleGenerationOptions::default()
    };
    let restored: SimpleGenerationOptions =
        serde_json::from_value(serde_json::to_value(&explicit).unwrap()).unwrap();
    assert_eq!(restored.cache_retention, Some(CacheRetention::Short));
    assert_eq!(restored.tool_choice, Some(ToolChoice::Auto));
}
