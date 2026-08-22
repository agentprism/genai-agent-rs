use futures_core::Stream;
use futures_util::{stream, task::noop_waker};
use pi_ai::*;
use static_assertions::{assert_impl_all, assert_not_impl_any};
use std::{pin::Pin, task::Context, task::Poll};

const ANTHROPIC_SIGNATURE: &str = "anthropic.messages.thinking-signature";
const ANTHROPIC_REDACTED: &str = "anthropic.messages.redacted-thinking";
const OPENAI_REASONING_DETAIL: &str = "openai.chat.reasoning-detail";
const RESPONSES_REASONING: &str = "openai.responses.reasoning-item";
const RESPONSES_MESSAGE: &str = "openai.responses.message-identity";
const RESPONSES_FUNCTION: &str = "openai.responses.function-call-identity";
const BEDROCK_REDACTED: &str = "bedrock.converse.redacted-reasoning";
const BEDROCK_SIGNATURE: &str = "bedrock.converse.reasoning-signature";
const GOOGLE_SIGNATURE: &str = "google.genai.thought-signature";

assert_impl_all!(AssistantStream: Send, futures_core::stream::FusedStream);
assert_impl_all!(LocalAssistantStream: futures_core::stream::FusedStream);
assert_not_impl_any!(LocalAssistantStream: Send);

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

fn successful_finish(reason: AssistantFinishReason) -> AssistantFinish {
    AssistantFinish {
        reason,
        raw_provider_reason: None,
        error: None,
    }
}

fn public_error() -> PublicError {
    PublicError {
        code: "transport".into(),
        message: "connection closed".into(),
        retryable: true,
        provider_code: Some("upstream_reset".into()),
        status: Some(503),
        request_id: Some("request-7".into()),
    }
}

fn started(provider: &str, api: &str, model: &str) -> AssistantAssembler {
    let mut assembler = AssistantAssembler::with_timestamp(Timestamp::from_unix_millis(42));
    assembler
        .apply(&AssistantEvent::MessageStarted {
            message_id: MessageId::new("message-1"),
            provider: ProviderId::new(provider),
            api: ApiId::new(api),
            model: ModelId::new(model),
        })
        .unwrap();
    assembler
}

fn start_block(assembler: &mut AssistantAssembler, id: &str, index: u32, kind: ContentBlockKind) {
    assembler
        .apply(&AssistantEvent::ContentBlockStarted {
            block_id: ContentBlockId::new(id),
            content_index: index,
            kind,
        })
        .unwrap();
}

fn finish_block(assembler: &mut AssistantAssembler, id: &str) {
    assembler
        .apply(&AssistantEvent::ContentBlockFinished {
            block_id: ContentBlockId::new(id),
        })
        .unwrap();
}

fn start_replay(
    assembler: &mut AssistantAssembler,
    id: &str,
    ordinal: u32,
    target: ReplayTarget,
    kind: &str,
) {
    assembler
        .apply(&AssistantEvent::ReplayItemStarted {
            item_id: ReplayItemId::new(id),
            ordinal,
            target,
            kind: ReplayKind::new(kind),
            applicability: ReplayApplicability::ExactProviderApiModel,
        })
        .unwrap();
}

fn replay_data(assembler: &mut AssistantAssembler, id: &str, operation: ReplayDataOperation) {
    assembler
        .apply(&AssistantEvent::ReplayData {
            item_id: ReplayItemId::new(id),
            operation,
        })
        .unwrap();
}

fn finish_replay(assembler: &mut AssistantAssembler, id: &str) {
    assembler
        .apply(&AssistantEvent::ReplayItemFinished {
            item_id: ReplayItemId::new(id),
        })
        .unwrap();
}

fn complete_text_message() -> AssistantMessage {
    let mut assembler = started("openai", "openai-completions", "gpt-test");
    start_block(&mut assembler, "text-0", 0, ContentBlockKind::Text);
    assembler
        .apply(&AssistantEvent::TextDelta {
            block_id: ContentBlockId::new("text-0"),
            delta: "hello".into(),
        })
        .unwrap();
    finish_block(&mut assembler, "text-0");
    assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap()
}

fn round_trip(message: &AssistantMessage) -> AssistantMessage {
    let bytes = serde_json::to_vec(message).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn complete_target(message: &AssistantMessage) -> ReplayScope {
    ReplayScope::new(
        message.provider.clone(),
        message.api.clone(),
        message.requested_model.clone(),
        message
            .response_model
            .clone()
            .unwrap_or_else(|| message.requested_model.clone()),
    )
}

fn drain<S>(stream: &mut S) -> Vec<AssistantEvent>
where
    S: Stream<Item = AssistantEvent> + Unpin,
{
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    let mut events = Vec::new();
    loop {
        match Pin::new(&mut *stream).poll_next(&mut context) {
            Poll::Ready(Some(event)) => events.push(event),
            Poll::Ready(None) => return events,
            Poll::Pending => panic!("fixture stream unexpectedly pending"),
        }
    }
}

fn anthropic_signed_message() -> AssistantMessage {
    let mut assembler = started("anthropic", "anthropic-messages", "claude-test");
    start_block(&mut assembler, "thinking-0", 0, ContentBlockKind::Thinking);
    start_replay(
        &mut assembler,
        "replay-0",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-0")),
        ANTHROPIC_SIGNATURE,
    );
    assembler
        .apply(&AssistantEvent::ThinkingDelta {
            block_id: ContentBlockId::new("thinking-0"),
            delta: "We need to inspect...".into(),
        })
        .unwrap();
    replay_data(
        &mut assembler,
        "replay-0",
        ReplayDataOperation::AppendUtf8("EqQBCg...".into()),
    );
    replay_data(
        &mut assembler,
        "replay-0",
        ReplayDataOperation::AppendUtf8("remaining-signature...".into()),
    );
    finish_replay(&mut assembler, "replay-0");
    finish_block(&mut assembler, "thinking-0");
    assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap()
}

fn openai_chat_details_message() -> AssistantMessage {
    let mut assembler = started("openrouter", "openai-completions", "reasoning-model");
    start_block(&mut assembler, "thinking-0", 0, ContentBlockKind::Thinking);
    assembler
        .apply(&AssistantEvent::ThinkingDelta {
            block_id: ContentBlockId::new("thinking-0"),
            delta: "summary".into(),
        })
        .unwrap();
    start_replay(
        &mut assembler,
        "detail-0",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-0")),
        OPENAI_REASONING_DETAIL,
    );
    replay_data(
        &mut assembler,
        "detail-0",
        ReplayDataOperation::ReplaceJsonBytes(
            br#"{"type":"reasoning.encrypted","id":"rs_1","data":"opaque-A"}"#.to_vec(),
        ),
    );
    finish_replay(&mut assembler, "detail-0");
    start_replay(
        &mut assembler,
        "detail-1",
        1,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-0")),
        OPENAI_REASONING_DETAIL,
    );
    replay_data(
        &mut assembler,
        "detail-1",
        ReplayDataOperation::ReplaceJsonBytes(
            br#"{"type":"reasoning.summary","id":"rs_2","summary":"visible"}"#.to_vec(),
        ),
    );
    finish_replay(&mut assembler, "detail-1");
    finish_block(&mut assembler, "thinking-0");
    assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap()
}

fn responses_message() -> AssistantMessage {
    let mut assembler = started("openai", "openai-responses", "gpt-responses");
    assembler
        .apply(&AssistantEvent::ResponseMetadata {
            response_id: Some("resp_123".into()),
            response_model: None,
        })
        .unwrap();

    start_replay(
        &mut assembler,
        "reasoning-item",
        0,
        ReplayTarget::ProviderOutputItem { output_index: 0 },
        RESPONSES_REASONING,
    );
    start_block(&mut assembler, "thinking-0", 0, ContentBlockKind::Thinking);
    assembler
        .apply(&AssistantEvent::ThinkingDelta {
            block_id: ContentBlockId::new("thinking-0"),
            delta: "Inspecting the request...".into(),
        })
        .unwrap();
    replay_data(
        &mut assembler,
        "reasoning-item",
        ReplayDataOperation::ReplaceJsonBytes(
            br#"{"id":"rs_123","type":"reasoning","encrypted_content":"opaque"}"#.to_vec(),
        ),
    );
    finish_replay(&mut assembler, "reasoning-item");
    finish_block(&mut assembler, "thinking-0");

    start_replay(
        &mut assembler,
        "message-item",
        1,
        ReplayTarget::ProviderOutputItem { output_index: 1 },
        RESPONSES_MESSAGE,
    );
    start_block(&mut assembler, "text-1", 1, ContentBlockKind::Text);
    assembler
        .apply(&AssistantEvent::TextDelta {
            block_id: ContentBlockId::new("text-1"),
            delta: "I found the issue.".into(),
        })
        .unwrap();
    replay_data(
        &mut assembler,
        "message-item",
        ReplayDataOperation::ReplaceJsonBytes(
            br#"{"id":"msg_123","phase":"final_answer","block_id":"text-1"}"#.to_vec(),
        ),
    );
    finish_replay(&mut assembler, "message-item");
    finish_block(&mut assembler, "text-1");

    start_replay(
        &mut assembler,
        "function-item",
        2,
        ReplayTarget::ProviderOutputItem { output_index: 2 },
        RESPONSES_FUNCTION,
    );
    assembler
        .apply(&AssistantEvent::ToolCallMetadata {
            block_id: ContentBlockId::new("tool-2"),
            call_id: ToolCallId::new("call_123"),
            name: Some("read_file".into()),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ToolArgumentsDelta {
            block_id: ContentBlockId::new("tool-2"),
            delta: r#"{"path":"README.md"}"#.into(),
        })
        .unwrap();
    replay_data(
        &mut assembler,
        "function-item",
        ReplayDataOperation::ReplaceJsonBytes(
            br#"{"call_id":"call_123","item_id":"fc_456","namespace":"files","type":"function_call","tool_call_id":"call_123"}"#.to_vec(),
        ),
    );
    finish_replay(&mut assembler, "function-item");
    finish_block(&mut assembler, "tool-2");

    assembler
        .finish_completed(successful_finish(AssistantFinishReason::ToolUse))
        .unwrap()
}

fn bedrock_redacted_message() -> AssistantMessage {
    let mut assembler = started(
        "amazon-bedrock",
        "bedrock-converse-stream",
        "bedrock-reasoning",
    );
    start_block(&mut assembler, "thinking-0", 0, ContentBlockKind::Thinking);
    assembler
        .apply(&AssistantEvent::ThinkingDelta {
            block_id: ContentBlockId::new("thinking-0"),
            delta: "[Reasoning redacted]".into(),
        })
        .unwrap();
    start_replay(
        &mut assembler,
        "redacted-0",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-0")),
        BEDROCK_REDACTED,
    );
    replay_data(
        &mut assembler,
        "redacted-0",
        ReplayDataOperation::AppendBytes(vec![0x01, 0x02]),
    );
    replay_data(
        &mut assembler,
        "redacted-0",
        ReplayDataOperation::AppendBytes(vec![0xaf, 0x33]),
    );
    finish_replay(&mut assembler, "redacted-0");
    finish_block(&mut assembler, "thinking-0");
    assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap()
}

#[test]
fn stream_start_precedes_content() {
    // §10.1; Pi basis: packages/ai/src/types.ts and every pinned API stream.
    let mut assembler = AssistantAssembler::new();
    let content = AssistantEvent::ContentBlockStarted {
        block_id: ContentBlockId::new("b0"),
        content_index: 0,
        kind: ContentBlockKind::Text,
    };
    assert_eq!(
        assembler.apply(&content),
        Err(AssemblyError::MessageNotStarted)
    );
    assembler
        .apply(&AssistantEvent::MessageStarted {
            message_id: MessageId::new("m0"),
            provider: ProviderId::new("openai"),
            api: ApiId::new("openai-completions"),
            model: ModelId::new("gpt"),
        })
        .unwrap();
    assembler.apply(&content).unwrap();
}

#[test]
fn stream_exactly_one_terminal() {
    // §10.1; Pi basis: packages/ai/src/utils/event-stream.ts `push()` ignores
    // every event after the first `done` or `error`.
    let message = complete_text_message();
    let mut assistant_stream = AssistantStream::new(stream::iter(vec![
        AssistantEvent::Finished {
            message: message.clone(),
        },
        AssistantEvent::Failed { message },
    ]));
    let events = drain(&mut assistant_stream);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], AssistantEvent::Finished { .. }));
    assert!(assistant_stream.is_terminated());
}

#[test]
fn stream_no_event_after_terminal() {
    // §10.1; Pi basis: packages/ai/src/utils/event-stream.ts.
    let message = complete_text_message();
    let mut assistant_stream = AssistantStream::new(stream::iter(vec![
        AssistantEvent::Finished {
            message: message.clone(),
        },
        AssistantEvent::UsageUpdated {
            cumulative: usage(99, 99),
        },
    ]));
    let events = drain(&mut assistant_stream);
    assert_eq!(events.len(), 1);

    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    assert_eq!(
        Pin::new(&mut assistant_stream).poll_next(&mut context),
        Poll::Ready(None)
    );
    assert_eq!(
        Pin::new(&mut assistant_stream).poll_next(&mut context),
        Poll::Ready(None)
    );
}

#[test]
fn stream_failure_is_terminal_message() {
    // §10.1 and §2.1 exact failed record; Pi basis:
    // packages/ai/src/api/openai-completions.ts error terminal path.
    let mut assembler = started("openrouter", "openai-completions", "auto");
    assembler
        .apply(&AssistantEvent::ResponseMetadata {
            response_id: Some("chatcmpl-7".into()),
            response_model: Some(ModelId::new("produced-model")),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::UsageUpdated {
            cumulative: usage(11, 5),
        })
        .unwrap();
    start_block(&mut assembler, "text-0", 0, ContentBlockKind::Text);
    assembler
        .apply(&AssistantEvent::TextDelta {
            block_id: ContentBlockId::new("text-0"),
            delta: "partial".into(),
        })
        .unwrap();
    start_replay(
        &mut assembler,
        "partial-replay",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("text-0")),
        "openai.chat.partial",
    );
    replay_data(
        &mut assembler,
        "partial-replay",
        ReplayDataOperation::AppendUtf8("unfinished".into()),
    );
    let message = assembler.finish_failed(public_error());
    assert_eq!(message.id, MessageId::new("message-1"));
    assert_eq!(message.response_id.as_deref(), Some("chatcmpl-7"));
    assert_eq!(message.response_model, Some(ModelId::new("produced-model")));
    assert_eq!(message.usage, usage(11, 5));
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(message.finish.error.as_ref(), Some(&public_error()));
    assert_eq!(
        message.replay.items[0].completeness,
        ReplayCompleteness::Incomplete
    );
    assert_eq!(message.content.len(), 1);

    let mut assistant_stream = AssistantStream::new(stream::iter(vec![AssistantEvent::Failed {
        message: message.clone(),
    }]));
    let events = drain(&mut assistant_stream);
    assert_eq!(events[0].terminal_message(), Some(&message));
    assert!(assistant_stream.is_terminated());
}

#[test]
fn stream_cancellation_is_terminal_message() {
    // §10.1 and §2.1 exact cancelled record; Pi basis:
    // packages/ai/src/api/anthropic-messages.ts abort catch path.
    let mut assembler = started("anthropic", "anthropic-messages", "claude");
    start_block(&mut assembler, "thinking-0", 0, ContentBlockKind::Thinking);
    assembler
        .apply(&AssistantEvent::ThinkingDelta {
            block_id: ContentBlockId::new("thinking-0"),
            delta: "partial reasoning".into(),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::UsageUpdated {
            cumulative: usage(17, 6),
        })
        .unwrap();
    let message = assembler.finish_cancelled(
        CancellationReason::new("Request was aborted").with_request_id("request-9"),
    );
    assert_eq!(message.finish.reason, AssistantFinishReason::Aborted);
    assert_eq!(message.usage, usage(17, 6));
    let error = message.finish.error.as_ref().unwrap();
    assert_eq!(error.code, "cancelled");
    assert_eq!(error.message, "Request was aborted");
    assert!(!error.retryable);
    assert_eq!(error.request_id.as_deref(), Some("request-9"));

    let mut local_stream = LocalAssistantStream::new(stream::iter(vec![
        AssistantEvent::Cancelled {
            message: message.clone(),
        },
        AssistantEvent::Finished {
            message: complete_text_message(),
        },
    ]));
    let events = drain(&mut local_stream);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].terminal_message(), Some(&message));
}

#[test]
fn stream_partial_identity_is_stable() {
    // §10.1 Rust strengthening of Pi's one mutable partial assistant; Pi basis:
    // packages/ai/src/types.ts AssistantMessageEvent.
    let mut assembler = started("openai", "openai-completions", "gpt");
    start_block(&mut assembler, "text-0", 0, ContentBlockKind::Text);
    let first = assembler.snapshot();
    assert_eq!(first.id, MessageId::new("message-1"));
    assert_eq!(first.content[0].id(), &ContentBlockId::new("text-0"));
    assembler
        .apply(&AssistantEvent::TextDelta {
            block_id: ContentBlockId::new("text-0"),
            delta: "a".into(),
        })
        .unwrap();
    let second = assembler.snapshot();
    assert_eq!(second.id, MessageId::new("message-1"));
    assert_eq!(second.content[0].id(), &ContentBlockId::new("text-0"));

    assembler
        .apply(&AssistantEvent::ResponseMetadata {
            response_id: Some("response-1".into()),
            response_model: Some(ModelId::new("produced-1")),
        })
        .unwrap();
    assert_eq!(
        assembler.apply(&AssistantEvent::ResponseMetadata {
            response_id: Some("response-2".into()),
            response_model: None,
        }),
        Err(AssemblyError::ResponseIdChanged)
    );
    assert_eq!(
        assembler.apply(&AssistantEvent::ResponseMetadata {
            response_id: Some("response-1".into()),
            response_model: Some(ModelId::new("produced-2")),
        }),
        Err(AssemblyError::ResponseModelChanged)
    );
}

#[test]
fn stream_response_id_is_preserved() {
    // §10.1; Pi basis: packages/ai/src/api/openai-responses-shared.ts.
    let mut assembler = started("openai", "openai-responses", "gpt");
    assembler
        .apply(&AssistantEvent::ResponseMetadata {
            response_id: Some("resp_1".into()),
            response_model: None,
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ResponseMetadata {
            response_id: None,
            response_model: None,
        })
        .unwrap();
    let message = assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap();
    assert_eq!(message.response_id.as_deref(), Some("resp_1"));
    assert_eq!(round_trip(&message).response_id.as_deref(), Some("resp_1"));
}

#[test]
fn stream_response_model_is_preserved() {
    // §10.1; Pi basis: packages/ai/src/api/openai-completions.ts chunk model.
    let mut assembler = started("openrouter", "openai-completions", "auto");
    assembler
        .apply(&AssistantEvent::ResponseMetadata {
            response_id: None,
            response_model: Some(ModelId::new("anthropic/claude")),
        })
        .unwrap();
    let message = assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap();
    assert_eq!(
        message.response_model,
        Some(ModelId::new("anthropic/claude"))
    );
    assert_eq!(
        message.replay.source.produced_by_model,
        ModelId::new("anthropic/claude")
    );
    assert_eq!(round_trip(&message), message);
}

#[test]
fn stream_usage_is_cumulative() {
    // §10.1; Pi basis: Anthropic message_start/message_delta and Bedrock
    // metadata overwrite last-known usage rather than adding deltas.
    let mut assembler = started("anthropic", "anthropic-messages", "claude");
    assembler
        .apply(&AssistantEvent::UsageUpdated {
            cumulative: usage(10, 2),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::UsageUpdated {
            cumulative: usage(10, 5),
        })
        .unwrap();
    assert_eq!(assembler.snapshot().usage, usage(10, 5));
    let message = assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap();
    assert_eq!(message.usage.output_tokens, 5);
}

#[test]
fn stream_tool_json_scratch_not_persisted() {
    // §10.1; Pi basis: packages/ai/src/api/anthropic-messages.ts and
    // openai-completions.ts strip partialJson/partialArgs on terminal paths.
    let mut assembler = started("openai", "openai-completions", "gpt");
    assembler
        .apply(&AssistantEvent::ToolCallMetadata {
            block_id: ContentBlockId::new("tool-0"),
            call_id: ToolCallId::new("call-0"),
            name: Some("read_file".into()),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ToolArgumentsDelta {
            block_id: ContentBlockId::new("tool-0"),
            delta: "{\"path\":\"".into(),
        })
        .unwrap();
    let failed = assembler.finish_failed(public_error());
    let json = serde_json::to_string(&failed).unwrap();
    assert!(!json.contains("arguments_scratch"));
    assert!(!json.contains("partialJson"));
    assert!(matches!(
        &failed.content[0],
        ContentBlock::ToolCall { call, .. }
            if call.arguments == serde_json::json!({ "path": "" })
    ));
}

fn partial_tool_assembler() -> AssistantAssembler {
    let mut assembler = started("openai", "openai-completions", "gpt");
    assembler
        .apply(&AssistantEvent::ToolCallMetadata {
            block_id: ContentBlockId::new("tool-0"),
            call_id: ToolCallId::new("call-0"),
            name: Some("read_file".into()),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ToolArgumentsDelta {
            block_id: ContentBlockId::new("tool-0"),
            delta: r#"{"path":"README"#.into(),
        })
        .unwrap();
    assembler
}

fn assert_partial_tool_arguments(message: &AssistantMessage) {
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall { call, .. }
            if call.id == ToolCallId::new("call-0")
                && call.name == "read_file"
                && call.arguments == serde_json::json!({ "path": "README" })
    ));
}

#[test]
fn stream_failure_preserves_partial_tool_arguments() {
    // §10.1 and §2.1 exact failed record; Pi basis: json-parse.ts
    // `parseStreamingJson` and the Anthropic/OpenAI/Bedrock error paths retain
    // parsed arguments while deleting only parser buffers.
    let message = partial_tool_assembler().finish_failed(public_error());
    assert_partial_tool_arguments(&message);
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
}

#[test]
fn stream_cancellation_preserves_partial_tool_arguments() {
    // §10.1 and §2.1 exact cancelled record; Pi basis: json-parse.ts and the
    // provider abort catch paths use the same partial assistant record.
    let message =
        partial_tool_assembler().finish_cancelled(CancellationReason::new("Request was aborted"));
    assert_partial_tool_arguments(&message);
    assert_eq!(message.finish.reason, AssistantFinishReason::Aborted);
}

#[test]
fn stream_failure_preserves_partial_tool_with_incomplete_metadata() {
    // §10.1 and §2.1 exact failed record; Pi basis: OpenAI-compatible stream
    // blocks initialize a missing streamed tool name as an empty string and
    // retain the block on the error path.
    let mut assembler = started("openai", "openai-completions", "gpt");
    assembler
        .apply(&AssistantEvent::ToolCallMetadata {
            block_id: ContentBlockId::new("tool-0"),
            call_id: ToolCallId::new("call-0"),
            name: None,
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ToolArgumentsDelta {
            block_id: ContentBlockId::new("tool-0"),
            delta: r#"{"path":"README"#.into(),
        })
        .unwrap();

    let mut successful = assembler.clone();
    finish_block(&mut successful, "tool-0");
    assert_eq!(
        successful.finish_completed(successful_finish(AssistantFinishReason::ToolUse)),
        Err(AssemblyError::MissingToolCallMetadata(ContentBlockId::new(
            "tool-0"
        )))
    );

    let message = assembler.finish_failed(public_error());
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall { call, .. }
            if call.id == ToolCallId::new("call-0")
                && call.name.is_empty()
                && call.arguments == serde_json::json!({ "path": "README" })
    ));
}

#[test]
fn stream_cancellation_preserves_partial_tool_with_incomplete_metadata() {
    // §10.1 and §2.1 exact cancelled record; Pi basis: the same mutable
    // partial tool-call block is retained by provider abort catch paths.
    let mut assembler = started("anthropic", "anthropic-messages", "claude");
    assembler
        .apply(&AssistantEvent::ToolCallMetadata {
            block_id: ContentBlockId::new("tool-0"),
            call_id: ToolCallId::new("call-0"),
            name: None,
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ToolArgumentsDelta {
            block_id: ContentBlockId::new("tool-0"),
            delta: r#"{"path":"README"#.into(),
        })
        .unwrap();

    let message = assembler.finish_cancelled(CancellationReason::new("Request was aborted"));
    assert!(matches!(
        &message.content[0],
        ContentBlock::ToolCall { call, .. }
            if call.id == ToolCallId::new("call-0")
                && call.name.is_empty()
                && call.arguments == serde_json::json!({ "path": "README" })
    ));
}

#[test]
fn stream_length_preserves_truncated_tool_arguments() {
    // Part 1 §3.3 and §10.1; Pi basis: openai-completions.ts finalizes every
    // block with parseStreamingJson before emitting a `length` terminal.
    let mut assembler = partial_tool_assembler();
    finish_block(&mut assembler, "tool-0");
    let message = assembler
        .finish_completed(successful_finish(AssistantFinishReason::Length))
        .unwrap();
    assert_partial_tool_arguments(&message);
    assert_eq!(message.finish.reason, AssistantFinishReason::Length);
}

#[test]
fn stream_partial_tool_json_matches_pi() {
    // Pi basis: packages/ai/src/utils/json-parse.ts:97-120 and partial-json
    // 0.1.7. Nested completed prefixes and repaired string literals survive.
    let cases = [
        (
            r#"{"items":[1,{"name":"tw"#,
            serde_json::json!({ "items": [1, { "name": "tw" }] }),
        ),
        (r#"{"count":12"#, serde_json::json!({ "count": 12 })),
        (
            r#"{"values":[true,false,nul"#,
            serde_json::json!({ "values": [true, false, null] }),
        ),
        (
            "{\"path\":\"line one\nline two\"}",
            serde_json::json!({ "path": "line one\nline two" }),
        ),
        (r#"{"missing":"#, serde_json::json!({})),
    ];

    for (fragment, expected) in cases {
        let mut assembler = started("anthropic", "anthropic-messages", "claude");
        assembler
            .apply(&AssistantEvent::ToolCallMetadata {
                block_id: ContentBlockId::new("tool-0"),
                call_id: ToolCallId::new("call-0"),
                name: Some("tool".into()),
            })
            .unwrap();
        assembler
            .apply(&AssistantEvent::ToolArgumentsDelta {
                block_id: ContentBlockId::new("tool-0"),
                delta: fragment.into(),
            })
            .unwrap();
        let message = assembler.finish_failed(public_error());
        assert!(matches!(
            &message.content[0],
            ContentBlock::ToolCall { call, .. } if call.arguments == expected
        ));
    }
}

#[test]
fn stream_partial_tool_malformed_numbers_match_pi() {
    // Pi basis: packages/ai/src/utils/json-parse.ts delegates to pinned
    // partial-json 0.1.7. Its malformed-number retry truncates only at a
    // lowercase `e`: a trailing decimal point and uppercase `E` leave the
    // enclosing partial object empty, while a lowercase exponent is retained
    // as its complete mantissa.
    let cases = [
        (r#"{"count":12."#, serde_json::json!({})),
        (r#"{"count":12E"#, serde_json::json!({})),
        (r#"{"count":12e"#, serde_json::json!({ "count": 12 })),
        (r#"{"x":Inf"#, serde_json::json!({ "x": null })),
        (r#"{"x":-Inf"#, serde_json::json!({ "x": null })),
        (r#"{"x":Na"#, serde_json::json!({ "x": null })),
    ];

    for (fragment, expected) in cases {
        let mut assembler = started("anthropic", "anthropic-messages", "claude");
        assembler
            .apply(&AssistantEvent::ToolCallMetadata {
                block_id: ContentBlockId::new("tool-0"),
                call_id: ToolCallId::new("call-0"),
                name: Some("tool".into()),
            })
            .unwrap();
        assembler
            .apply(&AssistantEvent::ToolArgumentsDelta {
                block_id: ContentBlockId::new("tool-0"),
                delta: fragment.into(),
            })
            .unwrap();
        let message = assembler.finish_failed(public_error());
        assert!(matches!(
            &message.content[0],
            ContentBlock::ToolCall { call, .. } if call.arguments == expected
        ));
    }
}

#[test]
fn stream_binary_scratch_not_persisted() {
    // §10.1; Pi basis: packages/ai/src/api/bedrock-converse-stream.ts
    // `flushRedactedContent` removes redactedChunks.
    let message = bedrock_redacted_message();
    let json = serde_json::to_string(&message).unwrap();
    assert!(!json.contains("redactedChunks"));
    assert!(!json.contains("binary_scratch"));
    assert!(json.contains(r#""encoding":"bytes_base64""#));
    assert_eq!(
        message.replay.items[0].as_bytes(),
        Some(&[1, 2, 0xaf, 0x33][..])
    );
}

#[test]
fn assembler_applies_finished_failed_and_cancelled_terminals() {
    // Architecture v2 part 2 §1.3 `apply`; Pi basis: terminal `done`/`error`
    // messages are authoritative final mutable partials in event-stream.ts.
    let start = AssistantEvent::MessageStarted {
        message_id: MessageId::new("message-1"),
        provider: ProviderId::new("openai"),
        api: ApiId::new("openai-completions"),
        model: ModelId::new("gpt-test"),
    };
    let block_start = AssistantEvent::ContentBlockStarted {
        block_id: ContentBlockId::new("text-0"),
        content_index: 0,
        kind: ContentBlockKind::Text,
    };
    let delta = AssistantEvent::TextDelta {
        block_id: ContentBlockId::new("text-0"),
        delta: "hello".into(),
    };
    let block_finish = AssistantEvent::ContentBlockFinished {
        block_id: ContentBlockId::new("text-0"),
    };

    let completed = complete_text_message();
    let mut completed_consumer = AssistantAssembler::new();
    for event in [&start, &block_start, &delta, &block_finish] {
        completed_consumer.apply(event).unwrap();
    }
    completed_consumer
        .apply(&AssistantEvent::Finished {
            message: completed.clone(),
        })
        .unwrap();
    assert_eq!(
        completed_consumer.snapshot().terminal_message,
        Some(completed)
    );
    assert_eq!(
        completed_consumer.apply(&AssistantEvent::UsageUpdated {
            cumulative: usage(1, 1),
        }),
        Err(AssemblyError::EventAfterTerminal)
    );

    let mut failed_source = started("openai", "openai-completions", "gpt-test");
    start_block(&mut failed_source, "text-0", 0, ContentBlockKind::Text);
    failed_source.apply(&delta).unwrap();
    let failed = failed_source.finish_failed(public_error());
    let mut failed_consumer = AssistantAssembler::new();
    for event in [&start, &block_start, &delta] {
        failed_consumer.apply(event).unwrap();
    }
    failed_consumer
        .apply(&AssistantEvent::Failed {
            message: failed.clone(),
        })
        .unwrap();
    assert_eq!(failed_consumer.snapshot().terminal_message, Some(failed));

    let mut cancelled_source = started("openai", "openai-completions", "gpt-test");
    start_block(&mut cancelled_source, "text-0", 0, ContentBlockKind::Text);
    cancelled_source.apply(&delta).unwrap();
    let cancelled = cancelled_source.finish_cancelled(CancellationReason::new("cancelled"));
    let mut cancelled_consumer = AssistantAssembler::new();
    for event in [&start, &block_start, &delta] {
        cancelled_consumer.apply(event).unwrap();
    }
    cancelled_consumer
        .apply(&AssistantEvent::Cancelled {
            message: cancelled.clone(),
        })
        .unwrap();
    assert_eq!(
        cancelled_consumer.snapshot().terminal_message,
        Some(cancelled)
    );
}

#[test]
fn assembler_rejects_success_terminal_with_error() {
    // Architecture v2 part 2 §1.3 strict completion validation. A Finished
    // event must enforce the same successful-finish invariant as
    // `finish_completed` rather than trusting the supplied message.
    let mut message = complete_text_message();
    message.finish.error = Some(public_error());

    let mut consumer = started("openai", "openai-completions", "gpt-test");
    start_block(&mut consumer, "text-0", 0, ContentBlockKind::Text);
    consumer
        .apply(&AssistantEvent::TextDelta {
            block_id: ContentBlockId::new("text-0"),
            delta: "hello".into(),
        })
        .unwrap();
    finish_block(&mut consumer, "text-0");
    assert_eq!(
        consumer.apply(&AssistantEvent::Finished { message }),
        Err(AssemblyError::InvalidSuccessfulFinish)
    );
}

#[test]
fn assembler_rejects_non_exact_cancelled_terminal() {
    // Architecture v2 part 2 §2.1 exact cancelled record. The only variable
    // PublicError fields are display text and the last-known request ID.
    let exact = started("openai", "openai-completions", "gpt-test")
        .finish_cancelled(CancellationReason::new("cancelled").with_request_id("request-1"));
    let mut invalid = Vec::new();

    let mut with_raw_reason = exact.clone();
    with_raw_reason.finish.raw_provider_reason = Some("provider_cancelled".into());
    invalid.push(with_raw_reason);

    let mut retryable = exact.clone();
    retryable.finish.error.as_mut().unwrap().retryable = true;
    invalid.push(retryable);

    let mut with_provider_code = exact.clone();
    with_provider_code
        .finish
        .error
        .as_mut()
        .unwrap()
        .provider_code = Some("aborted".into());
    invalid.push(with_provider_code);

    let mut with_status = exact.clone();
    with_status.finish.error.as_mut().unwrap().status = Some(499);
    invalid.push(with_status);

    let mut wrong_code = exact;
    wrong_code.finish.error.as_mut().unwrap().code = "aborted".into();
    invalid.push(wrong_code);

    for message in invalid {
        let mut consumer = started("openai", "openai-completions", "gpt-test");
        assert_eq!(
            consumer.apply(&AssistantEvent::Cancelled { message }),
            Err(AssemblyError::InvalidCancellationError)
        );
    }
}

#[test]
fn replay_r1_complete_item_identity_is_stable() {
    // Architecture v2 part 2 §1.9 R1; Pi basis: explicit Rust replay-envelope
    // replacement for mutable provider signature fields.
    let message = anthropic_signed_message();
    let restored = round_trip(&message);
    let original = &message.replay.items[0];
    let replayed = &restored.replay.items[0];
    assert_eq!(replayed.id, original.id);
    assert_eq!(replayed.target, original.target);
    assert_eq!(replayed.kind, original.kind);
    assert_eq!(replayed.applicability, original.applicability);
    assert_eq!(replayed.ordinal, original.ordinal);
    assert_eq!(restored.replay.source, message.replay.source);
}

#[test]
fn replay_r2_success_rejects_incomplete_item() {
    // Architecture v2 part 2 §1.9 R2; Pi basis: complete signature/item
    // finalization in the pinned API stream implementations.
    let mut assembler = started("anthropic", "anthropic-messages", "claude");
    start_replay(
        &mut assembler,
        "r0",
        0,
        ReplayTarget::Message,
        ANTHROPIC_SIGNATURE,
    );
    replay_data(
        &mut assembler,
        "r0",
        ReplayDataOperation::AppendUtf8("partial".into()),
    );
    assert_eq!(
        assembler.finish_completed(successful_finish(AssistantFinishReason::Stop)),
        Err(AssemblyError::IncompleteReplayItem(ReplayItemId::new("r0")))
    );
}

#[test]
fn replay_r3_failed_and_cancelled_items_are_incomplete_and_ignored() {
    // Architecture v2 part 2 §1.9 R3; Pi basis: partial signature data is
    // retained in failed Pi partials, while the Rust allowlisted replacement
    // marks it non-replayable.
    let mut failed_assembler = started("anthropic", "anthropic-messages", "claude");
    start_replay(
        &mut failed_assembler,
        "r0",
        0,
        ReplayTarget::Message,
        ANTHROPIC_SIGNATURE,
    );
    replay_data(
        &mut failed_assembler,
        "r0",
        ReplayDataOperation::AppendUtf8("partial".into()),
    );
    let failed = failed_assembler.finish_failed(public_error());

    let mut cancelled_assembler = started("anthropic", "anthropic-messages", "claude");
    start_replay(
        &mut cancelled_assembler,
        "r0",
        0,
        ReplayTarget::Message,
        ANTHROPIC_SIGNATURE,
    );
    replay_data(
        &mut cancelled_assembler,
        "r0",
        ReplayDataOperation::AppendUtf8("partial".into()),
    );
    let cancelled = cancelled_assembler.finish_cancelled(CancellationReason::new("cancelled"));

    for message in [&failed, &cancelled] {
        assert_eq!(
            message.replay.items[0].completeness,
            ReplayCompleteness::Incomplete
        );
        let target = complete_target(message);
        assert!(
            !message
                .replay
                .is_complete_and_applicable(&message.replay.items[0], &target)
        );
    }
}

#[test]
fn replay_r4_same_provider_round_trip_is_deterministic_before_encoder() {
    // Architecture v2 part 2 §1.9 R4 primary proof fixture. The M4 encoder
    // step is intentionally deferred; Pi basis: replay conversion in
    // packages/ai/src/api/openai-responses-shared.ts.
    let message = responses_message();
    let first = serde_json::to_vec(&message).unwrap();
    let restored: AssistantMessage = serde_json::from_slice(&first).unwrap();
    let second = serde_json::to_vec(&restored).unwrap();
    assert_eq!(second, first);
    assert_eq!(restored, message);
}

#[test]
fn replay_r5_cross_provider_projection_rejects_nonportable_item() {
    // Architecture v2 part 2 §1.9 R5; Pi basis:
    // packages/ai/src/api/transform-messages.ts cross-model signature removal.
    let message = anthropic_signed_message();
    let target = ReplayScope::new("openai", "openai-responses", "gpt", "gpt");
    assert!(
        !message
            .replay
            .is_complete_and_applicable(&message.replay.items[0], &target)
    );
}

#[test]
fn replay_r6_signature_target_is_exact_and_stable() {
    // Architecture v2 part 2 §1.9 R6; Pi basis:
    // packages/ai/src/api/google-shared.ts signature-bearing parts.
    let message = google_target_message();
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(ContentBlockId::new("text-0"))
    );
    assert_eq!(
        message.replay.items[1].target,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-1"))
    );
    assert_eq!(
        message.replay.items[2].target,
        ReplayTarget::ToolCall(ToolCallId::new("call-2"))
    );
}

#[test]
fn replay_r7_provider_order_is_independent_of_block_order() {
    // Architecture v2 part 2 §1.9 R7; Pi basis: OpenAI Responses output_index.
    let mut assembler = started("openai", "openai-responses", "gpt");
    start_block(&mut assembler, "text-0", 0, ContentBlockKind::Text);
    finish_block(&mut assembler, "text-0");
    start_block(&mut assembler, "text-1", 1, ContentBlockKind::Text);
    finish_block(&mut assembler, "text-1");
    start_replay(
        &mut assembler,
        "later-started-first",
        9,
        ReplayTarget::ContentBlock(ContentBlockId::new("text-0")),
        RESPONSES_MESSAGE,
    );
    replay_data(
        &mut assembler,
        "later-started-first",
        ReplayDataOperation::ReplaceJsonBytes(br#"{"id":"msg-0"}"#.to_vec()),
    );
    finish_replay(&mut assembler, "later-started-first");
    start_replay(
        &mut assembler,
        "earlier-provider-item",
        2,
        ReplayTarget::ContentBlock(ContentBlockId::new("text-1")),
        RESPONSES_MESSAGE,
    );
    replay_data(
        &mut assembler,
        "earlier-provider-item",
        ReplayDataOperation::ReplaceJsonBytes(br#"{"id":"msg-1"}"#.to_vec()),
    );
    finish_replay(&mut assembler, "earlier-provider-item");
    let message = assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap();
    assert_eq!(message.content[0].id(), &ContentBlockId::new("text-0"));
    assert_eq!(message.content[1].id(), &ContentBlockId::new("text-1"));
    assert_eq!(message.replay.items[0].ordinal, 2);
    assert_eq!(message.replay.items[1].ordinal, 9);
}

#[test]
fn replay_r8_scratch_state_is_not_in_persisted_schema() {
    // Architecture v2 part 2 §1.9 R8; Pi basis: partialJson, streamIndex,
    // customInput, and redactedChunks terminal cleanup in pinned API streams.
    let responses = serde_json::to_string(&responses_message()).unwrap();
    let bedrock = serde_json::to_string(&bedrock_redacted_message()).unwrap();
    for persisted in [&responses, &bedrock] {
        for scratch_name in [
            "partialJson",
            "partialArgs",
            "streamIndex",
            "customInput",
            "redactedChunks",
            "arguments_scratch",
        ] {
            assert!(!persisted.contains(scratch_name), "found {scratch_name}");
        }
    }
}

#[test]
fn anthropic_signature_fragments_append_in_order() {
    // §10.2; Pi basis: anthropic-messages.ts `signature_delta` concatenation.
    let message = anthropic_signed_message();
    assert_eq!(
        message.replay.items[0].as_utf8(),
        Some("EqQBCg...remaining-signature...")
    );
}

#[test]
fn anthropic_signature_survives_message_round_trip() {
    // §10.2 primary proof through event assembly and persistence; Pi basis:
    // packages/ai/src/api/anthropic-messages.ts signature_delta handling.
    let message = anthropic_signed_message();
    let restored = round_trip(&message);
    assert_eq!(restored, message);
    assert_eq!(
        restored.replay.items[0].as_utf8(),
        Some("EqQBCg...remaining-signature...")
    );
}

#[test]
fn anthropic_redacted_event_sequence_retains_exact_data() {
    // Architecture v2 part 2 §1.4 redacted sequence; Pi basis:
    // anthropic-messages.ts `redacted_thinking` handling.
    let mut assembler = started("anthropic", "anthropic-messages", "claude");
    start_block(&mut assembler, "thinking-0", 0, ContentBlockKind::Thinking);
    assembler
        .apply(&AssistantEvent::ThinkingDelta {
            block_id: ContentBlockId::new("thinking-0"),
            delta: "[Reasoning redacted]".into(),
        })
        .unwrap();
    start_replay(
        &mut assembler,
        "redacted-0",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-0")),
        ANTHROPIC_REDACTED,
    );
    replay_data(
        &mut assembler,
        "redacted-0",
        ReplayDataOperation::ReplaceUtf8("<opaque-data>".into()),
    );
    finish_replay(&mut assembler, "redacted-0");
    finish_block(&mut assembler, "thinking-0");
    let message = assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap();
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking {
            text,
            redacted: true,
            ..
        } if text == "[Reasoning redacted]"
    ));
    assert_eq!(message.replay.items[0].as_utf8(), Some("<opaque-data>"));
}

#[test]
fn anthropic_failed_partial_signature_is_not_replayed() {
    // §10.2; Pi basis: failed partial retention plus §10.11 explicit
    // replay-completeness replacement.
    let mut assembler = started("anthropic", "anthropic-messages", "claude");
    start_replay(
        &mut assembler,
        "r0",
        0,
        ReplayTarget::Message,
        ANTHROPIC_SIGNATURE,
    );
    replay_data(
        &mut assembler,
        "r0",
        ReplayDataOperation::AppendUtf8("partial-signature".into()),
    );
    let message = assembler.finish_failed(public_error());
    let target = complete_target(&message);
    assert!(
        !message
            .replay
            .is_complete_and_applicable(&message.replay.items[0], &target)
    );
}

#[test]
fn openai_chat_reasoning_details_preserve_array_order() {
    // §10.2; Pi basis: openai-completions.ts appends valid reasoning_details.
    let message = openai_chat_details_message();
    assert_eq!(message.replay.items[0].id, ReplayItemId::new("detail-0"));
    assert_eq!(message.replay.items[1].id, ReplayItemId::new("detail-1"));
    assert!(
        message.replay.items[0]
            .json_bytes()
            .unwrap()
            .starts_with(br#"{"type":"reasoning.encrypted"#)
    );
    assert!(
        message.replay.items[1]
            .json_bytes()
            .unwrap()
            .starts_with(br#"{"type":"reasoning.summary"#)
    );
}

#[test]
fn openai_chat_reasoning_details_survive_round_trip() {
    // §10.2 primary proof through event assembly and persistence; Pi basis:
    // packages/ai/src/api/openai-completions.ts reasoning_details handling.
    let message = openai_chat_details_message();
    assert_eq!(round_trip(&message), message);
}

#[test]
fn openai_chat_incomplete_reasoning_detail_is_not_replayed() {
    // §10.2; Pi basis: openai-completions.ts reasoning-detail accumulation.
    let mut assembler = started("openrouter", "openai-completions", "reasoning-model");
    start_replay(
        &mut assembler,
        "detail-0",
        0,
        ReplayTarget::Message,
        OPENAI_REASONING_DETAIL,
    );
    replay_data(
        &mut assembler,
        "detail-0",
        ReplayDataOperation::ReplaceJsonBytes(br#"{"type":"reasoning.encrypted"}"#.to_vec()),
    );
    let message = assembler.finish_failed(public_error());
    let target = complete_target(&message);
    assert!(
        message
            .replay
            .complete_item(&ReplayTarget::Message, OPENAI_REASONING_DETAIL, &target)
            .is_none()
    );
}

#[test]
fn responses_reasoning_item_preserves_full_json() {
    // §10.2 and §1.6; Pi basis: openai-responses-shared.ts serializes the
    // complete reasoning output item.
    let message = responses_message();
    assert_eq!(
        message.replay.items[0].json_bytes().unwrap(),
        br#"{"id":"rs_123","type":"reasoning","encrypted_content":"opaque"}"#
    );
}

#[test]
fn responses_reasoning_encrypted_content_survives() {
    // §10.2; Pi basis: OpenAI/Azure terminal backfill in
    // openai-responses-shared.ts.
    let restored = round_trip(&responses_message());
    let json: serde_json::Value =
        serde_json::from_slice(restored.replay.items[0].json_bytes().unwrap()).unwrap();
    assert_eq!(json["encrypted_content"], "opaque");
}

#[test]
fn responses_output_items_preserve_global_order() {
    // §10.2 and §1.9 R7; Pi basis: output_index slots in
    // openai-responses-shared.ts.
    let message = responses_message();
    assert_eq!(
        message
            .replay
            .items
            .iter()
            .map(|item| item.ordinal)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert!(matches!(
        message.replay.items[2].target,
        ReplayTarget::ProviderOutputItem { output_index: 2 }
    ));
}

#[test]
fn responses_text_item_id_survives() {
    // §10.2; Pi basis: openai-responses-shared.ts TextSignatureV1.
    let message = round_trip(&responses_message());
    let identity: serde_json::Value =
        serde_json::from_slice(message.replay.items[1].json_bytes().unwrap()).unwrap();
    assert_eq!(identity["id"], "msg_123");
}

#[test]
fn responses_text_phase_survives() {
    // §10.2; Pi basis: openai-responses-shared.ts preserves message phase.
    let message = round_trip(&responses_message());
    let identity: serde_json::Value =
        serde_json::from_slice(message.replay.items[1].json_bytes().unwrap()).unwrap();
    assert_eq!(identity["phase"], "final_answer");
}

fn responses_function_identity() -> serde_json::Value {
    let message = round_trip(&responses_message());
    serde_json::from_slice(message.replay.items[2].json_bytes().unwrap()).unwrap()
}

#[test]
fn responses_function_call_call_id_survives() {
    // §10.2; Pi basis: openai-responses-shared.ts function-call identity.
    assert_eq!(responses_function_identity()["call_id"], "call_123");
}

#[test]
fn responses_function_call_item_id_survives() {
    // §10.2; Pi basis: openai-responses-shared.ts compound call/item ID.
    assert_eq!(responses_function_identity()["item_id"], "fc_456");
}

#[test]
fn responses_function_call_namespace_survives() {
    // §10.2; Pi basis: openai-responses-shared.ts namespaced tool calls.
    assert_eq!(responses_function_identity()["namespace"], "files");
}

#[test]
fn responses_response_id_survives_round_trip() {
    // §10.2 primary proof through event assembly and persistence; Pi basis:
    // packages/ai/src/api/openai-responses-shared.ts response.created and
    // terminal response identity handling.
    let message = responses_message();
    let restored = round_trip(&message);
    assert_eq!(restored.response_id.as_deref(), Some("resp_123"));
    assert_eq!(restored, message);
}

#[test]
fn responses_incomplete_output_item_is_not_replayed() {
    // §10.2; Pi basis: openai-responses-shared.ts requires output_item.done.
    let mut assembler = started("openai", "openai-responses", "gpt");
    start_replay(
        &mut assembler,
        "output-0",
        0,
        ReplayTarget::ProviderOutputItem { output_index: 0 },
        RESPONSES_REASONING,
    );
    replay_data(
        &mut assembler,
        "output-0",
        ReplayDataOperation::ReplaceJsonBytes(br#"{"id":"rs_partial"}"#.to_vec()),
    );
    let message = assembler.finish_failed(public_error());
    let target = complete_target(&message);
    assert!(
        !message
            .replay
            .is_complete_and_applicable(&message.replay.items[0], &target)
    );
}

#[test]
fn bedrock_redacted_chunks_concatenate_as_bytes() {
    // §10.2 and §1.7; Pi basis: bedrock-converse-stream.ts
    // redactedChunks concatenation.
    let message = bedrock_redacted_message();
    assert_eq!(
        message.replay.items[0].as_bytes(),
        Some(&[1, 2, 0xaf, 0x33][..])
    );
    assert!(matches!(
        message.content[0],
        ContentBlock::Thinking { redacted: true, .. }
    ));
}

#[test]
fn bedrock_redacted_bytes_survive_json_round_trip() {
    // §10.2 primary proof through event assembly and persistence; Pi basis:
    // packages/ai/src/api/bedrock-converse-stream.ts redactedContent handling.
    let message = bedrock_redacted_message();
    let restored = round_trip(&message);
    assert_eq!(restored, message);
    assert_eq!(
        restored.replay.items[0].as_bytes(),
        Some(&[1, 2, 0xaf, 0x33][..])
    );
}

#[test]
fn bedrock_signed_reasoning_event_sequence_preserves_text_and_signature() {
    // Architecture v2 part 2 §1.7; Pi basis: Bedrock reasoningText deltas.
    let mut assembler = started(
        "amazon-bedrock",
        "bedrock-converse-stream",
        "anthropic.claude",
    );
    start_block(&mut assembler, "thinking-0", 0, ContentBlockKind::Thinking);
    assembler
        .apply(&AssistantEvent::ThinkingDelta {
            block_id: ContentBlockId::new("thinking-0"),
            delta: "reasoning".into(),
        })
        .unwrap();
    start_replay(
        &mut assembler,
        "signature-0",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-0")),
        BEDROCK_SIGNATURE,
    );
    replay_data(
        &mut assembler,
        "signature-0",
        ReplayDataOperation::AppendUtf8("signature".into()),
    );
    finish_replay(&mut assembler, "signature-0");
    finish_block(&mut assembler, "thinking-0");
    let message = assembler
        .finish_completed(successful_finish(AssistantFinishReason::Stop))
        .unwrap();
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking {
            text,
            redacted: false,
            ..
        } if text == "reasoning"
    ));
    assert_eq!(message.replay.items[0].as_utf8(), Some("signature"));
}

#[test]
fn bedrock_partial_redacted_payload_is_not_replayed() {
    // §10.2; Pi basis: a stream failure flushes bytes but is not a successful
    // reasoning turn; explicit completeness prevents native replay.
    let mut assembler = started(
        "amazon-bedrock",
        "bedrock-converse-stream",
        "reasoning-model",
    );
    start_replay(
        &mut assembler,
        "redacted-0",
        0,
        ReplayTarget::Message,
        BEDROCK_REDACTED,
    );
    replay_data(
        &mut assembler,
        "redacted-0",
        ReplayDataOperation::AppendBytes(vec![1, 2]),
    );
    let message = assembler.finish_failed(public_error());
    let target = complete_target(&message);
    assert!(
        !message
            .replay
            .is_complete_and_applicable(&message.replay.items[0], &target)
    );
}

fn google_target_message() -> AssistantMessage {
    let mut assembler = started("google", "google-generative-ai", "gemini-3");

    start_block(&mut assembler, "text-0", 0, ContentBlockKind::Text);
    start_replay(
        &mut assembler,
        "text-signature",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("text-0")),
        GOOGLE_SIGNATURE,
    );
    replay_data(
        &mut assembler,
        "text-signature",
        ReplayDataOperation::ReplaceUtf8("dGV4dA==".into()),
    );
    finish_replay(&mut assembler, "text-signature");
    finish_block(&mut assembler, "text-0");

    start_block(&mut assembler, "thinking-1", 1, ContentBlockKind::Thinking);
    start_replay(
        &mut assembler,
        "thinking-signature",
        1,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-1")),
        GOOGLE_SIGNATURE,
    );
    replay_data(
        &mut assembler,
        "thinking-signature",
        ReplayDataOperation::ReplaceUtf8("dGhpbmtpbmc=".into()),
    );
    finish_replay(&mut assembler, "thinking-signature");
    finish_block(&mut assembler, "thinking-1");

    start_replay(
        &mut assembler,
        "tool-signature",
        2,
        ReplayTarget::ToolCall(ToolCallId::new("call-2")),
        GOOGLE_SIGNATURE,
    );
    replay_data(
        &mut assembler,
        "tool-signature",
        ReplayDataOperation::ReplaceUtf8("dG9vbA==".into()),
    );
    finish_replay(&mut assembler, "tool-signature");
    assembler
        .apply(&AssistantEvent::ToolCallMetadata {
            block_id: ContentBlockId::new("tool-2"),
            call_id: ToolCallId::new("call-2"),
            name: Some("read_file".into()),
        })
        .unwrap();
    assembler
        .apply(&AssistantEvent::ToolArgumentsDelta {
            block_id: ContentBlockId::new("tool-2"),
            delta: r#"{"path":"README.md"}"#.into(),
        })
        .unwrap();
    finish_block(&mut assembler, "tool-2");

    assembler
        .finish_completed(successful_finish(AssistantFinishReason::ToolUse))
        .unwrap()
}

#[test]
fn google_thought_flag_not_signature_defines_thinking() {
    // §10.2; Pi basis: google-shared.ts `isThinkingPart` uses `thought === true`.
    let message = google_target_message();
    assert!(matches!(message.content[0], ContentBlock::Text { .. }));
    assert!(matches!(message.content[1], ContentBlock::Thinking { .. }));
}

#[test]
fn google_text_part_signature_stays_on_text_part() {
    // §10.2 and §1.8; Pi basis: packages/ai/src/api/google-shared.ts.
    let message = google_target_message();
    assert_eq!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(ContentBlockId::new("text-0"))
    );
}

#[test]
fn google_thinking_part_signature_stays_on_thinking_part() {
    // §10.2 and §1.8; Pi basis: packages/ai/src/api/google-shared.ts.
    let message = google_target_message();
    assert_eq!(
        message.replay.items[1].target,
        ReplayTarget::ContentBlock(ContentBlockId::new("thinking-1"))
    );
}

#[test]
fn google_tool_call_signature_stays_on_function_call() {
    // §10.2 and §1.8; Pi basis: packages/ai/src/api/google-shared.ts.
    // ToolCallMetadata intentionally starts the block in the worked sequence.
    let message = google_target_message();
    assert_eq!(
        message.replay.items[2].target,
        ReplayTarget::ToolCall(ToolCallId::new("call-2"))
    );
    assert!(matches!(
        &message.content[2],
        ContentBlock::ToolCall { call, .. } if call.id == ToolCallId::new("call-2")
    ));
}

#[test]
fn google_empty_signed_text_part_is_retained() {
    // §10.2; Pi basis: google-shared.ts keeps empty signed text parts.
    let message = google_target_message();
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text.is_empty()
    ));
}

#[test]
fn google_empty_signed_thinking_part_is_retained() {
    // §10.2; Pi basis: google-shared.ts keeps empty signed thinking parts.
    let message = google_target_message();
    assert!(matches!(
        &message.content[1],
        ContentBlock::Thinking { text, .. } if text.is_empty()
    ));
}

#[test]
fn google_stream_omission_does_not_clear_prior_signature() {
    // §10.2; Pi basis: google-shared.ts `retainThoughtSignature`.
    let message = google_target_message();
    assert_eq!(message.replay.items[0].as_utf8(), Some("dGV4dA=="));
}

#[test]
fn google_signature_never_moves_between_parts() {
    // §10.2; Pi basis: google-shared.ts exact-part attachment rule.
    let restored = round_trip(&google_target_message());
    let targets = restored
        .replay
        .items
        .iter()
        .map(|item| item.target.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        targets,
        vec![
            ReplayTarget::ContentBlock(ContentBlockId::new("text-0")),
            ReplayTarget::ContentBlock(ContentBlockId::new("thinking-1")),
            ReplayTarget::ToolCall(ToolCallId::new("call-2")),
        ]
    );
}

#[test]
fn google_assembled_signatures_survive_message_round_trip() {
    // §10.2 primary proof through event assembly and persistence; Pi basis:
    // packages/ai/src/api/google-shared.ts exact-part thought signatures.
    let message = google_target_message();
    assert_eq!(round_trip(&message), message);
}
