use pi_ai::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Number, Value, json, value::RawValue};
use static_assertions::assert_not_impl_any;
use std::{collections::BTreeSet, fmt::Debug};
use url::Url;

fn assert_round_trip<T>(value: &T)
where
    T: Serialize + DeserializeOwned + Debug + PartialEq,
{
    let bytes = serde_json::to_vec(value).expect("serialize persisted value");
    let restored = serde_json::from_slice::<T>(&bytes).expect("deserialize persisted value");
    assert_eq!(&restored, value);
}

fn scope(provider: &str, api: &str, requested: &str, produced: &str) -> ReplayScope {
    ReplayScope::new(provider, api, requested, produced)
}

fn usage() -> Usage {
    Usage {
        input_tokens: 1_000,
        output_tokens: 100,
        reasoning_tokens: Some(25),
        cache_read_tokens: Some(1_000),
        cache_write_tokens: Some(500),
        source: UsageSource::ProviderReported,
    }
}

fn finish(reason: AssistantFinishReason) -> AssistantFinish {
    AssistantFinish {
        reason,
        raw_provider_reason: None,
        error: None,
    }
}

fn replay_item(
    id: &str,
    ordinal: u32,
    target: ReplayTarget,
    kind: &str,
    payload: OpaquePayload,
) -> ReplayItem {
    ReplayItem {
        id: ReplayItemId::new(id),
        ordinal,
        target,
        kind: ReplayKind::new(kind),
        applicability: ReplayApplicability::ExactProviderApiModel,
        completeness: ReplayCompleteness::Complete,
        payload,
    }
}

fn empty_level_map<T>() -> ThinkingLevelMap<T> {
    ThinkingLevelMap {
        off: None,
        minimal: None,
        low: None,
        medium: None,
        high: None,
        xhigh: None,
        max: None,
    }
}

fn rates(input: u128, output: u128, cache_read: u128, cache_write: u128) -> TokenPriceRates {
    TokenPriceRates {
        input: MoneyRate::new(input),
        output: MoneyRate::new(output),
        cache_read: MoneyRate::new(cache_read),
        cache_write: MoneyRate::new(cache_write),
    }
}

fn pricing() -> ModelPricing {
    ModelPricing {
        default: rates(3_000_000, 15_000_000, 300_000, 3_750_000),
        request_wide_tiers: vec![RequestWidePriceTier {
            input_tokens_above: 5_000,
            rates: rates(6_000_000, 22_500_000, 600_000, 7_500_000),
        }],
        cache_write_retention: CacheWriteRetentionPricing {
            short: None,
            one_hour: Some(MoneyRate::new(6_000_000)),
        },
    }
}

fn raw(json: &str) -> Box<RawValue> {
    RawValue::from_string(json.to_owned()).expect("valid raw JSON")
}

fn number(json: &str) -> Number {
    serde_json::from_str(json).expect("valid exact JSON number")
}

fn complete_openai_completions_compat() -> OpenAiCompletionsCompat {
    let mut chat_template_kwargs = ChatTemplateValues::new();
    chat_template_kwargs.insert(
        "literal_string".into(),
        ChatTemplateKwargValue::String("value".into()),
    );
    chat_template_kwargs.insert(
        "literal_number".into(),
        ChatTemplateKwargValue::Number(Number::from(7)),
    );
    chat_template_kwargs.insert(
        "literal_boolean".into(),
        ChatTemplateKwargValue::Boolean(true),
    );
    chat_template_kwargs.insert("literal_null".into(), ChatTemplateKwargValue::Null);
    chat_template_kwargs.insert(
        "enabled".into(),
        ChatTemplateKwargValue::Variable(ChatTemplateVariable {
            variable: ChatTemplateVariableName::ThinkingEnabled,
            omit_when_off: Some(true),
        }),
    );
    chat_template_kwargs.insert(
        "effort".into(),
        ChatTemplateKwargValue::Variable(ChatTemplateVariable {
            variable: ChatTemplateVariableName::ThinkingEffort,
            omit_when_off: None,
        }),
    );

    let mut chat_template_args = ChatTemplateValues::new();
    chat_template_args.insert(
        "budget".into(),
        ChatTemplateKwargValue::Variable(ChatTemplateVariable {
            variable: ChatTemplateVariableName::ThinkingBudget,
            omit_when_off: Some(false),
        }),
    );

    OpenAiCompletionsCompat {
        supports_store: Some(false),
        supports_developer_role: Some(true),
        supports_reasoning_effort: Some(true),
        supports_usage_in_streaming: Some(true),
        supports_finish_reason: Some(true),
        max_tokens_field: Some(MaxTokensField::MaxCompletionTokens),
        requires_tool_result_name: Some(false),
        requires_assistant_after_tool_result: Some(false),
        requires_thinking_as_text: Some(false),
        requires_reasoning_content_on_assistant_messages: Some(false),
        thinking_format: Some(OpenAiThinkingFormat::OpenAi),
        chat_template_kwargs: Some(chat_template_kwargs),
        chat_template_args: Some(chat_template_args),
        open_router_routing: Some(OpenRouterRouting {
            allow_fallbacks: Some(true),
            require_parameters: Some(true),
            data_collection: Some(OpenRouterDataCollection::Deny),
            zdr: Some(true),
            enforce_distillable_text: Some(false),
            order: Some(vec!["anthropic".into(), "openai".into()]),
            only: Some(vec!["anthropic".into()]),
            ignore: Some(vec!["example".into()]),
            quantizations: Some(vec!["fp16".into(), "int8".into()]),
            sort: Some(OpenRouterSort::Options(OpenRouterSortOptions {
                by: Some("latency".into()),
                partition: NullableString::Null,
            })),
            max_price: Some(OpenRouterMaxPrice {
                prompt: Some(JsonNumberOrString::String("1.25".into())),
                completion: Some(JsonNumberOrString::Number(number("2.125"))),
                image: Some(JsonNumberOrString::String("0.5".into())),
                audio: Some(JsonNumberOrString::Number(Number::from(3))),
                request: Some(JsonNumberOrString::String("0.01".into())),
            }),
            preferred_min_throughput: Some(OpenRouterMetricPreference::Number(Number::from(20))),
            preferred_max_latency: Some(OpenRouterMetricPreference::Percentiles(
                OpenRouterPercentiles {
                    p50: Some(Number::from(1)),
                    p75: Some(Number::from(2)),
                    p90: Some(Number::from(3)),
                    p99: Some(Number::from(4)),
                },
            )),
        }),
        vercel_gateway_routing: Some(VercelGatewayRouting {
            only: Some(vec!["bedrock".into(), "anthropic".into()]),
            order: Some(vec!["anthropic".into(), "openai".into()]),
        }),
        zai_tool_stream: Some(true),
        thinking_token_budget_field: Some(ThinkingTokenBudgetField::ThinkingTokenBudget),
        supports_thinking_token_budget: Some(true),
        supports_strict_mode: Some(true),
        supports_openai_grammar_tools: Some(true),
        cache_control_format: Some(CacheControlFormat::Anthropic),
        send_session_affinity_headers: Some(true),
        deferred_tools_mode: Some(DeferredToolsMode::Kimi),
        session_affinity_format: Some(SessionAffinityFormat::OpenAi),
        supports_long_cache_retention: Some(true),
        extensions: ExtensionMap::new(),
    }
}

#[test]
fn all_identifier_model_ref_and_timestamp_types_round_trip() {
    assert_round_trip(&ProviderId::new("anthropic"));
    assert_round_trip(&ModelId::new("claude"));
    assert_round_trip(&ApiId::new("anthropic-messages"));
    assert_round_trip(&ReplayKind::new("anthropic.messages.thinking-signature"));
    assert_round_trip(&ExtensionId::new("example.feature"));
    assert_round_trip(&MessageId::new("m0"));
    assert_round_trip(&ContentBlockId::new("b0"));
    assert_round_trip(&ToolCallId::new("call_0"));
    assert_round_trip(&ReplayItemId::new("r0"));
    assert_round_trip(&RunId::new("run_0"));
    assert_round_trip(&Timestamp::from_unix_millis(1_777_777_777_000));
    assert_round_trip(&ModelRef::new("anthropic", "claude"));
}

#[test]
fn persisted_anthropic_thinking_json_matches_architecture() {
    // Architecture v2 part 2 §1.4 persisted ContentBlock example; Pi basis:
    // packages/ai/src/api/anthropic-messages.ts.
    let expected = concat!(
        r#"{"type":"thinking","id":"b0","text":"We need to inspect...","#,
        r#""redacted":false,"replayItem":"r0"}"#
    );
    let parsed: ContentBlock = serde_json::from_str(expected).expect("parse §1.4 block");
    assert_eq!(serde_json::to_string(&parsed).unwrap(), expected);
    assert_eq!(parsed.id(), &ContentBlockId::new("b0"));
}

#[test]
fn persisted_anthropic_replay_item_json_matches_architecture() {
    // Architecture v2 part 2 §1.4 persisted ReplayItem example; Pi basis:
    // packages/ai/src/api/anthropic-messages.ts.
    let expected = concat!(
        r#"{"id":"r0","ordinal":0,"target":{"type":"content_block","id":"b0"},"#,
        r#""kind":"anthropic.messages.thinking-signature","#,
        r#""applicability":"exact_provider_api_model","completeness":"complete","#,
        r#""payload":{"encoding":"utf8","data":"EqQBCg...remaining-signature..."}}"#
    );
    let parsed: ReplayItem = serde_json::from_str(expected).expect("parse §1.4 replay item");
    assert_eq!(serde_json::to_string(&parsed).unwrap(), expected);
    assert_eq!(parsed.as_utf8(), Some("EqQBCg...remaining-signature..."));
}

#[test]
fn anthropic_complete_replay_item_is_found_after_message_round_trip() {
    // Architecture v2 part 2 §1.4 persistence and replay lookup; Pi basis:
    // packages/ai/src/api/anthropic-messages.ts. The exact §10.2 assembly
    // proof lives in m1_2_streaming.rs.
    let source = scope("anthropic", "anthropic-messages", "claude", "claude");
    let message = AssistantMessage {
        id: MessageId::new("m0"),
        provider: source.provider.clone(),
        api: source.api.clone(),
        requested_model: source.requested_model.clone(),
        response_model: Some(source.produced_by_model.clone()),
        response_id: Some("msg_provider_0".into()),
        content: vec![ContentBlock::Thinking {
            id: ContentBlockId::new("b0"),
            text: "reasoning".into(),
            redacted: false,
            replay_item: Some(ReplayItemId::new("r0")),
        }],
        replay: ReplayEnvelope {
            schema_version: REPLAY_ENVELOPE_SCHEMA_VERSION,
            source: source.clone(),
            items: vec![replay_item(
                "r0",
                0,
                ReplayTarget::ContentBlock(ContentBlockId::new("b0")),
                "anthropic.messages.thinking-signature",
                OpaquePayload::Utf8("signature".into()),
            )],
        },
        usage: usage(),
        finish: finish(AssistantFinishReason::Stop),
        timestamp: Timestamp::from_unix_millis(10),
    };
    assert_round_trip(&message);
    let item = message
        .replay
        .complete_item_for_block(
            &ContentBlockId::new("b0"),
            "anthropic.messages.thinking-signature",
            &source,
        )
        .expect("complete same-model signature");
    assert_eq!(item.as_utf8(), Some("signature"));
}

#[test]
fn persisted_openai_chat_reasoning_detail_json_matches_architecture() {
    // Architecture v2 part 2 §1.5 persisted replay item; Pi basis:
    // packages/ai/src/api/openai-completions.ts. The exact §10.2 assembly
    // proof lives in m1_2_streaming.rs.
    let expected = concat!(
        r#"{"id":"r0","ordinal":0,"target":{"type":"content_block","id":"b0"},"#,
        r#""kind":"openai.chat.reasoning-detail","#,
        r#""applicability":"exact_provider_api_model","completeness":"complete","#,
        r#""payload":{"encoding":"json_bytes_base64","#,
        r#""data":"eyJ0eXBlIjoicmVhc29uaW5nLmVuY3J5cHRlZCIsImlkIjoicnNfMSIsImRhdGEiOiJvcGFxdWUtQSJ9"}}"#
    );
    let item: ReplayItem = serde_json::from_str(expected).expect("parse §1.5 replay item");
    assert_eq!(serde_json::to_string(&item).unwrap(), expected);
    assert_eq!(
        item.json_bytes().unwrap(),
        br#"{"type":"reasoning.encrypted","id":"rs_1","data":"opaque-A"}"#
    );
}

#[test]
fn persisted_openai_responses_message_round_trip() {
    // Architecture v2 part 2 §1.6 persisted Responses representation; Pi
    // basis: packages/ai/src/api/openai-responses-shared.ts. The exact §10.2
    // assembler proof lives in m1_2_streaming.rs.
    let source = scope("openai", "openai-responses", "gpt", "gpt");
    let message = AssistantMessage {
        id: MessageId::new("m-responses"),
        provider: source.provider.clone(),
        api: source.api.clone(),
        requested_model: source.requested_model.clone(),
        response_model: None,
        response_id: Some("resp_123".into()),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("b1"),
            text: "I found the issue.".into(),
        }],
        replay: ReplayEnvelope {
            schema_version: REPLAY_ENVELOPE_SCHEMA_VERSION,
            source,
            items: vec![replay_item(
                "r1",
                1,
                ReplayTarget::ProviderOutputItem { output_index: 1 },
                "openai.responses.message-identity",
                OpaquePayload::JsonBytes(
                    br#"{"id":"msg_123","phase":"final_answer","block_id":"b1"}"#.to_vec(),
                ),
            )],
        },
        usage: usage(),
        finish: finish(AssistantFinishReason::Stop),
        timestamp: Timestamp::from_unix_millis(20),
    };
    let bytes = serde_json::to_vec(&message).unwrap();
    let restored: AssistantMessage = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored.response_id.as_deref(), Some("resp_123"));
    assert_eq!(restored, message);
}

#[test]
fn bedrock_opaque_bytes_json_encoding_matches_architecture() {
    // Architecture v2 part 2 §1.7 persisted byte payload; Pi basis:
    // packages/ai/src/api/bedrock-converse-stream.ts. The exact §10.2
    // assembly proof lives in m1_2_streaming.rs.
    let payload = OpaquePayload::Bytes(vec![0x01, 0x02, 0xaf, 0x33]);
    let expected = r#"{"encoding":"bytes_base64","data":"AQKvMw=="}"#;
    assert_eq!(serde_json::to_string(&payload).unwrap(), expected);
    let restored: OpaquePayload = serde_json::from_str(expected).unwrap();
    assert_eq!(
        restored.as_bytes(),
        Some([0x01, 0x02, 0xaf, 0x33].as_slice())
    );
    assert_eq!(restored, payload);
}

#[test]
fn google_tool_call_replay_target_round_trip() {
    // Architecture v2 part 2 §1.8; Pi basis: packages/ai/src/api/google-shared.ts.
    let item = replay_item(
        "r0",
        0,
        ReplayTarget::ToolCall(ToolCallId::new("call_123")),
        "google.genai.thought-signature",
        OpaquePayload::Utf8("base64-signature==".into()),
    );
    assert_round_trip(&item);
    assert_eq!(
        serde_json::to_value(&item.target).unwrap(),
        json!({"type": "tool_call", "id": "call_123"})
    );
}

#[test]
fn replay_helpers_reject_incomplete_and_cross_model_items() {
    // Architecture v2 part 2 §1.9 R3–R5.
    let source = scope("google", "google-generative-ai", "gemini-a", "gemini-a");
    let same = scope("google", "google-generative-ai", "gemini-a", "gemini-a");
    let foreign = scope("google", "google-generative-ai", "gemini-b", "gemini-b");
    let mut envelope = ReplayEnvelope::new(source);
    envelope.items.push(replay_item(
        "r0",
        0,
        ReplayTarget::ContentBlock(ContentBlockId::new("b0")),
        "google.genai.thought-signature",
        OpaquePayload::Utf8("sig".into()),
    ));
    let mut incomplete = replay_item(
        "r1",
        1,
        ReplayTarget::ContentBlock(ContentBlockId::new("b0")),
        "google.genai.thought-signature",
        OpaquePayload::Utf8("partial".into()),
    );
    incomplete.completeness = ReplayCompleteness::Incomplete;
    envelope.items.push(incomplete);

    assert_eq!(
        envelope.items_for_block(&ContentBlockId::new("b0")).count(),
        2
    );
    assert!(
        envelope
            .complete_item_for_block(
                &ContentBlockId::new("b0"),
                "google.genai.thought-signature",
                &same,
            )
            .is_some()
    );
    assert!(
        envelope
            .complete_item_for_block(
                &ContentBlockId::new("b0"),
                "google.genai.thought-signature",
                &foreign,
            )
            .is_none()
    );
}

#[test]
fn canonical_message_context_and_conversation_types_round_trip() {
    let tool = ToolSpec {
        schema_version: 1,
        name: "read_file".into(),
        description: "Read one file".into(),
        parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        constrained_sampling: None,
    };
    let user = UserMessage {
        id: MessageId::new("u0"),
        content: vec![ContentBlock::Image {
            id: ContentBlockId::new("image0"),
            data: "AA==".into(),
            mime_type: "image/png".into(),
        }],
        timestamp: Timestamp::from_unix_millis(1),
    };
    let result = ToolResultMessage {
        id: MessageId::new("tr0"),
        tool_call_id: ToolCallId::new("call0"),
        tool_name: "read_file".into(),
        content: vec![ToolResultContent::Text {
            id: ContentBlockId::new("rb0"),
            text: "contents".into(),
        }],
        details: Some(VersionedExtension {
            schema_version: 1,
            value: raw(r#"{"path":"README.md"}"#),
        }),
        usage: Some(usage()),
        added_tool_names: vec!["write_file".into()],
        is_error: false,
        timestamp: Timestamp::from_unix_millis(2),
    };
    let conversation = Conversation {
        schema_version: CONVERSATION_SCHEMA_VERSION,
        system_prompt: Some("system".into()),
        messages: vec![
            Message::User(user.clone()),
            Message::ToolResult(result.clone()),
        ],
    };
    let context = Context {
        schema_version: CONTEXT_SCHEMA_VERSION,
        system_prompt: conversation.system_prompt.clone(),
        messages: conversation.messages.clone(),
        tools: vec![tool],
    };
    assert_round_trip(&conversation);
    assert_round_trip(&context);
    assert_eq!(conversation.messages[0].id(), &user.id);
    assert_eq!(conversation.messages[1].id(), &result.id);
}

#[test]
fn tool_call_spec_and_every_result_content_variant_round_trip() {
    let call = ToolCall {
        id: ToolCallId::new("call0"),
        name: "read_file".into(),
        arguments: json!({"path": "README.md"}),
    };
    let call_block = ContentBlock::ToolCall {
        id: ContentBlockId::new("b-call"),
        call,
    };
    let spec = ToolSpec {
        schema_version: 1,
        name: "read_file".into(),
        description: "Read one file".into(),
        parameters: json!({"type": "object"}),
        constrained_sampling: None,
    };
    let image_result = ToolResultContent::Image {
        id: ContentBlockId::new("result-image"),
        data: "AA==".into(),
        mime_type: "image/png".into(),
    };
    assert_round_trip(&call_block);
    assert_round_trip(&spec);
    assert_round_trip(&image_result);
}

#[test]
fn tool_spec_constrained_sampling_serialized_values_match_pi_contract() {
    // Contract coverage for Architecture v2 part 1 §4.5; Pi basis:
    // packages/ai/src/types.ts:492–519 and
    // packages/ai/test/constrained-sampling.test.ts.
    let base = |constrained_sampling| ToolSpec {
        schema_version: 1,
        name: "sample_tool".into(),
        description: "Sample tool".into(),
        parameters: json!({"type": "object"}),
        constrained_sampling,
    };

    assert_eq!(
        serde_json::to_value(base(None)).unwrap(),
        json!({
            "schema_version": 1,
            "name": "sample_tool",
            "description": "Sample tool",
            "parameters": {"type": "object"}
        })
    );
    assert_eq!(
        serde_json::to_value(base(Some(ConstrainedSampling::Disabled))).unwrap(),
        json!({
            "schema_version": 1,
            "name": "sample_tool",
            "description": "Sample tool",
            "parameters": {"type": "object"},
            "constrained_sampling": false
        })
    );

    for strict in [JsonSchemaStrictMode::Prefer, JsonSchemaStrictMode::Require] {
        let expected = match strict {
            JsonSchemaStrictMode::Prefer => "prefer",
            JsonSchemaStrictMode::Require => "require",
        };
        let constraint =
            ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict });
        assert_eq!(
            serde_json::to_value(&constraint).unwrap(),
            json!({"type": "json_schema", "strict": expected})
        );
        assert_round_trip(&constraint);
    }

    let variants = [
        (GrammarFormat::OpenAiLark, "start: /[a-z]+/".into()),
        (GrammarFormat::OpenAiRegex, "[a-z]+".into()),
    ]
    .into_iter()
    .collect();
    let grammar = ConstrainedSampling::Config(ConstrainedSamplingConfig::Grammar { variants });
    assert_eq!(
        serde_json::to_value(&grammar).unwrap(),
        json!({
            "type": "grammar",
            "variants": {
                "openai_lark": "start: /[a-z]+/",
                "openai_regex": "[a-z]+"
            }
        })
    );
    assert_round_trip(&grammar);
    assert!(serde_json::from_str::<ConstrainedSampling>("true").is_err());
}

#[test]
fn replay_target_applicability_completeness_and_usage_sources_round_trip() {
    for target in [
        ReplayTarget::Message,
        ReplayTarget::ContentBlock(ContentBlockId::new("b0")),
        ReplayTarget::ToolCall(ToolCallId::new("call0")),
        ReplayTarget::ProviderOutputItem { output_index: 7 },
    ] {
        assert_round_trip(&target);
    }
    for applicability in [
        ReplayApplicability::ExactProviderApiModel,
        ReplayApplicability::ExactProviderApi,
        ReplayApplicability::ApiFamily,
    ] {
        assert_round_trip(&applicability);
    }
    for completeness in [ReplayCompleteness::Complete, ReplayCompleteness::Incomplete] {
        assert_round_trip(&completeness);
    }
    for source in [
        UsageSource::ProviderReported,
        UsageSource::Estimated,
        UsageSource::Mixed,
        UsageSource::Unknown,
    ] {
        assert_round_trip(&source);
    }
}

#[test]
fn finish_and_public_error_types_round_trip() {
    for reason in [
        AssistantFinishReason::Stop,
        AssistantFinishReason::Length,
        AssistantFinishReason::ToolUse,
        AssistantFinishReason::Deferred,
        AssistantFinishReason::Error,
        AssistantFinishReason::Aborted,
    ] {
        let value = AssistantFinish {
            reason,
            raw_provider_reason: Some("provider_reason".into()),
            error: Some(PublicError {
                code: "normalized".into(),
                message: "safe".into(),
                retryable: false,
                provider_code: Some("provider".into()),
                status: Some(400),
                request_id: Some("req_0".into()),
            }),
        };
        assert_round_trip(&value);
    }
}

#[test]
fn pricing_types_round_trip_and_use_integer_arithmetic() {
    let pricing = pricing();
    assert_round_trip(&usage());
    assert_round_trip(&pricing);
    let cost = pricing
        .calculate_cost(&usage(), Currency::usd(), CacheWriteRetention::OneHour)
        .unwrap();
    assert_eq!(cost.currency.as_str(), "USD");
    assert_eq!(cost.micros, 7_800);
    assert_round_trip(&cost);
    assert_eq!(usage().total_tokens(), 2_600);
}

#[test]
fn request_wide_pricing_uses_highest_strictly_exceeded_tier() {
    let pricing = ModelPricing {
        default: rates(1_000_000, 0, 0, 0),
        request_wide_tiers: vec![
            RequestWidePriceTier {
                input_tokens_above: 100,
                rates: rates(2_000_000, 0, 0, 0),
            },
            RequestWidePriceTier {
                input_tokens_above: 200,
                rates: rates(3_000_000, 0, 0, 0),
            },
        ],
        cache_write_retention: CacheWriteRetentionPricing::default(),
    };
    let at_boundary = Usage {
        input_tokens: 200,
        output_tokens: 0,
        reasoning_tokens: None,
        cache_read_tokens: None,
        cache_write_tokens: None,
        source: UsageSource::ProviderReported,
    };
    assert_eq!(
        pricing.rates_for(&at_boundary).input,
        MoneyRate::new(2_000_000)
    );
    let above_boundary = Usage {
        input_tokens: 201,
        ..at_boundary
    };
    assert_eq!(
        pricing.rates_for(&above_boundary).input,
        MoneyRate::new(3_000_000)
    );
    assert_eq!(
        MoneyRate::new(u128::MAX).cost_for_tokens(2),
        Err(CostArithmeticError::Overflow)
    );
}

#[test]
fn all_api_model_config_variants_round_trip() {
    let configs = vec![
        ApiModelConfig::OpenAiCompletions(OpenAiCompletionsModelConfig {
            compat: OpenAiCompletionsCompat::default(),
            thinking_levels: empty_level_map(),
            sampling_defaults: OrderedJsonObject::new(),
        }),
        ApiModelConfig::OpenAiResponses(OpenAiResponsesModelConfig {
            compat: OpenAiResponsesCompat::default(),
            thinking_levels: empty_level_map(),
            sampling_defaults: OrderedJsonObject::new(),
        }),
        ApiModelConfig::AnthropicMessages(AnthropicMessagesModelConfig {
            compat: AnthropicMessagesCompat::default(),
            thinking_levels: empty_level_map(),
        }),
        ApiModelConfig::GoogleGenerativeAi(GoogleModelConfig::default()),
        ApiModelConfig::GoogleVertex(GoogleModelConfig::default()),
        ApiModelConfig::BedrockConverse(BedrockModelConfig::default()),
        ApiModelConfig::MistralConversations(MistralModelConfig::default()),
        ApiModelConfig::Custom(CustomApiModelConfig {
            api: ApiId::new("example-api"),
            schema_version: 3,
            value: raw(r#"{"custom":true}"#),
        }),
    ];
    for config in configs {
        let api = config.api_id();
        assert_round_trip(&config);
        assert!(!api.as_str().is_empty());
    }
}

#[test]
fn model_descriptor_and_typed_compat_fields_round_trip() {
    let mut input = BTreeSet::new();
    input.insert(Modality::Text);
    input.insert(Modality::Image);
    let mut output = BTreeSet::new();
    output.insert(Modality::Text);
    let compat = complete_openai_completions_compat();

    let descriptor = ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new("openai", "gpt"),
            display_name: "GPT".into(),
            base_url: Url::parse("https://api.example.test/v1").unwrap(),
            modalities: ModalityCapabilities { input, output },
            limits: ModelLimits {
                context_window: 128_000,
                max_output_tokens: 16_384,
            },
            pricing: pricing(),
            reasoning: true,
            headers: [("x-model".into(), Some("gpt".into()))]
                .into_iter()
                .collect(),
        },
        api: ApiModelConfig::OpenAiCompletions(OpenAiCompletionsModelConfig {
            compat,
            thinking_levels: ThinkingLevelMap {
                off: Some(LevelSupport::Value(OpenAiThinkingValue::Disabled)),
                minimal: Some(LevelSupport::Unsupported),
                low: Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
                    "low".into(),
                ))),
                medium: None,
                high: Some(LevelSupport::Value(OpenAiThinkingValue::TokenBudget(8_192))),
                xhigh: None,
                max: None,
            },
            sampling_defaults: [("seed".into(), Value::from(7))].into_iter().collect(),
        }),
        extensions: ExtensionMap::new(),
    };
    assert_round_trip(&descriptor);
}

#[test]
fn openai_completions_compat_serialized_values_match_pi_contract() {
    // Contract coverage for Architecture v2 part 2 §5.1; Pi basis:
    // packages/ai/src/types.ts:541–624. This exact value assertion covers
    // every known lowering-critical OpenAI Completions compatibility field.
    let compat = complete_openai_completions_compat();
    assert_eq!(
        serde_json::to_value(&compat).unwrap(),
        json!({
            "supports_store": false,
            "supports_developer_role": true,
            "supports_reasoning_effort": true,
            "supports_usage_in_streaming": true,
            "supports_finish_reason": true,
            "max_tokens_field": "max_completion_tokens",
            "requires_tool_result_name": false,
            "requires_assistant_after_tool_result": false,
            "requires_thinking_as_text": false,
            "requires_reasoning_content_on_assistant_messages": false,
            "thinking_format": "openai",
            "chat_template_kwargs": {
                "literal_string": "value",
                "literal_number": 7,
                "literal_boolean": true,
                "literal_null": null,
                "enabled": {"$var": "thinking.enabled", "omitWhenOff": true},
                "effort": {"$var": "thinking.effort"}
            },
            "chat_template_args": {
                "budget": {"$var": "thinking.budget", "omitWhenOff": false}
            },
            "open_router_routing": {
                "allow_fallbacks": true,
                "require_parameters": true,
                "data_collection": "deny",
                "zdr": true,
                "enforce_distillable_text": false,
                "order": ["anthropic", "openai"],
                "only": ["anthropic"],
                "ignore": ["example"],
                "quantizations": ["fp16", "int8"],
                "sort": {"by": "latency", "partition": null},
                "max_price": {
                    "prompt": "1.25",
                    "completion": 2.125,
                    "image": "0.5",
                    "audio": 3,
                    "request": "0.01"
                },
                "preferred_min_throughput": 20,
                "preferred_max_latency": {"p50": 1, "p75": 2, "p90": 3, "p99": 4}
            },
            "vercel_gateway_routing": {
                "only": ["bedrock", "anthropic"],
                "order": ["anthropic", "openai"]
            },
            "zai_tool_stream": true,
            "thinking_token_budget_field": "thinking_token_budget",
            "supports_thinking_token_budget": true,
            "supports_strict_mode": true,
            "supports_openai_grammar_tools": true,
            "cache_control_format": "anthropic",
            "send_session_affinity_headers": true,
            "deferred_tools_mode": "kimi",
            "session_affinity_format": "openai",
            "supports_long_cache_retention": true,
            "extensions": {}
        })
    );
    assert_round_trip(&compat);

    let thinking_formats = [
        (OpenAiThinkingFormat::OpenAi, "openai"),
        (OpenAiThinkingFormat::OpenRouter, "openrouter"),
        (OpenAiThinkingFormat::DeepSeek, "deepseek"),
        (OpenAiThinkingFormat::Together, "together"),
        (OpenAiThinkingFormat::Baseten, "baseten"),
        (OpenAiThinkingFormat::Zai, "zai"),
        (OpenAiThinkingFormat::Qwen, "qwen"),
        (OpenAiThinkingFormat::ChatTemplate, "chat-template"),
        (OpenAiThinkingFormat::QwenChatTemplate, "qwen-chat-template"),
        (OpenAiThinkingFormat::StringThinking, "string-thinking"),
        (OpenAiThinkingFormat::AntLing, "ant-ling"),
    ];
    for (value, expected) in thinking_formats {
        assert_eq!(serde_json::to_value(value).unwrap(), Value::from(expected));
    }

    for (value, expected) in [
        (SessionAffinityFormat::OpenAi, "openai"),
        (SessionAffinityFormat::OpenAiNoSession, "openai-nosession"),
        (SessionAffinityFormat::OpenRouter, "openrouter"),
    ] {
        assert_eq!(serde_json::to_value(value).unwrap(), Value::from(expected));
    }
    assert_eq!(
        serde_json::to_value(OpenRouterSort::Name("price".into())).unwrap(),
        Value::from("price")
    );
}

#[test]
fn openrouter_max_price_decimal_number_is_lossless_and_distinct_from_string() {
    // Contract coverage for Architecture v2 part 2 §5.1; Pi basis:
    // packages/ai/src/types.ts:751–763 (`number | string`). This is catalog
    // compatibility data rather than one of the named §10 conformance tests.
    const DECIMAL: &str = "0.123456789012345678901234567890123456789";

    let numeric: JsonNumberOrString = serde_json::from_str(DECIMAL).unwrap();
    assert!(matches!(numeric, JsonNumberOrString::Number(_)));
    assert_eq!(serde_json::to_string(&numeric).unwrap(), DECIMAL);
    assert_round_trip(&numeric);

    let quoted = format!(r#""{DECIMAL}""#);
    let string: JsonNumberOrString = serde_json::from_str(&quoted).unwrap();
    assert!(matches!(string, JsonNumberOrString::String(_)));
    assert_eq!(serde_json::to_string(&string).unwrap(), quoted);
    assert_ne!(numeric, string);
}

#[test]
fn anthropic_compat_every_architecture_field_round_trips() {
    let compat = AnthropicMessagesCompat {
        supports_eager_tool_input_streaming: Some(true),
        supports_long_cache_retention: Some(true),
        send_session_affinity_headers: Some(true),
        supports_cache_control_on_tools: Some(true),
        supports_temperature: Some(false),
        force_adaptive_thinking: Some(true),
        allow_empty_signature: Some(false),
        supports_strict_tools: Some(true),
        supports_tool_references: Some(true),
        allowed_fallback_models: vec![AnthropicFallbackModel {
            provider: ProviderId::new("anthropic"),
            model: "fallback".into(),
            cost: pricing(),
        }],
        extensions: ExtensionMap::new(),
    };
    assert_round_trip(&compat);
    assert_round_trip(&AnthropicThinkingValue::Effort(AnthropicEffort::Xhigh));
}

#[test]
fn catalog_unknown_extensions_round_trip() {
    // §10.7 `catalog_unknown_extensions_round_trip`; Pi basis:
    // packages/ai/src/types.ts model metadata plus Architecture v2 replacement.
    let extension = VersionedExtension {
        schema_version: 9,
        value: raw(r#"{"z":1,"a":[true,null]}"#),
    };
    let mut extensions = ExtensionMap::new();
    extensions.insert(ExtensionId::new("third.party/feature"), extension);
    let bytes = serde_json::to_vec(&extensions).unwrap();
    let restored: ExtensionMap = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored, extensions);
    assert_eq!(
        restored[&ExtensionId::new("third.party/feature")]
            .value
            .get(),
        r#"{"z":1,"a":[true,null]}"#
    );
}

#[test]
fn auth_provider_extra_fields_round_trip() {
    // §10.7 `auth_provider_extra_fields_round_trip`; Pi basis:
    // packages/ai/src/auth/types.ts and Architecture v2 part 2 §6.6.
    let values = [
        ProviderOAuthExtra::None,
        ProviderOAuthExtra::Radius {
            gateway_url: Url::parse("https://radius.example.test/").unwrap(),
            organization_id: Some("org".into()),
        },
        ProviderOAuthExtra::GitHubCopilot {
            api_endpoint: Url::parse("https://copilot.example.test/").unwrap(),
            account_id: Some("account".into()),
        },
        ProviderOAuthExtra::OpenAiCodex {
            account_id: "account".into(),
        },
        ProviderOAuthExtra::Custom {
            schema: ExtensionId::new("example.oauth"),
            schema_version: 2,
            value: raw(r#"{"tenant":"one","nested":{"kept":true}}"#),
        },
    ];
    for value in values {
        assert_round_trip(&value);
    }
}

#[test]
fn secret_types_are_not_serializable_and_redact_debug() {
    assert_not_impl_any!(SecretString: serde::Serialize);
    assert_not_impl_any!(OAuthCredential: serde::Serialize);

    let credential = OAuthCredential {
        access: SecretString::new("access-token-never-print"),
        refresh: SecretString::new("refresh-token-never-print"),
        expires_at: Timestamp::from_unix_millis(100),
        extra: ProviderOAuthExtra::OpenAiCodex {
            account_id: "account".into(),
        },
    };
    let debug = format!("{credential:?}");
    assert!(!debug.contains("access-token-never-print"));
    assert!(!debug.contains("refresh-token-never-print"));
    assert!(debug.contains("[REDACTED]"));
    assert_eq!(
        credential.access.expose_secret(),
        "access-token-never-print"
    );
}
