//! Shared message lowering ⇐ pi `src/api/transform-messages.ts` (preserved seam #11).

use crate::types::{
    AssistantContent, AssistantMessage, JsString, Message, Model, ModelInput, StopReason,
    TextContent, ToolCall, ToolResultMessage, ToolResultRole, UserContent, UserContentBlock,
};
use crate::utils::hash::short_hash;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{SystemTime, UNIX_EPOCH};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";
const MISTRAL_TOOL_CALL_ID_LENGTH: usize = 9;

pub type ToolCallIdNormalizer<'a> =
    dyn Fn(&JsString, &Model, &AssistantMessage) -> JsString + Send + Sync + 'a;

fn replace_images_with_placeholder(
    content: &[UserContentBlock],
    placeholder: &str,
) -> Vec<UserContentBlock> {
    let mut result = Vec::new();
    let mut previous_was_placeholder = false;
    for block in content {
        match block {
            UserContentBlock::Image(_) => {
                if !previous_was_placeholder {
                    result.push(UserContentBlock::Text(TextContent::new(placeholder)));
                }
                previous_was_placeholder = true;
            }
            UserContentBlock::Text(text) => {
                result.push(UserContentBlock::Text(text.clone()));
                previous_was_placeholder = text.text == placeholder;
            }
        }
    }
    result
}

fn downgrade_unsupported_images(message: &Message, model: &Model) -> Message {
    if model.input.contains(&ModelInput::Image) {
        return message.clone();
    }
    match message {
        Message::User(message) => {
            let mut message = (**message).clone();
            if let UserContent::Blocks(content) = &message.content {
                message.content = UserContent::Blocks(replace_images_with_placeholder(
                    content,
                    NON_VISION_USER_IMAGE_PLACEHOLDER,
                ));
            }
            Message::User(Box::new(message))
        }
        Message::ToolResult(message) => {
            let mut message = (**message).clone();
            message.content = replace_images_with_placeholder(
                &message.content,
                NON_VISION_TOOL_IMAGE_PLACEHOLDER,
            );
            Message::ToolResult(Box::new(message))
        }
        Message::Assistant(_) => message.clone(),
    }
}

pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<&ToolCallIdNormalizer<'_>>,
) -> Vec<Message> {
    let mut tool_call_id_map = BTreeMap::<crate::types::JsString, crate::types::JsString>::new();
    let mut transformed = Vec::with_capacity(messages.len());

    for original in messages {
        let message = downgrade_unsupported_images(original, model);
        match message {
            Message::User(_) => transformed.push(message),
            Message::ToolResult(mut tool_result) => {
                if let Some(normalized) = tool_call_id_map.get(&tool_result.tool_call_id)
                    && normalized != &tool_result.tool_call_id
                {
                    tool_result.tool_call_id = normalized.clone();
                }
                transformed.push(Message::ToolResult(tool_result));
            }
            Message::Assistant(mut assistant) => {
                let same_model = assistant.provider == model.provider
                    && assistant.api == model.api
                    && assistant.model == model.id;
                let mut content = Vec::with_capacity(assistant.content.len());
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            if thinking.redacted == Some(true) {
                                if same_model {
                                    content.push(block.clone());
                                }
                                continue;
                            }
                            if same_model
                                && thinking
                                    .thinking_signature
                                    .as_ref()
                                    .is_some_and(|signature| !signature.is_empty())
                            {
                                content.push(block.clone());
                                continue;
                            }
                            if thinking.thinking.is_blank() {
                                continue;
                            }
                            if same_model {
                                content.push(block.clone());
                            } else {
                                content.push(AssistantContent::Text(TextContent::new(
                                    &thinking.thinking,
                                )));
                            }
                        }
                        AssistantContent::Text(text) => {
                            if same_model {
                                content.push(block.clone());
                            } else {
                                content.push(AssistantContent::Text(TextContent::new(&text.text)));
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let mut normalized = tool_call.clone();
                            if !same_model
                                && normalized
                                    .thought_signature
                                    .as_ref()
                                    .is_some_and(|signature| !signature.is_empty())
                            {
                                normalized.thought_signature = None;
                            }
                            if !same_model && let Some(normalizer) = normalize_tool_call_id {
                                let normalized_id = normalizer(&tool_call.id, model, &assistant);
                                if normalized_id != tool_call.id {
                                    tool_call_id_map
                                        .insert(tool_call.id.clone(), normalized_id.clone());
                                    normalized.id = normalized_id;
                                }
                            }
                            content.push(AssistantContent::ToolCall(normalized));
                        }
                    }
                }
                assistant.content = content;
                transformed.push(Message::Assistant(assistant));
            }
        }
    }

    let mut result = Vec::with_capacity(transformed.len());
    let mut pending_tool_calls = Vec::new();
    let mut existing_tool_result_ids = BTreeSet::new();
    for message in transformed {
        match message {
            Message::Assistant(assistant) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }
                pending_tool_calls = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tool_call) => Some(tool_call.clone()),
                        AssistantContent::Text(_) | AssistantContent::Thinking(_) => None,
                    })
                    .collect();
                existing_tool_result_ids.clear();
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                existing_tool_result_ids.insert(tool_result.tool_call_id.clone());
                result.push(Message::ToolResult(tool_result));
            }
            Message::User(user) => {
                insert_synthetic_tool_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_tool_result_ids,
                );
                result.push(Message::User(user));
            }
        }
    }
    insert_synthetic_tool_results(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_tool_result_ids,
    );
    result
}

fn insert_synthetic_tool_results(
    result: &mut Vec<Message>,
    pending_tool_calls: &mut Vec<ToolCall>,
    existing_tool_result_ids: &mut BTreeSet<crate::types::JsString>,
) {
    for tool_call in pending_tool_calls.drain(..) {
        if !existing_tool_result_ids.contains(&tool_call.id) {
            result.push(Message::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: tool_call.id,
                tool_name: tool_call.name,
                content: vec![UserContentBlock::Text(TextContent::new(
                    "No result provided",
                ))],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: true,
                timestamp: now_millis(),
            })));
        }
    }
    existing_tool_result_ids.clear();
}

fn now_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64
}

pub fn normalize_anthropic_tool_call_id(id: &str) -> String {
    sanitize_id_part(id, true).chars().take(64).collect()
}

pub fn normalize_open_ai_completions_tool_call_id(id: &str, model: &Model) -> String {
    if let Some(separator) = id.find('|') {
        let call_id = sanitize_id_part(&id[..separator], true);
        let item_id = sanitize_id_part(&id[separator + 1..], true);
        let combined = if item_id.is_empty() {
            call_id.clone()
        } else {
            format!("{call_id}_{item_id}")
        };
        if combined.chars().count() <= 40 {
            return combined;
        }
        let hash = short_hash(id).chars().take(8).collect::<String>();
        let prefix_length = 40_usize.saturating_sub(hash.len() + 1).max(1);
        let prefix = call_id.chars().take(prefix_length).collect::<String>();
        return format!("{prefix}_{hash}");
    }
    if model.provider.as_str() == "openai" {
        return take_utf16(id, 40);
    }
    id.to_owned()
}

pub fn normalize_responses_tool_call_id(
    id: &str,
    model: &Model,
    source: &AssistantMessage,
    allowed_tool_call_providers: &BTreeSet<String>,
) -> String {
    if !allowed_tool_call_providers.contains(model.provider.as_str()) {
        return normalize_responses_id_part(id);
    }
    let Some((call_id, remainder)) = id.split_once('|') else {
        return normalize_responses_id_part(id);
    };
    let item_id = remainder.split('|').next().unwrap_or_default();
    let normalized_call_id = normalize_responses_id_part(call_id);
    let foreign = source.provider != model.provider || source.api != model.api;
    let mut normalized_item_id = if foreign {
        format!("fc_{}", short_hash(item_id))
            .chars()
            .take(64)
            .collect()
    } else {
        normalize_responses_id_part(item_id)
    };
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_responses_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

fn normalize_responses_id_part(part: &str) -> String {
    sanitize_id_part(part, true)
        .chars()
        .take(64)
        .collect::<String>()
        .trim_end_matches('_')
        .to_owned()
}

fn sanitize_id_part(id: &str, allow_underscore_hyphen: bool) -> String {
    id.encode_utf16()
        .map(|unit| {
            let byte = u8::try_from(unit).ok();
            if byte.is_some_and(|byte| {
                byte.is_ascii_alphanumeric()
                    || (allow_underscore_hyphen && matches!(byte, b'_' | b'-'))
            }) {
                char::from(byte.expect("checked above"))
            } else {
                '_'
            }
        })
        .collect()
}

fn take_utf16(value: &str, maximum: usize) -> String {
    let mut units = 0;
    value
        .chars()
        .take_while(|character| {
            let next = units + character.len_utf16();
            if next > maximum {
                false
            } else {
                units = next;
                true
            }
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct MistralToolCallIdNormalizer {
    id_map: BTreeMap<String, String>,
    reverse_map: BTreeMap<String, String>,
}

impl MistralToolCallIdNormalizer {
    pub fn normalize(&mut self, id: &str) -> String {
        if let Some(existing) = self.id_map.get(id) {
            return existing.clone();
        }
        for attempt in 0_u64.. {
            let candidate = derive_mistral_tool_call_id(id, attempt);
            let owner = self.reverse_map.get(&candidate);
            if owner.is_none_or(|owner| owner == id) {
                self.id_map.insert(id.to_owned(), candidate.clone());
                self.reverse_map.insert(candidate.clone(), id.to_owned());
                return candidate;
            }
        }
        unreachable!("the hash namespace has an available identifier")
    }
}

fn derive_mistral_tool_call_id(id: &str, attempt: u64) -> String {
    let normalized = id
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>();
    if attempt == 0 && normalized.len() == MISTRAL_TOOL_CALL_ID_LENGTH {
        return normalized;
    }
    let seed_base = if normalized.is_empty() {
        id
    } else {
        &normalized
    };
    let seed = if attempt == 0 {
        seed_base.to_owned()
    } else {
        format!("{seed_base}:{attempt}")
    };
    short_hash(&seed)
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(MISTRAL_TOOL_CALL_ID_LENGTH)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ImageContent;
    use crate::types::*;
    use serde_json::{Map, json};

    fn model(input: Vec<ModelInput>) -> Model {
        Model {
            id: "target".to_owned(),
            name: "Target".to_owned(),
            api: "anthropic-messages".into(),
            provider: "target-provider".into(),
            base_url: "https://example.test".to_owned(),
            reasoning: true,
            thinking_level_map: None,
            input,
            cost: ModelCost::default(),
            context_window: 128_000.0,
            max_tokens: 4_096.0,
            sampling_params: None,
            headers: None,
            compat: None,
        }
    }

    fn assistant(model: &Model, content: Vec<AssistantContent>) -> AssistantMessage {
        let mut message = AssistantMessage::pending(
            model.api.clone(),
            model.provider.clone(),
            model.id.clone(),
            1.0,
        );
        message.content = content;
        message.stop_reason = StopReason::Stop;
        message
    }

    fn foreign_assistant(content: Vec<AssistantContent>) -> AssistantMessage {
        let source = model(vec![ModelInput::Text]);
        let mut message = assistant(&source, content);
        message.api = "openai-responses".into();
        message.provider = "source-provider".into();
        message.model = "source".into();
        message
    }

    fn tool_result(id: &str) -> Message {
        Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: id.into(),
            tool_name: "run".into(),
            content: vec![UserContentBlock::Text(TextContent::new("done"))],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 2.0,
        }))
    }

    /// Pins pi `src/api/transform-messages.ts:95-125` same-model and cross-model replay branches.
    #[test]
    fn thinking_and_text_fidelity_follow_model_identity() {
        let target = model(vec![ModelInput::Text]);
        let mut signed_empty = ThinkingContent::new("");
        signed_empty.thinking_signature = Some("ciphertext".into());
        let mut unsigned = ThinkingContent::new("reasoning");
        unsigned.redacted = Some(false);
        let mut redacted = ThinkingContent::new("");
        redacted.redacted = Some(true);
        redacted.thinking_signature = Some("opaque".into());
        let mut text = TextContent::new("answer");
        text.text_signature = Some("message-id".into());
        let same = Message::Assistant(Box::new(assistant(
            &target,
            vec![
                AssistantContent::Thinking(signed_empty.clone()),
                AssistantContent::Thinking(ThinkingContent::new("   ")),
                AssistantContent::Thinking(unsigned.clone()),
                AssistantContent::Thinking(redacted.clone()),
                AssistantContent::Text(text.clone()),
            ],
        )));
        let transformed = transform_messages(&[same], &target, None);
        let Message::Assistant(same) = &transformed[0] else {
            panic!("assistant")
        };
        assert_eq!(
            same.content,
            vec![
                AssistantContent::Thinking(signed_empty),
                AssistantContent::Thinking(unsigned),
                AssistantContent::Thinking(redacted),
                AssistantContent::Text(text),
            ]
        );

        let foreign = Message::Assistant(Box::new(foreign_assistant(vec![
            AssistantContent::Thinking(ThinkingContent::new("reasoning")),
            AssistantContent::Thinking({
                let mut value = ThinkingContent::new("hidden");
                value.redacted = Some(true);
                value
            }),
            AssistantContent::Text({
                let mut value = TextContent::new("answer");
                value.text_signature = Some("foreign".into());
                value
            }),
        ])));
        let transformed = transform_messages(&[foreign], &target, None);
        let Message::Assistant(foreign) = &transformed[0] else {
            panic!("assistant")
        };
        assert_eq!(
            foreign.content,
            vec![
                AssistantContent::Text(TextContent::new("reasoning")),
                AssistantContent::Text(TextContent::new("answer")),
            ]
        );
    }

    /// Pins pi `src/api/transform-messages.ts:127-145` id remapping and fidelity stripping.
    #[test]
    fn cross_model_tool_calls_drop_signature_and_remap_results() {
        let target = model(vec![ModelInput::Text]);
        let mut call = ToolCall::new("call|foreign", "run", Map::new());
        call.thought_signature = Some("opaque".into());
        let messages = [
            Message::Assistant(Box::new(foreign_assistant(vec![
                AssistantContent::ToolCall(call),
            ]))),
            tool_result("call|foreign"),
        ];
        let normalizer = |id: &JsString, _: &Model, _: &AssistantMessage| {
            normalize_anthropic_tool_call_id(&id.to_utf8_lossy()).into()
        };
        let transformed = transform_messages(&messages, &target, Some(&normalizer));
        let Message::Assistant(assistant) = &transformed[0] else {
            panic!("assistant")
        };
        let AssistantContent::ToolCall(call) = &assistant.content[0] else {
            panic!("tool")
        };
        assert_eq!(call.id, "call_foreign");
        assert_eq!(call.thought_signature, None);
        let Message::ToolResult(result) = &transformed[1] else {
            panic!("result")
        };
        assert_eq!(result.tool_call_id, "call_foreign");
    }

    /// Pins pi `src/api/transform-messages.ts:158-220` terminal filtering and orphan repair.
    #[test]
    fn drops_failed_turns_and_synthesizes_only_missing_results() {
        let target = model(vec![ModelInput::Text]);
        let mut calls = foreign_assistant(vec![
            AssistantContent::ToolCall(ToolCall::new("one|fc_1", "read", Map::new())),
            AssistantContent::ToolCall(ToolCall::new("two|fc_2", "run", Map::new())),
        ]);
        calls.stop_reason = StopReason::ToolUse;
        let mut failed =
            foreign_assistant(vec![AssistantContent::Text(TextContent::new("partial"))]);
        failed.stop_reason = StopReason::Error;
        let mut aborted =
            foreign_assistant(vec![AssistantContent::Text(TextContent::new("partial"))]);
        aborted.stop_reason = StopReason::Aborted;
        let messages = [
            Message::Assistant(Box::new(calls)),
            tool_result("one|fc_1"),
            Message::Assistant(Box::new(failed)),
            Message::Assistant(Box::new(aborted)),
            Message::User(Box::new(UserMessage {
                role: UserRole::User,
                content: UserContent::Text(("continue".to_owned()).into()),
                timestamp: 3.0,
            })),
        ];
        let normalizer = |id: &JsString, _: &Model, _: &AssistantMessage| {
            normalize_anthropic_tool_call_id(&id.to_utf8_lossy()).into()
        };
        let transformed = transform_messages(&messages, &target, Some(&normalizer));
        assert_eq!(
            transformed
                .iter()
                .filter(|message| matches!(message, Message::Assistant(_)))
                .count(),
            1
        );
        let synthetic = transformed
            .iter()
            .filter_map(|message| match message {
                Message::ToolResult(result) if result.is_error => Some(result),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(synthetic.len(), 1);
        assert_eq!(synthetic[0].tool_call_id, "two_fc_2");
        assert_eq!(
            synthetic[0].content,
            vec![UserContentBlock::Text(TextContent::new(
                "No result provided"
            ))]
        );
    }

    /// Pins pi `src/api/transform-messages.ts:158-220` interruption and trailing repair.
    #[test]
    fn repairs_interrupted_and_trailing_tool_flows_in_order() {
        let target = model(vec![ModelInput::Text]);
        let mut first = foreign_assistant(vec![AssistantContent::ToolCall(ToolCall::new(
            "interrupted",
            "read",
            Map::new(),
        ))]);
        first.stop_reason = StopReason::ToolUse;
        let mut second = foreign_assistant(vec![AssistantContent::ToolCall(ToolCall::new(
            "trailing",
            "run",
            Map::new(),
        ))]);
        second.stop_reason = StopReason::ToolUse;
        let transformed = transform_messages(
            &[
                Message::Assistant(Box::new(first)),
                Message::User(Box::new(UserMessage {
                    role: UserRole::User,
                    content: UserContent::Text(("interrupt".to_owned()).into()),
                    timestamp: 3.0,
                })),
                Message::Assistant(Box::new(second)),
            ],
            &target,
            None,
        );
        assert!(matches!(&transformed[0], Message::Assistant(_)));
        assert!(matches!(
            &transformed[1],
            Message::ToolResult(result) if result.tool_call_id == "interrupted" && result.is_error
        ));
        assert!(matches!(&transformed[2], Message::User(_)));
        assert!(matches!(&transformed[3], Message::Assistant(_)));
        assert!(matches!(
            &transformed[4],
            Message::ToolResult(result) if result.tool_call_id == "trailing" && result.is_error
        ));
    }

    /// Pins pi `src/api/transform-messages.ts:12-57` image downgrade and placeholder coalescing.
    #[test]
    fn degrades_images_only_for_non_vision_models() {
        let image = UserContentBlock::Image(ImageContent::new("AA==", "image/png"));
        let user = Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Blocks(vec![
                image.clone(),
                image.clone(),
                UserContentBlock::Text(TextContent::new("after")),
            ]),
            timestamp: 1.0,
        }));
        let non_vision = model(vec![ModelInput::Text]);
        let transformed = transform_messages(std::slice::from_ref(&user), &non_vision, None);
        let Message::User(message) = &transformed[0] else {
            panic!("user")
        };
        assert_eq!(
            message.content,
            UserContent::Blocks(vec![
                UserContentBlock::Text(TextContent::new(NON_VISION_USER_IMAGE_PLACEHOLDER)),
                UserContentBlock::Text(TextContent::new("after")),
            ])
        );
        let vision = model(vec![ModelInput::Text, ModelInput::Image]);
        assert_eq!(
            transform_messages(std::slice::from_ref(&user), &vision, None),
            vec![user]
        );

        let tool = Message::ToolResult(Box::new(ToolResultMessage {
            role: ToolResultRole::ToolResult,
            tool_call_id: "image".into(),
            tool_name: "render".into(),
            content: vec![
                UserContentBlock::Text(TextContent::new(NON_VISION_TOOL_IMAGE_PLACEHOLDER)),
                image,
            ],
            details: None,
            usage: None,
            added_tool_names: None,
            is_error: false,
            timestamp: 2.0,
        }));
        let transformed = transform_messages(&[tool], &non_vision, None);
        let Message::ToolResult(message) = &transformed[0] else {
            panic!("tool result")
        };
        assert_eq!(
            message.content,
            vec![UserContentBlock::Text(TextContent::new(
                NON_VISION_TOOL_IMAGE_PLACEHOLDER
            ))]
        );
    }

    /// Pins pi `src/api/anthropic-messages.ts:1115-1118`,
    /// `src/api/openai-completions.ts:1146-1169`, and
    /// `src/api/openai-responses-shared.ts:147-170` UTF-16 ID constraints.
    #[test]
    fn target_id_normalizers_match_pi_constraints() {
        let anthropic = normalize_anthropic_tool_call_id(&format!("bad|{}", "x".repeat(80)));
        assert_eq!(anthropic.len(), 64);
        assert!(
            anthropic.chars().all(
                |character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
            )
        );
        assert_eq!(normalize_anthropic_tool_call_id("a💥b"), "a__b");
        assert_eq!(
            normalize_anthropic_tool_call_id(&format!("{}💥z", "a".repeat(63))),
            format!("{}_", "a".repeat(63))
        );

        let mut mistral = MistralToolCallIdNormalizer::default();
        let first = mistral.normalize("call|with/special+characters");
        assert_eq!(first, "q49m1uqci");
        assert!(
            first
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        );
        assert_eq!(mistral.normalize("call|with/special+characters"), first);
        assert_eq!(mistral.normalize("Abc123xyz"), "Abc123xyz");

        let mut completions = model(vec![ModelInput::Text]);
        completions.api = "openai-completions".into();
        completions.provider = "openai".into();
        let one = normalize_open_ai_completions_tool_call_id(
            &format!("call_shared|{}", "a+/=".repeat(100)),
            &completions,
        );
        let two = normalize_open_ai_completions_tool_call_id(
            &format!("call_shared|{}", "b+/=".repeat(100)),
            &completions,
        );
        assert!(one.len() <= 40);
        assert_eq!(one, "call_shared_1ehe6jv1");
        assert_eq!(two, "call_shared_v3nhpp1d");
        assert_ne!(one, two);
        assert_eq!(
            normalize_open_ai_completions_tool_call_id("a💥|b", &completions),
            "a___b"
        );
        assert_eq!(
            normalize_open_ai_completions_tool_call_id(
                &format!("{}💥z", "a".repeat(38)),
                &completions
            ),
            format!("{}💥", "a".repeat(38))
        );
        assert_eq!(
            normalize_open_ai_completions_tool_call_id(&"x".repeat(50), &completions).len(),
            40
        );

        let mut responses = model(vec![ModelInput::Text]);
        responses.api = "openai-responses".into();
        responses.provider = "openai".into();
        let mut same_source = assistant(&responses, vec![]);
        same_source.model = "other-model".into();
        let allowed = BTreeSet::from(["openai".to_owned()]);
        assert_eq!(
            normalize_responses_tool_call_id("call_1|fc_1", &responses, &same_source, &allowed),
            "call_1|fc_1"
        );
        let foreign = foreign_assistant(vec![]);
        let normalized = normalize_responses_tool_call_id(
            "call_1|foreign+/item",
            &responses,
            &foreign,
            &allowed,
        );
        assert_eq!(normalized, "call_1|fc_81dvun1sxj9to");
        assert!(normalized.contains('|'));
        assert_eq!(
            normalize_responses_tool_call_id("a💥b|fc_x", &responses, &same_source, &allowed),
            "a__b|fc_x"
        );
    }

    /// Same-model callbacks are intentionally bypassed by pi `src/api/transform-messages.ts:136`.
    #[test]
    fn same_model_tool_ids_and_signatures_are_verbatim() {
        let target = model(vec![ModelInput::Text]);
        let mut call = ToolCall::new(
            "call|verbatim",
            "run",
            Map::from_iter([("x".to_owned(), json!(1))]),
        );
        call.thought_signature = Some("signature".into());
        let message = Message::Assistant(Box::new(assistant(
            &target,
            vec![AssistantContent::ToolCall(call.clone())],
        )));
        let normalizer =
            |_: &JsString, _: &Model, _: &AssistantMessage| panic!("must not normalize");
        let transformed = transform_messages(&[message], &target, Some(&normalizer));
        let Message::Assistant(assistant) = &transformed[0] else {
            panic!("assistant")
        };
        assert_eq!(assistant.content, vec![AssistantContent::ToolCall(call)]);
    }
}
