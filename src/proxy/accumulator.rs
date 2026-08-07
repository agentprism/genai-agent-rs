//! Strict validation and partial-state reconstruction for compact proxy events.
//!
//! Event order, dense content indexes, block type, and open/closed lifecycles are checked before
//! local state is mutated. Progressive tool-argument snapshots accept no more than 128 nested JSON
//! containers; deeper snapshots use the parser's safe empty-object fallback. Each tool call is
//! limited to 1 MiB of retained raw argument JSON and 4,096 deltas (including empty deltas), and one
//! invocation is limited to 16 MiB of cumulative snapshot-reparse work across all tool calls.
//!
//! A protocol or resource violation emits exactly one in-band terminal error while preserving the
//! partial assistant message; later input is ignored. Local cancellation similarly emits an in-band
//! aborted terminal. These limits do not bound SSE event/text framing or ordinary assistant text, so
//! the proxy endpoint remains a trusted resource boundary.

use super::{ProxyAssistantMessageEvent, ProxyDoneReason, ProxyErrorReason, ProxyUsage};
use crate::{
    AgentToolCall, AgentUsage, AssistantContent, AssistantMessage, AssistantMessageEvent,
    AssistantMessageEventStream, StopReason, parse_streaming_json,
};
use genai::ModelIden;

/// Maximum cumulative raw JSON retained for one in-progress proxy tool call.
const MAX_TOOL_ARGUMENT_RAW_BYTES: usize = 1024 * 1024;
/// Maximum number of deltas accepted for one proxy tool call, including empty deltas.
const MAX_TOOL_ARGUMENT_DELTAS: usize = 4096;
/// Maximum sum of cumulative tool-argument snapshot lengths reparsed across one invocation.
const MAX_TOOL_ARGUMENT_REPARSE_BYTES: usize = 16 * 1024 * 1024;

/// Strict fold state for the compact proxy protocol.
///
/// The wire format deliberately omits partial assistant snapshots. This accumulator reconstructs
/// them while validating every content index and block transition before touching local state.
/// Tool-argument parsing applies the 128-level, 1-MiB/tool, 4,096-delta/tool, and 16-MiB/invocation
/// reparse-work limits described in this module. A violation settles once with partial state intact.
#[derive(Debug)]
pub(crate) struct ProxyAccumulator {
    partial: AssistantMessage,
    started: bool,
    terminal: bool,
    blocks: Vec<BlockState>,
    tool_argument_reparse_bytes: usize,
}

#[derive(Debug)]
enum BlockState {
    Text {
        open: bool,
    },
    Thinking {
        open: bool,
    },
    ToolCall {
        open: bool,
        raw_arguments: String,
        delta_count: usize,
    },
}

impl BlockState {
    fn is_open(&self) -> bool {
        match self {
            Self::Text { open } | Self::Thinking { open } | Self::ToolCall { open, .. } => *open,
        }
    }
}

impl ProxyAccumulator {
    /// Create empty fold state for the model reported by partial and terminal messages.
    pub(crate) fn new(model: ModelIden) -> Self {
        Self {
            partial: AssistantMessage::new(model),
            started: false,
            terminal: false,
            blocks: Vec::new(),
            tool_argument_reparse_bytes: 0,
        }
    }

    /// Return whether the first terminal event or local failure has settled the accumulator.
    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Build a fused assistant stream for a failure detected before SSE folding begins.
    ///
    /// The stream contains a synthesized `Start` followed by one in-band error terminal.
    pub(crate) fn request_error(
        model: ModelIden,
        error: impl Into<String>,
    ) -> AssistantMessageEventStream {
        let mut accumulator = Self::new(model);
        AssistantMessageEventStream::from_events(accumulator.fail(error))
    }

    /// Fold one decoded compact event into the public assistant protocol.
    pub(crate) fn fold(&mut self, event: ProxyAssistantMessageEvent) -> Vec<AssistantMessageEvent> {
        if self.terminal {
            return Vec::new();
        }

        let event_name = proxy_event_name(&event);
        if !matches!(event, ProxyAssistantMessageEvent::Start) && !self.started {
            return self.protocol_error(format!("received {event_name} before start"));
        }

        match event {
            ProxyAssistantMessageEvent::Start => self.fold_start(),
            ProxyAssistantMessageEvent::TextStart { content_index } => {
                self.fold_text_start(content_index)
            }
            ProxyAssistantMessageEvent::TextDelta {
                content_index,
                delta,
            } => self.fold_text_delta(content_index, delta),
            ProxyAssistantMessageEvent::TextEnd {
                content_index,
                content_signature,
            } => self.fold_text_end(content_index, content_signature),
            ProxyAssistantMessageEvent::ThinkingStart { content_index } => {
                self.fold_thinking_start(content_index)
            }
            ProxyAssistantMessageEvent::ThinkingDelta {
                content_index,
                delta,
            } => self.fold_thinking_delta(content_index, delta),
            ProxyAssistantMessageEvent::ThinkingEnd {
                content_index,
                content_signature,
            } => self.fold_thinking_end(content_index, content_signature),
            ProxyAssistantMessageEvent::ToolCallStart {
                content_index,
                id,
                tool_name,
            } => self.fold_tool_call_start(content_index, id, tool_name),
            ProxyAssistantMessageEvent::ToolCallDelta {
                content_index,
                delta,
            } => self.fold_tool_call_delta(content_index, delta),
            ProxyAssistantMessageEvent::ToolCallEnd {
                content_index,
                thought_signatures,
            } => self.fold_tool_call_end(content_index, thought_signatures),
            ProxyAssistantMessageEvent::Done {
                reason,
                usage,
                response_id,
                provider_stop_reason,
            } => self.fold_done(reason, usage, response_id, provider_stop_reason),
            ProxyAssistantMessageEvent::Error {
                reason,
                error_message,
                usage,
                response_id,
                provider_stop_reason,
            } => self.fold_error(
                reason,
                error_message,
                usage,
                response_id,
                provider_stop_reason,
            ),
        }
    }

    /// Terminate with a local transport or protocol error while preserving accumulated content.
    pub(crate) fn fail(&mut self, error: impl Into<String>) -> Vec<AssistantMessageEvent> {
        self.terminal_error(StopReason::Error, Some(error.into()))
    }

    /// Terminate when the invocation's local cancellation token fires.
    pub(crate) fn abort(&mut self) -> Vec<AssistantMessageEvent> {
        self.terminal_error(
            StopReason::Aborted,
            Some("Request aborted by user".to_owned()),
        )
    }

    /// A successful HTTP body must contain exactly one compact terminal event.
    pub(crate) fn finish_without_terminal(&mut self) -> Vec<AssistantMessageEvent> {
        self.fail("Proxy SSE stream ended without a terminal event")
    }

    fn fold_start(&mut self) -> Vec<AssistantMessageEvent> {
        if self.started {
            return self.protocol_error("received duplicate start");
        }
        self.started = true;
        vec![AssistantMessageEvent::Start {
            partial: self.partial.clone(),
        }]
    }

    fn fold_text_start(&mut self, content_index: u32) -> Vec<AssistantMessageEvent> {
        let index = match self.next_content_index("text_start", content_index) {
            Ok(index) => index,
            Err(events) => return events,
        };
        self.partial.content.push(AssistantContent::text(""));
        self.blocks.push(BlockState::Text { open: true });
        vec![AssistantMessageEvent::TextStart {
            content_index: index,
            partial: self.partial.clone(),
        }]
    }

    fn fold_text_delta(&mut self, content_index: u32, delta: String) -> Vec<AssistantMessageEvent> {
        let index = content_index as usize;
        if !matches!(
            self.blocks.get(index),
            Some(BlockState::Text { open: true })
        ) {
            return self.protocol_error(format!(
                "received text_delta for contentIndex {content_index} without an open text block"
            ));
        }
        let Some(AssistantContent::Text { text, .. }) = self.partial.content.get_mut(index) else {
            return self.internal_state_error("text_delta", content_index);
        };
        text.push_str(&delta);
        vec![AssistantMessageEvent::TextDelta {
            content_index: index,
            delta,
            partial: self.partial.clone(),
        }]
    }

    fn fold_text_end(
        &mut self,
        content_index: u32,
        content_signature: Option<String>,
    ) -> Vec<AssistantMessageEvent> {
        let index = content_index as usize;
        if !matches!(
            self.blocks.get(index),
            Some(BlockState::Text { open: true })
        ) {
            return self.protocol_error(format!(
                "received text_end for contentIndex {content_index} without an open text block"
            ));
        }
        let Some(AssistantContent::Text { text, signature }) = self.partial.content.get_mut(index)
        else {
            return self.internal_state_error("text_end", content_index);
        };
        *signature = content_signature;
        let content = text.clone();
        if let Some(BlockState::Text { open }) = self.blocks.get_mut(index) {
            *open = false;
        }
        vec![AssistantMessageEvent::TextEnd {
            content_index: index,
            content,
            partial: self.partial.clone(),
        }]
    }

    fn fold_thinking_start(&mut self, content_index: u32) -> Vec<AssistantMessageEvent> {
        let index = match self.next_content_index("thinking_start", content_index) {
            Ok(index) => index,
            Err(events) => return events,
        };
        self.partial.content.push(AssistantContent::thinking(""));
        self.blocks.push(BlockState::Thinking { open: true });
        vec![AssistantMessageEvent::ThinkingStart {
            content_index: index,
            partial: self.partial.clone(),
        }]
    }

    fn fold_thinking_delta(
        &mut self,
        content_index: u32,
        delta: String,
    ) -> Vec<AssistantMessageEvent> {
        let index = content_index as usize;
        if !matches!(
            self.blocks.get(index),
            Some(BlockState::Thinking { open: true })
        ) {
            return self.protocol_error(format!(
                "received thinking_delta for contentIndex {content_index} without an open thinking block"
            ));
        }
        let Some(AssistantContent::Thinking { thinking, .. }) = self.partial.content.get_mut(index)
        else {
            return self.internal_state_error("thinking_delta", content_index);
        };
        thinking.push_str(&delta);
        vec![AssistantMessageEvent::ThinkingDelta {
            content_index: index,
            delta,
            partial: self.partial.clone(),
        }]
    }

    fn fold_thinking_end(
        &mut self,
        content_index: u32,
        content_signature: Option<String>,
    ) -> Vec<AssistantMessageEvent> {
        let index = content_index as usize;
        if !matches!(
            self.blocks.get(index),
            Some(BlockState::Thinking { open: true })
        ) {
            return self.protocol_error(format!(
                "received thinking_end for contentIndex {content_index} without an open thinking block"
            ));
        }
        let Some(AssistantContent::Thinking {
            thinking,
            signature,
        }) = self.partial.content.get_mut(index)
        else {
            return self.internal_state_error("thinking_end", content_index);
        };
        *signature = content_signature;
        let thinking = thinking.clone();
        if let Some(BlockState::Thinking { open }) = self.blocks.get_mut(index) {
            *open = false;
        }
        vec![AssistantMessageEvent::ThinkingEnd {
            content_index: index,
            thinking,
            partial: self.partial.clone(),
        }]
    }

    fn fold_tool_call_start(
        &mut self,
        content_index: u32,
        id: String,
        tool_name: String,
    ) -> Vec<AssistantMessageEvent> {
        let index = match self.next_content_index("toolcall_start", content_index) {
            Ok(index) => index,
            Err(events) => return events,
        };
        self.partial
            .content
            .push(AssistantContent::ToolCall(AgentToolCall::new(
                id,
                tool_name,
                parse_streaming_json(""),
            )));
        self.blocks.push(BlockState::ToolCall {
            open: true,
            raw_arguments: String::new(),
            delta_count: 0,
        });
        vec![AssistantMessageEvent::ToolCallStart {
            content_index: index,
            partial: self.partial.clone(),
        }]
    }

    fn fold_tool_call_delta(
        &mut self,
        content_index: u32,
        delta: String,
    ) -> Vec<AssistantMessageEvent> {
        let index = content_index as usize;
        let (current_raw_bytes, current_delta_count) = match self.blocks.get(index) {
            Some(BlockState::ToolCall {
                open: true,
                raw_arguments,
                delta_count,
            }) => (raw_arguments.len(), *delta_count),
            _ => {
                return self.protocol_error(format!(
                    "received toolcall_delta for contentIndex {content_index} without an open tool-call block"
                ));
            }
        };
        if !matches!(
            self.partial.content.get(index),
            Some(AssistantContent::ToolCall(_))
        ) {
            return self.internal_state_error("toolcall_delta", content_index);
        }

        let Some(next_delta_count) = current_delta_count.checked_add(1) else {
            return self.protocol_error(format!(
                "tool-call argument delta count exceeded limit of {MAX_TOOL_ARGUMENT_DELTAS} for contentIndex {content_index}"
            ));
        };
        if next_delta_count > MAX_TOOL_ARGUMENT_DELTAS {
            return self.protocol_error(format!(
                "tool-call argument delta count exceeded limit of {MAX_TOOL_ARGUMENT_DELTAS} for contentIndex {content_index}"
            ));
        }

        let Some(next_raw_bytes) = current_raw_bytes.checked_add(delta.len()) else {
            return self.protocol_error(format!(
                "tool-call argument raw JSON exceeded limit of {MAX_TOOL_ARGUMENT_RAW_BYTES} bytes for contentIndex {content_index}"
            ));
        };
        if next_raw_bytes > MAX_TOOL_ARGUMENT_RAW_BYTES {
            return self.protocol_error(format!(
                "tool-call argument raw JSON exceeded limit of {MAX_TOOL_ARGUMENT_RAW_BYTES} bytes for contentIndex {content_index}"
            ));
        }

        let Some(next_reparse_bytes) = self.tool_argument_reparse_bytes.checked_add(next_raw_bytes)
        else {
            return self.protocol_error(format!(
                "tool-call argument cumulative reparse work exceeded invocation limit of {MAX_TOOL_ARGUMENT_REPARSE_BYTES} bytes"
            ));
        };
        if next_reparse_bytes > MAX_TOOL_ARGUMENT_REPARSE_BYTES {
            return self.protocol_error(format!(
                "tool-call argument cumulative reparse work exceeded invocation limit of {MAX_TOOL_ARGUMENT_REPARSE_BYTES} bytes"
            ));
        }

        let raw_arguments = match self.blocks.get_mut(index) {
            Some(BlockState::ToolCall {
                open: true,
                raw_arguments,
                delta_count,
            }) => {
                raw_arguments.push_str(&delta);
                *delta_count = next_delta_count;
                raw_arguments.clone()
            }
            _ => return self.internal_state_error("toolcall_delta", content_index),
        };
        self.tool_argument_reparse_bytes = next_reparse_bytes;
        let Some(AssistantContent::ToolCall(tool_call)) = self.partial.content.get_mut(index)
        else {
            return self.internal_state_error("toolcall_delta", content_index);
        };
        tool_call.arguments = parse_streaming_json(&raw_arguments);
        vec![AssistantMessageEvent::ToolCallDelta {
            content_index: index,
            delta,
            partial: self.partial.clone(),
        }]
    }

    fn fold_tool_call_end(
        &mut self,
        content_index: u32,
        thought_signatures: Vec<String>,
    ) -> Vec<AssistantMessageEvent> {
        let index = content_index as usize;
        if !matches!(
            self.blocks.get(index),
            Some(BlockState::ToolCall { open: true, .. })
        ) {
            return self.protocol_error(format!(
                "received toolcall_end for contentIndex {content_index} without an open tool-call block"
            ));
        }
        let Some(AssistantContent::ToolCall(tool_call)) = self.partial.content.get_mut(index)
        else {
            return self.internal_state_error("toolcall_end", content_index);
        };
        tool_call.thought_signatures = thought_signatures;
        let tool_call = tool_call.clone();
        if let Some(BlockState::ToolCall { open, .. }) = self.blocks.get_mut(index) {
            *open = false;
        }
        vec![AssistantMessageEvent::ToolCallEnd {
            content_index: index,
            tool_call,
            partial: self.partial.clone(),
        }]
    }

    fn fold_done(
        &mut self,
        reason: ProxyDoneReason,
        usage: ProxyUsage,
        response_id: Option<String>,
        provider_stop_reason: Option<String>,
    ) -> Vec<AssistantMessageEvent> {
        if let Some(index) = self.blocks.iter().position(BlockState::is_open) {
            return self.protocol_error(format!(
                "received done while content block {index} is still open"
            ));
        }

        let reason = match reason {
            ProxyDoneReason::Stop => StopReason::Stop,
            ProxyDoneReason::Length => StopReason::Length,
            ProxyDoneReason::ToolUse => StopReason::ToolUse,
        };
        self.partial.stop_reason = reason;
        self.partial.error_message = None;
        self.partial.usage = agent_usage(usage);
        self.partial.response_id = response_id;
        self.partial.provider_stop_reason = provider_stop_reason;
        self.terminal = true;
        vec![AssistantMessageEvent::Done {
            reason,
            message: self.partial.clone(),
        }]
    }

    fn fold_error(
        &mut self,
        reason: ProxyErrorReason,
        error_message: Option<String>,
        usage: ProxyUsage,
        response_id: Option<String>,
        provider_stop_reason: Option<String>,
    ) -> Vec<AssistantMessageEvent> {
        let reason = match reason {
            ProxyErrorReason::Error => StopReason::Error,
            ProxyErrorReason::Aborted => StopReason::Aborted,
        };
        self.partial.stop_reason = reason;
        self.partial.error_message = error_message;
        self.partial.usage = agent_usage(usage);
        self.partial.response_id = response_id;
        self.partial.provider_stop_reason = provider_stop_reason;
        self.terminal = true;
        vec![AssistantMessageEvent::Error {
            reason,
            error: self.partial.clone(),
        }]
    }

    /// Validate that a block-start appends the next dense content slot.
    fn next_content_index(
        &mut self,
        event: &str,
        content_index: u32,
    ) -> Result<usize, Vec<AssistantMessageEvent>> {
        let index = content_index as usize;
        let expected = self.partial.content.len();
        if index != expected {
            return Err(self.protocol_error(format!(
                "received {event} for contentIndex {content_index}; expected {expected}"
            )));
        }
        Ok(index)
    }

    fn internal_state_error(
        &mut self,
        event: &str,
        content_index: u32,
    ) -> Vec<AssistantMessageEvent> {
        self.protocol_error(format!(
            "received {event} for inconsistent contentIndex {content_index}"
        ))
    }

    fn protocol_error(&mut self, detail: impl Into<String>) -> Vec<AssistantMessageEvent> {
        self.fail(format!("Proxy protocol error: {}", detail.into()))
    }

    fn ensure_started(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        if !self.started {
            self.started = true;
            events.push(AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            });
        }
    }

    fn terminal_error(
        &mut self,
        reason: StopReason,
        error_message: Option<String>,
    ) -> Vec<AssistantMessageEvent> {
        if self.terminal {
            return Vec::new();
        }
        let mut events = Vec::new();
        self.ensure_started(&mut events);
        self.partial.stop_reason = reason;
        self.partial.error_message = error_message;
        self.terminal = true;
        events.push(AssistantMessageEvent::Error {
            reason,
            error: self.partial.clone(),
        });
        events
    }
}

fn agent_usage(usage: ProxyUsage) -> AgentUsage {
    AgentUsage {
        input_tokens: usage.input,
        output_tokens: usage.output,
        cache_read_tokens: usage.cache_read,
        cache_write_tokens: usage.cache_write,
        total_tokens: usage.total_tokens,
    }
}

fn proxy_event_name(event: &ProxyAssistantMessageEvent) -> &'static str {
    match event {
        ProxyAssistantMessageEvent::Start => "start",
        ProxyAssistantMessageEvent::TextStart { .. } => "text_start",
        ProxyAssistantMessageEvent::TextDelta { .. } => "text_delta",
        ProxyAssistantMessageEvent::TextEnd { .. } => "text_end",
        ProxyAssistantMessageEvent::ThinkingStart { .. } => "thinking_start",
        ProxyAssistantMessageEvent::ThinkingDelta { .. } => "thinking_delta",
        ProxyAssistantMessageEvent::ThinkingEnd { .. } => "thinking_end",
        ProxyAssistantMessageEvent::ToolCallStart { .. } => "toolcall_start",
        ProxyAssistantMessageEvent::ToolCallDelta { .. } => "toolcall_delta",
        ProxyAssistantMessageEvent::ToolCallEnd { .. } => "toolcall_end",
        ProxyAssistantMessageEvent::Done { .. } => "done",
        ProxyAssistantMessageEvent::Error { .. } => "error",
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use genai::adapter::AdapterKind;

    fn accumulator() -> ProxyAccumulator {
        ProxyAccumulator::new(ModelIden::new(AdapterKind::OpenAI, "gpt-4o"))
    }

    fn start_tool(accumulator: &mut ProxyAccumulator, content_index: u32) {
        let events = accumulator.fold(ProxyAssistantMessageEvent::ToolCallStart {
            content_index,
            id: format!("call-{content_index}"),
            tool_name: "bounded_tool".to_owned(),
        });
        assert!(matches!(
            events.as_slice(),
            [AssistantMessageEvent::ToolCallStart { .. }]
        ));
    }

    fn assert_terminal_resource_error(events: &[AssistantMessageEvent], expected: &str) {
        let message = match events {
            [AssistantMessageEvent::Error { error, .. }] => error
                .error_message
                .as_deref()
                .expect("resource error must include a diagnostic"),
            other => panic!(
                "expected exactly one terminal Error, got {} event(s)",
                other.len()
            ),
        };
        assert!(
            message.contains(expected),
            "resource diagnostic did not mention {expected:?}: {message}"
        );
    }

    #[test]
    fn empty_tool_deltas_count_toward_the_per_tool_limit() {
        let mut accumulator = accumulator();
        assert!(matches!(
            accumulator
                .fold(ProxyAssistantMessageEvent::Start)
                .as_slice(),
            [AssistantMessageEvent::Start { .. }]
        ));
        start_tool(&mut accumulator, 0);

        for _ in 0..4096 {
            let events = accumulator.fold(ProxyAssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: String::new(),
            });
            assert!(matches!(
                events.as_slice(),
                [AssistantMessageEvent::ToolCallDelta { .. }]
            ));
        }
        let events = accumulator.fold(ProxyAssistantMessageEvent::ToolCallDelta {
            content_index: 0,
            delta: String::new(),
        });
        assert_terminal_resource_error(&events, "delta count");
        assert!(accumulator.is_terminal());
        assert!(
            accumulator
                .fold(ProxyAssistantMessageEvent::ToolCallDelta {
                    content_index: 0,
                    delta: String::new(),
                })
                .is_empty(),
            "the resource violation must settle exactly once"
        );
    }

    #[test]
    fn cumulative_reparse_work_is_bounded_across_tool_blocks() {
        const CHUNK_BYTES: usize = 32 * 1024;
        const SECOND_TOOL_BYTES: usize = 512 * 1024 + 1;

        let mut accumulator = accumulator();
        accumulator.fold(ProxyAssistantMessageEvent::Start);
        start_tool(&mut accumulator, 0);
        for _ in 0..31 {
            let events = accumulator.fold(ProxyAssistantMessageEvent::ToolCallDelta {
                content_index: 0,
                delta: " ".repeat(CHUNK_BYTES),
            });
            assert!(matches!(
                events.as_slice(),
                [AssistantMessageEvent::ToolCallDelta { .. }]
            ));
        }
        assert!(matches!(
            accumulator
                .fold(ProxyAssistantMessageEvent::ToolCallEnd {
                    content_index: 0,
                    thought_signatures: Vec::new(),
                })
                .as_slice(),
            [AssistantMessageEvent::ToolCallEnd { .. }]
        ));

        start_tool(&mut accumulator, 1);
        let events = accumulator.fold(ProxyAssistantMessageEvent::ToolCallDelta {
            content_index: 1,
            delta: " ".repeat(SECOND_TOOL_BYTES),
        });
        assert_terminal_resource_error(&events, "cumulative reparse work");
    }
}
