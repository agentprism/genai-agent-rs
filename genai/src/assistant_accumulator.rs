//! Translation from `genai` chat-stream events into the assistant event protocol.
//!
//! [`AssistantAccumulator`] is a pure fold state: it incrementally builds message snapshots,
//! reconciles them with the provider's authoritative `StreamEnd` capture, and converts upstream
//! failures and cancellation into in-band terminal events.

use crate::{AgentToolCall, AgentUsage, AssistantContent, AssistantMessage, AssistantMessageEvent};
use genai::ModelIden;
use genai::chat::{ChatStreamEvent, ContentPart, StreamEnd};
use serde_json::{Map, Number, Value};
use std::fmt::Display;

/// Pure fold state for translating `genai` chat-stream events into the assistant protocol.
///
/// Text and reasoning chunks open one block of their respective kind and emit deltas against the
/// current snapshot. Tool snapshots are correlated by call id (or an unambiguous cumulative
/// prefix), and their incomplete JSON arguments are parsed on a best-effort basis. At `StreamEnd`,
/// captured content replaces incremental approximations, open blocks receive matching end events,
/// captured-only blocks are synthesized as complete event sequences, and one terminal
/// [`AssistantMessageEvent::Done`] is emitted.
///
/// `ChatStream` itself cannot be constructed outside `genai`, so keeping this state machine
/// independent of the stream makes provider edge cases testable with ordinary event values. Once
/// terminal, every further fold or terminal request is an idempotent no-op.
#[derive(Debug)]
pub struct AssistantAccumulator {
    partial: AssistantMessage,
    started: bool,
    terminal: bool,
    text_index: Option<usize>,
    thinking_index: Option<usize>,
    tool_states: Vec<ToolState>,
    pending_thought_signature: String,
}

#[derive(Debug)]
struct ToolState {
    content_index: usize,
    raw_arguments: String,
    open: bool,
}

impl AssistantAccumulator {
    /// Create empty fold state for a response from `model`.
    pub fn new(model: ModelIden) -> Self {
        Self {
            partial: AssistantMessage::new(model),
            started: false,
            terminal: false,
            text_index: None,
            thinking_index: None,
            tool_states: Vec::new(),
            pending_thought_signature: String::new(),
        }
    }

    /// Borrow the latest accumulated message snapshot.
    ///
    /// Before successful completion its stop reason is pending. After completion, failure, or
    /// cancellation this is the same authoritative state cloned into the terminal event.
    pub fn partial(&self) -> &AssistantMessage {
        &self.partial
    }

    /// Return whether a `Done` or `Error` event has already been emitted.
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Fold one successful upstream event into zero or more assistant events.
    ///
    /// `Start` is emitted at most once and is synthesized before the first content or terminal
    /// update when the provider omits it. Text, reasoning, and tool-call chunks map to their
    /// corresponding start/delta events; heartbeats map to no event. `StreamEnd` reconciles final
    /// capture data, emits end events in content-index order, then emits `Done`.
    ///
    /// Calling this after terminal settlement returns an empty vector.
    pub fn fold(&mut self, event: ChatStreamEvent) -> Vec<AssistantMessageEvent> {
        if self.terminal {
            return Vec::new();
        }

        match event {
            ChatStreamEvent::Start => {
                let mut events = Vec::new();
                self.ensure_started(&mut events);
                events
            }
            ChatStreamEvent::Chunk(chunk) => self.fold_text(chunk.content),
            ChatStreamEvent::ReasoningChunk(chunk) => self.fold_thinking(chunk.content),
            ChatStreamEvent::ThoughtSignatureChunk(chunk) => {
                self.fold_thought_signature(chunk.content)
            }
            ChatStreamEvent::ToolCallChunk(chunk) => self.fold_tool_call(chunk.tool_call),
            ChatStreamEvent::Heartbeat => Vec::new(),
            ChatStreamEvent::End(end) => self.fold_end(end),
        }
    }

    /// Fold an upstream stream item, converting a displayable error into an in-band terminal
    /// [`AssistantMessageEvent::Error`] that retains accumulated content.
    pub fn fold_result<E>(
        &mut self,
        result: Result<ChatStreamEvent, E>,
    ) -> Vec<AssistantMessageEvent>
    where
        E: Display,
    {
        match result {
            Ok(event) => self.fold(event),
            Err(error) => self.fail(error.to_string()),
        }
    }

    /// Terminate with a provider/runtime error while retaining all partial content.
    ///
    /// This synthesizes `Start` first if necessary and is a no-op after terminal settlement.
    pub fn fail(&mut self, error: impl Into<String>) -> Vec<AssistantMessageEvent> {
        self.terminal_error(crate::StopReason::Error, error.into())
    }

    /// Terminate in-band when the invocation's cancellation token fires.
    ///
    /// The resulting terminal message uses [`crate::StopReason::Aborted`] and retains partial
    /// content. This is a no-op after terminal settlement.
    pub fn abort(&mut self) -> Vec<AssistantMessageEvent> {
        self.terminal_error(
            crate::StopReason::Aborted,
            "Request aborted by user".to_string(),
        )
    }

    /// Convert an upstream close without `StreamEnd` into the in-band, never-throw error
    /// contract while retaining partial content.
    pub fn finish_without_end(&mut self) -> Vec<AssistantMessageEvent> {
        self.fail("genai chat stream ended without an End event")
    }

    fn ensure_started(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        if !self.started {
            self.started = true;
            events.push(AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            });
        }
    }

    fn fold_text(&mut self, delta: String) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        self.ensure_started(&mut events);

        let content_index = match self.text_index {
            Some(index) => index,
            None => {
                let index = self.partial.content.len();
                self.partial.content.push(AssistantContent::text(""));
                self.text_index = Some(index);
                events.push(AssistantMessageEvent::TextStart {
                    content_index: index as u32,
                    partial: self.partial.clone(),
                });
                index
            }
        };

        if let Some(AssistantContent::Text { text, .. }) =
            self.partial.content.get_mut(content_index)
        {
            text.push_str(&delta);
        }
        events.push(AssistantMessageEvent::TextDelta {
            content_index: content_index as u32,
            delta,
            partial: self.partial.clone(),
        });
        events
    }

    fn fold_thinking(&mut self, delta: String) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        self.ensure_started(&mut events);

        let content_index = match self.thinking_index {
            Some(index) => index,
            None => {
                let index = self.partial.content.len();
                let signature = (!self.pending_thought_signature.is_empty())
                    .then(|| std::mem::take(&mut self.pending_thought_signature));
                self.partial.content.push(AssistantContent::Thinking {
                    thinking: String::new(),
                    signature,
                });
                self.thinking_index = Some(index);
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: index as u32,
                    partial: self.partial.clone(),
                });
                index
            }
        };

        if let Some(AssistantContent::Thinking { thinking, .. }) =
            self.partial.content.get_mut(content_index)
        {
            thinking.push_str(&delta);
        }
        events.push(AssistantMessageEvent::ThinkingDelta {
            content_index: content_index as u32,
            delta,
            partial: self.partial.clone(),
        });
        events
    }

    fn fold_thought_signature(&mut self, signature_chunk: String) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        self.ensure_started(&mut events);

        if let Some(index) = self.thinking_index
            && let Some(AssistantContent::Thinking { signature, .. }) =
                self.partial.content.get_mut(index)
        {
            signature
                .get_or_insert_with(String::new)
                .push_str(&signature_chunk);
        } else if let Some(first_tool) = self.tool_states.first() {
            if let Some(AssistantContent::ToolCall(call)) =
                self.partial.content.get_mut(first_tool.content_index)
            {
                if let Some(last) = call.thought_signatures.last_mut() {
                    last.push_str(&signature_chunk);
                } else {
                    call.thought_signatures.push(signature_chunk);
                }
            }
        } else {
            self.pending_thought_signature.push_str(&signature_chunk);
        }

        events
    }

    fn fold_tool_call(&mut self, tool_call: genai::chat::ToolCall) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        self.ensure_started(&mut events);

        let raw_arguments = raw_tool_arguments(&tool_call.fn_arguments);
        let existing = self.find_tool_state(&tool_call, &raw_arguments);

        if let Some(state_index) = existing {
            let content_index = self.tool_states[state_index].content_index;
            let previous_raw = self.tool_states[state_index].raw_arguments.clone();
            let delta = cumulative_delta(&previous_raw, &raw_arguments);
            self.tool_states[state_index].raw_arguments = raw_arguments.clone();

            if let Some(AssistantContent::ToolCall(call)) =
                self.partial.content.get_mut(content_index)
            {
                if call.id.is_empty() && !tool_call.call_id.is_empty() {
                    call.id = tool_call.call_id;
                }
                if !tool_call.fn_name.is_empty() {
                    call.name = tool_call.fn_name;
                }
                call.arguments = streamed_arguments(&tool_call.fn_arguments);
                if let Some(signatures) = tool_call.thought_signatures
                    && !signatures.is_empty()
                {
                    call.thought_signatures = signatures;
                }
            }

            if let Some(delta) = delta {
                events.push(AssistantMessageEvent::ToolCallDelta {
                    content_index: content_index as u32,
                    delta,
                    partial: self.partial.clone(),
                });
            }
            return events;
        }

        let content_index = self.partial.content.len();
        let mut call = AgentToolCall {
            id: tool_call.call_id,
            name: tool_call.fn_name,
            arguments: streamed_arguments(&tool_call.fn_arguments),
            namespace: None,
            thought_signatures: tool_call.thought_signatures.unwrap_or_default(),
        };
        if self.thinking_index.is_none()
            && self.tool_states.is_empty()
            && !self.pending_thought_signature.is_empty()
        {
            call.thought_signatures
                .push(std::mem::take(&mut self.pending_thought_signature));
        }

        self.partial.content.push(AssistantContent::ToolCall(call));
        self.tool_states.push(ToolState {
            content_index,
            raw_arguments: raw_arguments.clone(),
            open: true,
        });
        events.push(AssistantMessageEvent::ToolCallStart {
            content_index: content_index as u32,
            partial: self.partial.clone(),
        });
        if !raw_arguments.is_empty() {
            events.push(AssistantMessageEvent::ToolCallDelta {
                content_index: content_index as u32,
                delta: raw_arguments,
                partial: self.partial.clone(),
            });
        }
        events
    }

    fn find_tool_state(
        &self,
        incoming: &genai::chat::ToolCall,
        incoming_raw: &str,
    ) -> Option<usize> {
        if !incoming.call_id.is_empty()
            && let Some(index) = self.tool_states.iter().position(|state| {
                matches!(
                    self.partial.content.get(state.content_index),
                    Some(AssistantContent::ToolCall(call)) if call.id == incoming.call_id
                )
            })
        {
            return Some(index);
        }

        // A few OpenAI-compatible endpoints omit the id on later snapshots. In that case a
        // cumulative-prefix match is the least ambiguous identity available in ChatStreamEvent.
        let mut candidates = self
            .tool_states
            .iter()
            .enumerate()
            .filter(|(_, state)| {
                matches!(
                    self.partial.content.get(state.content_index),
                    Some(AssistantContent::ToolCall(call))
                        if (incoming.fn_name.is_empty() || call.name == incoming.fn_name)
                            && incoming_raw.starts_with(&state.raw_arguments)
                )
            })
            .map(|(index, _)| index);
        let first = candidates.next()?;
        candidates.next().is_none().then_some(first)
    }

    fn fold_end(&mut self, end: StreamEnd) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        self.ensure_started(&mut events);

        let FinalCapture {
            reasoning,
            texts,
            tools,
            provider_stop_reason,
            stop_reason,
            usage,
            response_id,
        } = FinalCapture::from_end(end, &self.partial, &self.pending_thought_signature);

        if let Some(usage) = usage {
            self.partial.usage = usage;
        }
        self.partial.provider_stop_reason = provider_stop_reason;
        self.partial.response_id = response_id;

        let mut matched_reasoning = vec![false; reasoning.len()];
        let mut matched_texts = vec![false; texts.len()];
        let mut matched_tools = vec![false; tools.len()];

        if let Some(index) = self.thinking_index
            && let Some(final_index) = (!reasoning.is_empty()).then_some(0)
        {
            self.partial.content[index] = reasoning[final_index].clone();
            matched_reasoning[final_index] = true;
        }
        if let Some(index) = self.text_index
            && let Some(final_index) = (!texts.is_empty()).then_some(0)
        {
            self.partial.content[index] = texts[final_index].clone();
            matched_texts[final_index] = true;
        }

        for state in &self.tool_states {
            let current_call = match self.partial.content.get(state.content_index) {
                Some(AssistantContent::ToolCall(call)) => call,
                _ => continue,
            };
            let final_index = tools
                .iter()
                .enumerate()
                .find(|(index, part)| {
                    !matched_tools[*index]
                        && matches!(part, AssistantContent::ToolCall(call) if !call.id.is_empty() && call.id == current_call.id)
                })
                .map(|(index, _)| index)
                .or_else(|| matched_tools.iter().position(|matched| !*matched));
            if let Some(final_index) = final_index {
                self.partial.content[state.content_index] = tools[final_index].clone();
                matched_tools[final_index] = true;
            }
        }

        let mut close_indices = Vec::new();
        if let Some(index) = self.thinking_index.take() {
            close_indices.push((index, CloseKind::Thinking));
        }
        if let Some(index) = self.text_index.take() {
            close_indices.push((index, CloseKind::Text));
        }
        for state in &mut self.tool_states {
            if state.open {
                state.open = false;
                close_indices.push((state.content_index, CloseKind::ToolCall));
            }
        }
        close_indices.sort_by_key(|(index, _)| *index);

        for (content_index, kind) in close_indices {
            match (kind, self.partial.content.get(content_index)) {
                (CloseKind::Text, Some(AssistantContent::Text { text, .. })) => {
                    events.push(AssistantMessageEvent::TextEnd {
                        content_index: content_index as u32,
                        content: text.clone(),
                        partial: self.partial.clone(),
                    });
                }
                (CloseKind::Thinking, Some(AssistantContent::Thinking { thinking, .. })) => {
                    events.push(AssistantMessageEvent::ThinkingEnd {
                        content_index: content_index as u32,
                        thinking: thinking.clone(),
                        partial: self.partial.clone(),
                    });
                }
                (CloseKind::ToolCall, Some(AssistantContent::ToolCall(call))) => {
                    events.push(AssistantMessageEvent::ToolCallEnd {
                        content_index: content_index as u32,
                        tool_call: call.clone(),
                        partial: self.partial.clone(),
                    });
                }
                _ => {}
            }
        }

        for (index, part) in reasoning.iter().enumerate() {
            if !matched_reasoning[index] {
                self.emit_captured_only_part(part.clone(), &mut events);
            }
        }
        for (index, part) in texts.iter().enumerate() {
            if !matched_texts[index] {
                self.emit_captured_only_part(part.clone(), &mut events);
            }
        }
        for (index, part) in tools.iter().enumerate() {
            if !matched_tools[index] {
                self.emit_captured_only_part(part.clone(), &mut events);
            }
        }

        self.partial.content = reasoning.into_iter().chain(texts).chain(tools).collect();
        self.partial.stop_reason = stop_reason;
        self.partial.error_message = None;
        self.terminal = true;
        events.push(AssistantMessageEvent::Done {
            reason: stop_reason,
            message: self.partial.clone(),
        });
        events
    }

    fn emit_captured_only_part(
        &mut self,
        final_part: AssistantContent,
        events: &mut Vec<AssistantMessageEvent>,
    ) {
        let content_index = self.partial.content.len();
        match final_part {
            AssistantContent::Text { text, signature } => {
                self.partial.content.push(AssistantContent::Text {
                    text: String::new(),
                    signature: signature.clone(),
                });
                events.push(AssistantMessageEvent::TextStart {
                    content_index: content_index as u32,
                    partial: self.partial.clone(),
                });
                if !text.is_empty() {
                    if let Some(AssistantContent::Text { text: partial, .. }) =
                        self.partial.content.get_mut(content_index)
                    {
                        *partial = text.clone();
                    }
                    events.push(AssistantMessageEvent::TextDelta {
                        content_index: content_index as u32,
                        delta: text.clone(),
                        partial: self.partial.clone(),
                    });
                }
                events.push(AssistantMessageEvent::TextEnd {
                    content_index: content_index as u32,
                    content: text,
                    partial: self.partial.clone(),
                });
            }
            AssistantContent::Thinking {
                thinking,
                signature,
            } => {
                self.partial.content.push(AssistantContent::Thinking {
                    thinking: String::new(),
                    signature: signature.clone(),
                });
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: content_index as u32,
                    partial: self.partial.clone(),
                });
                if !thinking.is_empty() {
                    if let Some(AssistantContent::Thinking {
                        thinking: partial, ..
                    }) = self.partial.content.get_mut(content_index)
                    {
                        *partial = thinking.clone();
                    }
                    events.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: content_index as u32,
                        delta: thinking.clone(),
                        partial: self.partial.clone(),
                    });
                }
                events.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: content_index as u32,
                    thinking,
                    partial: self.partial.clone(),
                });
            }
            AssistantContent::ToolCall(call) => {
                let mut partial_call = call.clone();
                partial_call.arguments = Value::Object(Map::new());
                self.partial
                    .content
                    .push(AssistantContent::ToolCall(partial_call));
                events.push(AssistantMessageEvent::ToolCallStart {
                    content_index: content_index as u32,
                    partial: self.partial.clone(),
                });
                let delta = serde_json::to_string(&call.arguments).unwrap_or_default();
                self.partial.content[content_index] = AssistantContent::ToolCall(call.clone());
                if !delta.is_empty() {
                    events.push(AssistantMessageEvent::ToolCallDelta {
                        content_index: content_index as u32,
                        delta,
                        partial: self.partial.clone(),
                    });
                }
                events.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: content_index as u32,
                    tool_call: call,
                    partial: self.partial.clone(),
                });
            }
        }
    }

    fn terminal_error(
        &mut self,
        reason: crate::StopReason,
        error_message: String,
    ) -> Vec<AssistantMessageEvent> {
        if self.terminal {
            return Vec::new();
        }
        let mut events = Vec::new();
        self.ensure_started(&mut events);
        self.partial.stop_reason = reason;
        self.partial.error_message = Some(error_message);
        self.terminal = true;
        events.push(AssistantMessageEvent::Error {
            reason,
            error: self.partial.clone(),
        });
        events
    }
}

/// Backward-friendly explicit name for callers that prefer the full protocol type name.
pub type AssistantMessageAccumulator = AssistantAccumulator;

#[derive(Debug, Clone, Copy)]
enum CloseKind {
    Text,
    Thinking,
    ToolCall,
}

struct FinalCapture {
    reasoning: Vec<AssistantContent>,
    texts: Vec<AssistantContent>,
    tools: Vec<AssistantContent>,
    provider_stop_reason: Option<String>,
    stop_reason: crate::StopReason,
    usage: Option<AgentUsage>,
    response_id: Option<String>,
}

impl FinalCapture {
    fn from_end(
        end: StreamEnd,
        partial: &AssistantMessage,
        pending_thought_signature: &str,
    ) -> Self {
        let StreamEnd {
            captured_usage,
            captured_stop_reason,
            captured_content,
            captured_reasoning_content,
            captured_response_id,
        } = end;

        let mut captured_texts = Vec::new();
        let mut captured_reasoning_parts = Vec::new();
        let mut captured_tools = Vec::new();
        let mut thought_signatures = Vec::new();

        if let Some(content) = captured_content {
            for part in content.into_parts() {
                match part {
                    ContentPart::Text(text) => captured_texts.push(text),
                    ContentPart::ReasoningContent(reasoning) => {
                        captured_reasoning_parts.push(reasoning)
                    }
                    ContentPart::ThoughtSignature(signature) => {
                        push_unique(&mut thought_signatures, signature)
                    }
                    ContentPart::ToolCall(call) => {
                        if let Some(signatures) = &call.thought_signatures {
                            for signature in signatures {
                                push_unique(&mut thought_signatures, signature.clone());
                            }
                        }
                        captured_tools.push(AgentToolCall::from(call));
                    }
                    ContentPart::Binary(_)
                    | ContentPart::ToolResponse(_)
                    | ContentPart::Custom(_) => {}
                }
            }
        }

        if let Some(reasoning) = captured_reasoning_content
            && (captured_reasoning_parts.is_empty()
                || !captured_reasoning_parts.contains(&reasoning))
        {
            captured_reasoning_parts.insert(0, reasoning);
        }

        let streamed_reasoning = partial
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Thinking {
                    thinking,
                    signature,
                } => {
                    if let Some(signature) = signature {
                        push_unique(&mut thought_signatures, signature.clone());
                    }
                    Some(thinking.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let streamed_texts = partial
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let streamed_tools = partial
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::ToolCall(call) => {
                    for signature in &call.thought_signatures {
                        push_unique(&mut thought_signatures, signature.clone());
                    }
                    Some(call.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        if !pending_thought_signature.is_empty() {
            push_unique(
                &mut thought_signatures,
                pending_thought_signature.to_string(),
            );
        }

        let reasoning_values = if captured_reasoning_parts.is_empty() {
            streamed_reasoning
        } else {
            captured_reasoning_parts
        };
        let text_values = if captured_texts.is_empty() {
            streamed_texts
        } else {
            captured_texts
        };
        let mut tool_values = if captured_tools.is_empty() {
            streamed_tools
        } else {
            captured_tools
        };

        // StreamEnd intentionally repeats thought-signature parts on its first tool call.
        // Store them once in the agent representation: on a thinking block when there is a
        // single corresponding reasoning block, otherwise losslessly on the first tool call.
        for tool in &mut tool_values {
            tool.thought_signatures.clear();
        }
        let mut reasoning = reasoning_values
            .into_iter()
            .map(|thinking| AssistantContent::Thinking {
                thinking,
                signature: None,
            })
            .collect::<Vec<_>>();
        if thought_signatures.len() == 1 && !reasoning.is_empty() {
            if let AssistantContent::Thinking { signature, .. } = &mut reasoning[0] {
                *signature = thought_signatures.pop();
            }
        } else if let Some(first_tool) = tool_values.first_mut() {
            first_tool.thought_signatures = thought_signatures;
        } else if !reasoning.is_empty()
            && !thought_signatures.is_empty()
            && let AssistantContent::Thinking { signature, .. } = &mut reasoning[0]
        {
            *signature = Some(thought_signatures.concat());
        }

        let texts = text_values
            .into_iter()
            .map(AssistantContent::text)
            .collect::<Vec<_>>();
        let tools = tool_values
            .into_iter()
            .map(AssistantContent::ToolCall)
            .collect::<Vec<_>>();

        let (stop_reason, provider_stop_reason) =
            map_stop_reason(captured_stop_reason, !tools.is_empty());

        Self {
            reasoning,
            texts,
            tools,
            provider_stop_reason,
            stop_reason,
            usage: captured_usage.map(AgentUsage::from),
            response_id: captured_response_id,
        }
    }
}

fn map_stop_reason(
    reason: Option<genai::chat::StopReason>,
    has_tool_calls: bool,
) -> (crate::StopReason, Option<String>) {
    use genai::chat::StopReason as GenaiStopReason;

    match reason {
        Some(reason) => {
            let raw = reason.raw().to_string();
            let normalized = match reason {
                GenaiStopReason::Completed(_) => crate::StopReason::Stop,
                GenaiStopReason::MaxTokens(_) => crate::StopReason::Length,
                GenaiStopReason::ToolCall(_) => crate::StopReason::ToolUse,
                GenaiStopReason::ContentFilter(_)
                | GenaiStopReason::StopSequence(_)
                | GenaiStopReason::Other(_) => crate::StopReason::Stop,
            };
            (normalized, Some(raw))
        }
        None if has_tool_calls => (crate::StopReason::ToolUse, None),
        None => (crate::StopReason::Stop, None),
    }
}

fn raw_tool_arguments(arguments: &Value) -> String {
    match arguments {
        Value::String(raw) => raw.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn streamed_arguments(arguments: &Value) -> Value {
    match arguments {
        Value::String(raw) => parse_streaming_json(raw),
        other => other.clone(),
    }
}

fn cumulative_delta(previous: &str, current: &str) -> Option<String> {
    if previous == current {
        None
    } else if let Some(suffix) = current.strip_prefix(previous) {
        (!suffix.is_empty()).then(|| suffix.to_string())
    } else if current.len() < previous.len() && previous.starts_with(current) {
        // Ignore a stale/out-of-order cumulative snapshot.
        None
    } else {
        // The protocol has no replacement operation. Preserve the provider's raw update rather
        // than silently dropping it; well-behaved genai streamers always take the prefix branch.
        Some(current.to_string())
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

/// Maximum container nesting accepted by [`parse_streaming_json`].
///
/// The partial parser is recursive, so a non-recursive scan enforces this bound before either the
/// complete or best-effort parser sees provider-controlled input.
const MAX_STREAMING_JSON_DEPTH: usize = 128;

/// Best-effort parser for cumulative, potentially incomplete tool-call JSON snapshots.
///
/// Complete JSON always goes through `serde_json`. The fallback accepts EOF inside objects,
/// arrays, strings, numbers, and literals, retaining every complete key/value observed so far.
/// Inputs deeper than 128 containers return the same safe empty-object fallback.
pub fn parse_streaming_json(raw: &str) -> Value {
    let raw = raw.trim();
    if raw.is_empty() || exceeds_streaming_json_depth(raw) {
        return Value::Object(Map::new());
    }
    if let Ok(value) = serde_json::from_str(raw) {
        return value;
    }

    PartialJsonParser::new(raw)
        .parse_value()
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// Scan JSON structural bytes without recursion, ignoring delimiters inside strings (including
/// escaped quotes and backslashes). Mismatched closers do not reduce the conservative container
/// stack; the partial parser remains responsible for its existing best-effort malformed behavior.
fn exceeds_streaming_json_depth(input: &str) -> bool {
    let mut containers = [0_u8; MAX_STREAMING_JSON_DEPTH];
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for byte in input.bytes() {
        if in_string {
            if escaped {
                escaped = false;
            } else {
                match byte {
                    b'\\' => escaped = true,
                    b'"' => in_string = false,
                    _ => {}
                }
            }
            continue;
        }

        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                let Some(next_depth) = depth.checked_add(1) else {
                    return true;
                };
                if next_depth > MAX_STREAMING_JSON_DEPTH {
                    return true;
                }
                containers[depth] = byte;
                depth = next_depth;
            }
            b'}' if depth > 0 && containers[depth - 1] == b'{' => depth -= 1,
            b']' if depth > 0 && containers[depth - 1] == b'[' => depth -= 1,
            _ => {}
        }
    }

    false
}

struct PartialJsonParser {
    chars: Vec<char>,
    cursor: usize,
}

impl PartialJsonParser {
    fn new(input: &str) -> Self {
        Self {
            chars: input.chars().collect(),
            cursor: 0,
        }
    }

    fn parse_value(&mut self) -> Option<Value> {
        self.skip_whitespace();
        match self.peek()? {
            '{' => Some(Value::Object(self.parse_object())),
            '[' => Some(Value::Array(self.parse_array())),
            '"' => Some(Value::String(self.parse_string().0)),
            't' => self.parse_literal("true", Value::Bool(true)),
            'f' => self.parse_literal("false", Value::Bool(false)),
            'n' => self.parse_literal("null", Value::Null),
            '-' | '0'..='9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_object(&mut self) -> Map<String, Value> {
        self.bump(); // {
        let mut object = Map::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None | Some('}') => {
                    self.bump_if('}');
                    break;
                }
                Some(',') => {
                    self.bump();
                    continue;
                }
                Some('"') => {}
                Some(_) => {
                    self.skip_to_member_boundary();
                    continue;
                }
            }

            let (key, key_closed) = self.parse_string();
            self.skip_whitespace();
            if !key_closed || !self.bump_if(':') {
                break;
            }
            self.skip_whitespace();
            let Some(value) = self.parse_value() else {
                break;
            };
            object.insert(key, value);

            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some('}') => {
                    self.bump();
                    break;
                }
                None => break,
                Some(_) => self.skip_to_member_boundary(),
            }
        }
        object
    }

    fn parse_array(&mut self) -> Vec<Value> {
        self.bump(); // [
        let mut array = Vec::new();
        loop {
            self.skip_whitespace();
            match self.peek() {
                None | Some(']') => {
                    self.bump_if(']');
                    break;
                }
                Some(',') => {
                    self.bump();
                    continue;
                }
                Some(_) => {}
            }
            let Some(value) = self.parse_value() else {
                break;
            };
            array.push(value);
            self.skip_whitespace();
            match self.peek() {
                Some(',') => {
                    self.bump();
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                None => break,
                Some(_) => self.skip_to_member_boundary(),
            }
        }
        array
    }

    fn parse_string(&mut self) -> (String, bool) {
        if !self.bump_if('"') {
            return (String::new(), false);
        }
        let mut output = String::new();
        while let Some(ch) = self.bump() {
            match ch {
                '"' => return (output, true),
                '\\' => match self.bump() {
                    Some('"') => output.push('"'),
                    Some('\\') => output.push('\\'),
                    Some('/') => output.push('/'),
                    Some('b') => output.push('\u{0008}'),
                    Some('f') => output.push('\u{000c}'),
                    Some('n') => output.push('\n'),
                    Some('r') => output.push('\r'),
                    Some('t') => output.push('\t'),
                    Some('u') => self.parse_unicode_escape(&mut output),
                    Some(other) => {
                        // Match pi-ai's repairJson: an invalid escape retains the backslash.
                        output.push('\\');
                        output.push(other);
                    }
                    None => output.push('\\'),
                },
                other => output.push(other),
            }
        }
        (output, false)
    }

    fn parse_unicode_escape(&mut self, output: &mut String) {
        let start = self.cursor;
        let mut digits = String::new();
        for _ in 0..4 {
            match self.peek() {
                Some(ch) if ch.is_ascii_hexdigit() => {
                    digits.push(ch);
                    self.bump();
                }
                _ => break,
            }
        }
        if digits.len() == 4
            && let Ok(codepoint) = u32::from_str_radix(&digits, 16)
            && let Some(ch) = char::from_u32(codepoint)
        {
            output.push(ch);
        } else {
            self.cursor = start;
            output.push_str("\\u");
        }
    }

    fn parse_literal(&mut self, expected: &str, value: Value) -> Option<Value> {
        let remaining = self.chars[self.cursor..].iter().collect::<String>();
        let token = remaining
            .chars()
            .take_while(|ch| ch.is_ascii_alphabetic())
            .collect::<String>();
        if expected.starts_with(&token) && !token.is_empty() {
            self.cursor += token.chars().count();
            Some(value)
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<Value> {
        let start = self.cursor;
        while matches!(self.peek(), Some('-' | '+' | '.' | 'e' | 'E' | '0'..='9')) {
            self.bump();
        }
        let mut token = self.chars[start..self.cursor].iter().collect::<String>();
        while !token.is_empty() {
            if let Ok(number) = token.parse::<Number>() {
                return Some(Value::Number(number));
            }
            token.pop();
        }
        None
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn skip_to_member_boundary(&mut self) {
        while let Some(ch) = self.peek() {
            if ch == ',' {
                break;
            }
            if matches!(ch, '}' | ']') {
                // Consume a mismatched closer so malformed provider JSON cannot stall the fold.
                self.bump();
                break;
            }
            self.bump();
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.cursor).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let value = self.peek()?;
        self.cursor += 1;
        Some(value)
    }

    fn bump_if(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }
}
