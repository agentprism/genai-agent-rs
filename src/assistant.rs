//! Provider-neutral assistant messages, content blocks, and streaming events.
//!
//! Assistant streams carry self-contained [`AssistantMessage`] snapshots. Incremental events
//! identify the affected content block by index, while [`AssistantMessageEvent::Done`] and
//! [`AssistantMessageEvent::Error`] carry the authoritative terminal message.

use genai::ModelIden;
use genai::adapter::AdapterKind;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

/// Estimated monetary cost of a response, in the catalog's currency (conventionally USD).
///
/// Every component is a dollar amount, not a token count. This is populated only when a
/// [`crate::PriceCatalog`] is configured and prices the response's model; see
/// [`crate::compute_cost`]. When no catalog applies, [`AgentUsage::cost`] stays `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentCost {
    /// Cost attributed to fresh (non-cached) input tokens.
    pub input: f64,
    /// Cost attributed to generated output tokens.
    pub output: f64,
    /// Cost attributed to cache-read (cache-hit) input tokens.
    pub cache_read: f64,
    /// Cost attributed to cache-write (cache-creation) input tokens, across retention splits.
    pub cache_write: f64,
    /// Sum of the component costs.
    pub total: f64,
}

impl AgentCost {
    /// Set the fresh (non-cached) input component cost.
    pub fn with_input(mut self, input: f64) -> Self {
        self.input = input;
        self
    }

    /// Set the generated-output component cost.
    pub fn with_output(mut self, output: f64) -> Self {
        self.output = output;
        self
    }

    /// Set the cache-read component cost.
    pub fn with_cache_read(mut self, cache_read: f64) -> Self {
        self.cache_read = cache_read;
        self
    }

    /// Set the cache-write component cost.
    pub fn with_cache_write(mut self, cache_write: f64) -> Self {
        self.cache_write = cache_write;
        self
    }

    /// Set the aggregate cost.
    ///
    /// This is an independent field rather than a derived value: [`compute_cost`](crate::compute_cost)
    /// sets it to the sum of the components, and callers constructing an [`AgentCost`] by hand supply
    /// the intended total.
    pub fn with_total(mut self, total: f64) -> Self {
        self.total = total;
        self
    }
}

/// Normalized token accounting for an assistant or tool result.
///
/// [`AgentUsage`] no longer derives `Eq`: [`Self::cost`] carries floating-point dollar amounts,
/// which have no total equality. Value comparisons continue to use [`PartialEq`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentUsage {
    /// Tokens consumed by the provider input, including the prompt and transcript.
    pub input_tokens: u64,
    /// Tokens generated in the provider response.
    pub output_tokens: u64,
    /// Input tokens served from a provider prompt cache.
    pub cache_read_tokens: u64,
    /// Input tokens written to a provider prompt cache.
    pub cache_write_tokens: u64,
    /// Subset of [`Self::cache_write_tokens`] written with 1h retention, when the provider reports
    /// the split (only Anthropic does today; mirrors TS `Usage.cacheWrite1h`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_1h_tokens: Option<u64>,
    /// Reasoning/thinking tokens reported by the provider, when available. This is a subset of
    /// [`Self::output_tokens`] (mirrors TS `Usage.reasoning`).
    ///
    /// genai zero-elides this counter (its `zero_as_none` deserializer maps `0` to `None`), so a
    /// value reaching this field through genai is always strictly positive: `Some(0)` is
    /// unreachable via the [`From`] impl below.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// Provider-reported total tokens, or the input/output sum when none was reported.
    pub total_tokens: u64,
    /// Estimated monetary cost of the response, populated only by a configured price catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<AgentCost>,
}

impl AgentUsage {
    /// Construct usage without cache accounting and derive the total from input plus output.
    pub const fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            cache_write_1h_tokens: None,
            reasoning_tokens: None,
            total_tokens: input_tokens + output_tokens,
            cost: None,
        }
    }

    /// Set the cache-read (cache-hit) input token count.
    pub const fn with_cache_read_tokens(mut self, cache_read_tokens: u64) -> Self {
        self.cache_read_tokens = cache_read_tokens;
        self
    }

    /// Set the cache-write (cache-creation) input token count.
    pub const fn with_cache_write_tokens(mut self, cache_write_tokens: u64) -> Self {
        self.cache_write_tokens = cache_write_tokens;
        self
    }

    /// Set the 1h-retention subset of the cache-write token count.
    pub const fn with_cache_write_1h_tokens(mut self, cache_write_1h_tokens: u64) -> Self {
        self.cache_write_1h_tokens = Some(cache_write_1h_tokens);
        self
    }

    /// Set the reasoning/thinking token count (a subset of [`Self::output_tokens`]).
    pub const fn with_reasoning_tokens(mut self, reasoning_tokens: u64) -> Self {
        self.reasoning_tokens = Some(reasoning_tokens);
        self
    }

    /// Override the provider-reported total token count.
    ///
    /// [`Self::new`] seeds this with `input_tokens + output_tokens`; use this builder when the
    /// provider reports a different total.
    pub const fn with_total_tokens(mut self, total_tokens: u64) -> Self {
        self.total_tokens = total_tokens;
        self
    }

    /// Attach an estimated monetary cost.
    pub const fn with_cost(mut self, cost: AgentCost) -> Self {
        self.cost = Some(cost);
        self
    }
}

impl From<genai::chat::Usage> for AgentUsage {
    fn from(value: genai::chat::Usage) -> Self {
        let input_tokens = value.prompt_tokens.unwrap_or_default().max(0) as u64;
        let output_tokens = value.completion_tokens.unwrap_or_default().max(0) as u64;
        let cache_read_tokens = value
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .unwrap_or_default()
            .max(0) as u64;
        let cache_write_tokens = value
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cache_creation_tokens)
            .unwrap_or_default()
            .max(0) as u64;
        // Subset of cache-write tokens with 1h retention, nested under the cache-creation TTL
        // breakdown. Negatives are clamped to 0 like the counters above; genai zero-elides this
        // field on deserialize, so `Some(0)` never survives to here.
        let cache_write_1h_tokens = value
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cache_creation_details.as_ref())
            .and_then(|details| details.ephemeral_1h_tokens)
            .map(|tokens| tokens.max(0) as u64);
        // Reasoning tokens are a subset of the output count. genai zero-elides this counter, so a
        // reported value is always strictly positive.
        let reasoning_tokens = value
            .completion_tokens_details
            .as_ref()
            .and_then(|details| details.reasoning_tokens)
            .map(|tokens| tokens.max(0) as u64);
        Self {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            cache_write_1h_tokens,
            reasoning_tokens,
            total_tokens: value
                .total_tokens
                .unwrap_or((input_tokens + output_tokens) as i32)
                .max(0) as u64,
            cost: None,
        }
    }
}

/// Agent-level stop reasons. `Pending`, `Error`, and `Aborted` are supplied by this crate;
/// the other variants normalize provider stop reasons.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The message is still being assembled and is not terminal.
    #[default]
    Pending,
    /// Generation completed normally, including provider-specific stop conditions.
    Stop,
    /// Generation stopped because the provider reached an output limit.
    Length,
    /// Generation stopped to request one or more tool calls.
    ToolUse,
    /// Generation failed because of a provider, protocol, or runtime error.
    Error,
    /// Generation was cancelled by the caller.
    Aborted,
}

/// A provider-neutral assistant tool-call block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentToolCall {
    /// Provider-assigned identifier used to correlate the eventual tool result.
    pub id: String,
    /// Name of the requested tool.
    pub name: String,
    /// Parsed JSON arguments supplied to the tool.
    pub arguments: Value,
    /// Opaque provider signatures that must accompany the call in later requests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thought_signatures: Vec<String>,
}

impl AgentToolCall {
    /// Construct a tool call without provider thought signatures.
    pub fn new(id: impl Into<String>, name: impl Into<String>, arguments: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments,
            thought_signatures: Vec::new(),
        }
    }

    /// Attach opaque provider thought signatures to the call.
    pub fn with_thought_signatures(mut self, signatures: Vec<String>) -> Self {
        self.thought_signatures = signatures;
        self
    }
}

impl From<genai::chat::ToolCall> for AgentToolCall {
    fn from(value: genai::chat::ToolCall) -> Self {
        Self {
            id: value.call_id,
            name: value.fn_name,
            arguments: value.fn_arguments,
            thought_signatures: value.thought_signatures.unwrap_or_default(),
        }
    }
}

impl From<AgentToolCall> for genai::chat::ToolCall {
    fn from(value: AgentToolCall) -> Self {
        Self {
            call_id: value.id,
            fn_name: value.name,
            fn_arguments: value.arguments,
            thought_signatures: (!value.thought_signatures.is_empty())
                .then_some(value.thought_signatures),
        }
    }
}

/// One ordered block in an assistant response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantContent {
    /// User-visible generated text.
    Text {
        /// Text for this ordered response block.
        text: String,
        /// Opaque provider signature associated with the block, when supplied.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// Provider reasoning or thinking content.
    Thinking {
        /// Reasoning text for this ordered response block.
        thinking: String,
        /// Opaque provider signature required when replaying the reasoning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// A request to execute a tool.
    ToolCall(AgentToolCall),
}

impl AssistantContent {
    /// Construct an unsigned text block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            signature: None,
        }
    }

    /// Construct an unsigned thinking block.
    pub fn thinking(thinking: impl Into<String>) -> Self {
        Self::Thinking {
            thinking: thinking.into(),
            signature: None,
        }
    }

    /// Wrap a provider-neutral tool call as assistant content.
    pub fn tool_call(call: AgentToolCall) -> Self {
        Self::ToolCall(call)
    }
}

/// A complete or partial assistant message accumulated from stream events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    /// Ordered text, thinking, and tool-call blocks accumulated for the response.
    pub content: Vec<AssistantContent>,
    /// Current completion state; partial messages use [`StopReason::Pending`].
    pub stop_reason: StopReason,
    /// Provider's unnormalized stop-reason string, when available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_stop_reason: Option<String>,
    /// Human-readable failure detail for error or aborted messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Normalized token accounting captured for the response.
    pub usage: AgentUsage,
    /// Provider adapter and model that generated the response.
    pub model: ModelIden,
    /// Provider response identifier, when one was returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// Message creation time in milliseconds since the Unix epoch.
    pub timestamp: i64,
}

impl AssistantMessage {
    /// Construct an empty, pending message for `model` with the current timestamp.
    pub fn new(model: ModelIden) -> Self {
        Self {
            content: Vec::new(),
            stop_reason: StopReason::Pending,
            provider_stop_reason: None,
            error_message: None,
            usage: AgentUsage::default(),
            model,
            response_id: None,
            timestamp: timestamp_ms(),
        }
    }

    /// Construct a message with completed content and the supplied stop reason.
    pub fn completed(model: ModelIden, content: Vec<AssistantContent>, reason: StopReason) -> Self {
        Self {
            content,
            stop_reason: reason,
            ..Self::new(model)
        }
    }

    /// Construct a terminal failure message.
    ///
    /// `reason` is preserved only for [`StopReason::Error`] and [`StopReason::Aborted`]; every
    /// other reason is normalized to [`StopReason::Error`].
    pub fn error(model: ModelIden, reason: StopReason, error: impl Into<String>) -> Self {
        let reason = if matches!(reason, StopReason::Error | StopReason::Aborted) {
            reason
        } else {
            StopReason::Error
        };
        Self {
            stop_reason: reason,
            error_message: Some(error.into()),
            ..Self::new(model)
        }
    }

    /// Iterate over tool calls in content order, skipping text and thinking blocks.
    pub fn tool_calls(&self) -> impl Iterator<Item = &AgentToolCall> {
        self.content.iter().filter_map(|part| match part {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// Concatenate all user-visible text blocks without inserting separators.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

impl Default for AssistantMessage {
    fn default() -> Self {
        Self::new(unknown_model_iden())
    }
}

/// The lossless assistant streaming protocol consumed by the agent loop.
///
/// Every nonterminal event carries the complete message snapshot after that event's update. Block
/// indices refer to [`AssistantMessage::content`]. A conforming producer finishes with exactly one
/// [`Done`](Self::Done) or [`Error`](Self::Error); [`crate::AssistantMessageEventStream`] publishes
/// the first such event as its result and becomes fused, so later upstream events are not observed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    /// Begins an assistant response before its incremental content events.
    Start {
        /// Initial pending message snapshot.
        partial: AssistantMessage,
    },
    /// Announces a new user-visible text block.
    TextStart {
        /// Index assigned to the text block.
        content_index: usize,
        /// Message snapshot containing the newly opened block.
        partial: AssistantMessage,
    },
    /// Appends generated text to an open text block.
    TextDelta {
        /// Index of the text block being updated.
        content_index: usize,
        /// Newly generated text fragment.
        delta: String,
        /// Message snapshot after applying the fragment.
        partial: AssistantMessage,
    },
    /// Closes a text block with its authoritative captured content.
    TextEnd {
        /// Index of the text block being closed.
        content_index: usize,
        /// Complete text for the block.
        content: String,
        /// Message snapshot after finalizing the block.
        partial: AssistantMessage,
    },
    /// Announces a new provider reasoning block.
    ThinkingStart {
        /// Index assigned to the thinking block.
        content_index: usize,
        /// Message snapshot containing the newly opened block.
        partial: AssistantMessage,
    },
    /// Appends provider reasoning to an open thinking block.
    ThinkingDelta {
        /// Index of the thinking block being updated.
        content_index: usize,
        /// Newly generated reasoning fragment.
        delta: String,
        /// Message snapshot after applying the fragment.
        partial: AssistantMessage,
    },
    /// Closes a thinking block with its authoritative captured content.
    ThinkingEnd {
        /// Index of the thinking block being closed.
        content_index: usize,
        /// Complete reasoning text for the block.
        thinking: String,
        /// Message snapshot after finalizing the block.
        partial: AssistantMessage,
    },
    /// Announces a new tool-call block.
    ToolCallStart {
        /// Index assigned to the tool call.
        content_index: usize,
        /// Current message snapshot for the opened call.
        partial: AssistantMessage,
    },
    /// Supplies a raw JSON-argument fragment for an open tool call.
    ToolCallDelta {
        /// Index of the tool call being updated.
        content_index: usize,
        /// Newly observed raw argument fragment.
        delta: String,
        /// Message snapshot with best-effort parsed arguments.
        partial: AssistantMessage,
    },
    /// Closes a tool call with its authoritative parsed value.
    ToolCallEnd {
        /// Index of the tool call being closed.
        content_index: usize,
        /// Complete provider-neutral tool call.
        tool_call: AgentToolCall,
        /// Message snapshot after finalizing the call.
        partial: AssistantMessage,
    },
    /// Successfully terminates the response, including length and tool-use stops.
    Done {
        /// Normalized terminal stop reason.
        reason: StopReason,
        /// Authoritative completed assistant message.
        message: AssistantMessage,
    },
    /// Terminates the response in-band because it failed or was aborted.
    Error {
        /// Normalized error or abort reason.
        reason: StopReason,
        /// Authoritative terminal message retaining any partial content and failure detail.
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    /// Return the authoritative message for a terminal event.
    ///
    /// Nonterminal events return `None`; use [`Self::partial`] when a snapshot is needed for every
    /// event kind.
    pub fn terminal_message(&self) -> Option<&AssistantMessage> {
        match self {
            Self::Done { message, .. } => Some(message),
            Self::Error { error, .. } => Some(error),
            _ => None,
        }
    }

    /// Return the current message snapshot for either a partial or terminal event.
    pub fn partial(&self) -> &AssistantMessage {
        match self {
            Self::Start { partial }
            | Self::TextStart { partial, .. }
            | Self::TextDelta { partial, .. }
            | Self::TextEnd { partial, .. }
            | Self::ThinkingStart { partial, .. }
            | Self::ThinkingDelta { partial, .. }
            | Self::ThinkingEnd { partial, .. }
            | Self::ToolCallStart { partial, .. }
            | Self::ToolCallDelta { partial, .. }
            | Self::ToolCallEnd { partial, .. } => partial,
            Self::Done { message, .. } => message,
            Self::Error { error, .. } => error,
        }
    }
}

pub(crate) fn timestamp_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

pub(crate) fn unknown_model_iden() -> ModelIden {
    ModelIden::new(AdapterKind::Ollama, "unknown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{CacheCreationDetails, CompletionTokensDetails, PromptTokensDetails, Usage};

    #[test]
    fn usage_from_genai_maps_cache_write_1h_and_reasoning_tokens() {
        let usage = Usage {
            prompt_tokens: Some(100),
            prompt_tokens_details: Some(PromptTokensDetails {
                cache_creation_tokens: Some(50),
                cache_creation_details: Some(CacheCreationDetails {
                    ephemeral_5m_tokens: Some(30),
                    ephemeral_1h_tokens: Some(20),
                }),
                cached_tokens: Some(10),
                ..Default::default()
            }),
            completion_tokens: Some(40),
            completion_tokens_details: Some(CompletionTokensDetails {
                reasoning_tokens: Some(25),
                ..Default::default()
            }),
            total_tokens: Some(150),
        };

        let agent = AgentUsage::from(usage);

        assert_eq!(agent.input_tokens, 100);
        assert_eq!(agent.output_tokens, 40);
        assert_eq!(agent.cache_read_tokens, 10);
        assert_eq!(agent.cache_write_tokens, 50);
        assert_eq!(agent.cache_write_1h_tokens, Some(20));
        assert_eq!(agent.reasoning_tokens, Some(25));
        assert_eq!(agent.total_tokens, 150);
        // Cost is never derived by the From impl; only a price catalog populates it.
        assert_eq!(agent.cost, None);
    }

    #[test]
    fn usage_from_genai_leaves_new_fields_none_without_details() {
        let usage = Usage {
            prompt_tokens: Some(5),
            completion_tokens: Some(3),
            ..Default::default()
        };

        let agent = AgentUsage::from(usage);

        assert_eq!(agent.cache_write_1h_tokens, None);
        assert_eq!(agent.reasoning_tokens, None);
        assert_eq!(agent.cost, None);
        // Total falls back to input + output when the provider reports none.
        assert_eq!(agent.total_tokens, 8);
    }
}
