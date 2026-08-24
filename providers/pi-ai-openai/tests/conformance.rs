use futures_util::{StreamExt, stream};
use http::{HeaderMap, HeaderValue};
use pi_ai::{
    ApiFamily, ApiModelConfig, ApiRequestOptions, AssistantEvent, AssistantFinish,
    AssistantFinishReason, AssistantMessage, AuthAnswer, AuthError, AuthEvent,
    AuthHostCapabilities, AuthInteraction, AuthInteractionError, AuthPrompt, AuthResolver,
    CONTEXT_SAFETY_TOKENS, CacheControlFormat, CacheRetention, CacheWriteRetention,
    CacheWriteRetentionPricing, CancellationToken, ChatTemplateKwargValue, ChatTemplateValues,
    ChatTemplateVariable, ChatTemplateVariableName, CommonModelDescriptor, ConstrainedSampling,
    ConstrainedSamplingConfig, ContentBlock, ContentBlockId, Context, Credential, Currency,
    DefaultRetryClassifier, DeferredToolsMode, EncodeContext, GrammarFormat, HeaderMapSpec,
    HeaderTransform, HeaderTransformContext, HttpRequest, HttpResponse, HttpTransport,
    InMemoryCredentialStore, JsonSchemaStrictMode, LevelSupport, LocalAuthInteraction,
    LocalAuthResolver, LocalBoxFuture, LocalDefaultRetryClassifier, LocalHeaderTransform,
    LocalHttpResponse, LocalHttpTransport, LocalModelRuntime, LocalModels, LocalOAuthAuth,
    LocalProviderRegistration, LocalRedirectReceiver, LocalResolveAuthRequest,
    LocalResolvedApiRequest, MaxTokensField, Message, MessageId, MiddlewareError, Modality,
    ModalityCapabilities, ModelDescriptor, ModelLimits, ModelPricing, ModelRef, ModelRequest,
    ModelRuntime, Models, MoneyRate, OAuthAuth, OAuthCredential, OPENAI_CHAT_REASONING_DETAIL_KIND,
    OPENAI_CHAT_REASONING_FIELD_KIND, OpenAiAllowedToolsMode, OpenAiCompletions,
    OpenAiCompletionsCompat, OpenAiCompletionsHandoff, OpenAiCompletionsModelConfig,
    OpenAiCompletionsOptions, OpenAiCompletionsToolChoice, OpenAiReasoningEffortProvenance,
    OpenAiReasoningMode, OpenAiReasoningPlan, OpenAiReasoningTokenBudget, OpenAiResponses,
    OpenAiResponsesOptions, OpenAiThinkingFormat, OpenAiThinkingValue, OrderedJsonArray,
    OrderedJsonObject, OrderedJsonValue, OrderedJsonWriter, ProviderOAuthExtra,
    ProviderRegistration, RedirectArrival, RedirectReceiver, RedirectReceiverRequest,
    ReplayApplicability, ReplayCompleteness, ReplayEnvelope, ReplayItem, ReplayItemId, ReplayKind,
    ReplayScope, ReplayTarget, ResolveAuthRequest, ResolvedApiRequest, RetryPolicy, SecretString,
    SendBoxFuture, SessionAffinityFormat, SimpleGenerationOptions, SimpleLoweringContext,
    ThinkingBudgets, ThinkingLevelMap, ThinkingTokenBudgetField, Timestamp, TokenPriceRates,
    ToolCall, ToolCallId, ToolChoice, ToolResultContent, ToolResultMessage, ToolSpec,
    TransportError, TypedModelDescriptor, Usage, UsageSource, UserMessage, estimate_context_tokens,
    import_legacy_openai_chat_tool_signatures, openai_grammar_tool_input_properties,
    parse_ordered_json, resolve_openai_completions_compat, transform_context_for_model,
};
use pi_ai_openai::{
    LocalOpenRouterOAuth, OpenAiCompletionsDecodeContext, OpenRouterOAuth,
    decode_openai_completions_sse, deepseek_models, deepseek_provider_with_api,
    local_openai_completions_api, local_openai_provider, local_openai_responses_api,
    openai_completions_api, openai_provider, openai_responses_api, openrouter_models,
    openrouter_provider_with_api,
};
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use url::Url;

const FIXTURE_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../fixtures/openai-completions"
);
const CREDENTIAL_FIXTURE_ROOT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../fixtures/credential-backed/openai-completions"
);
const FIXTURE_AUTHORIZATION: &str = "Bearer fixture-secret-never-captured";

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/api/openai-completions.ts` and the captured M4 corpus.
#[test]
fn wire_openai_completions_pi_exact() {
    let mut case_dirs = fs::read_dir(FIXTURE_ROOT)
        .expect("fixture root")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    case_dirs.sort();
    assert_eq!(case_dirs.len(), 28, "captured OpenAI fixture count changed");

    for case_dir in case_dirs {
        assert_fixture_case(&case_dir);
    }

    let mut credential_case_dirs = fs::read_dir(CREDENTIAL_FIXTURE_ROOT)
        .expect("credential-backed OpenAI fixture root")
        .map(|entry| entry.expect("credential-backed fixture entry").path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    credential_case_dirs.sort();
    assert_eq!(
        credential_case_dirs.len(),
        4,
        "credential-backed OpenAI fixture count changed"
    );
    for case_dir in credential_case_dirs {
        assert_fixture_case(&case_dir);
    }

    assert_native_tool_choice_wire_bodies();
    assert_baseten_wire_case();
    assert_history_only_tools_omit_tool_stream();
    assert_prompt_cache_uses_catalog_model_base_url();
    assert_zero_max_tokens_is_omitted();
    assert_empty_assistant_omission_preserves_tool_result_bridge();
}

/// Architecture v2 part 2 §10.2 and §10.8; pinned Pi basis:
/// `packages/ai/test/openai-completions-reasoning-details.test.ts`.
#[test]
fn openai_chat_reasoning_details_turn_two_pi_exact() {
    for case_name in [
        "signed-thinking-replay",
        "redacted-encrypted-reasoning-replay",
    ] {
        assert_fixture_case(&Path::new(FIXTURE_ROOT).join(case_name));
    }
}

/// Architecture v2 part 2 §10.5; pinned Pi basis:
/// `packages/ai/src/api/openai-completions.ts` reasoning-field priority.
#[test]
fn openai_chat_reasoning_field_name_is_preserved() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"reasoning_content":"one","reasoning":"duplicate"},"finish_reason":null}]}

data: {"id":"chat-1","model":"fixture","choices":[{"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#,
        decode_context(),
    );
    let message = terminal_message(&events);
    let field = message
        .replay
        .items
        .iter()
        .find(|item| item.kind.as_str() == OPENAI_CHAT_REASONING_FIELD_KIND)
        .and_then(ReplayItem::as_utf8);
    assert_eq!(field, Some("reasoning_content"));
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking { text, .. } if text == "one"
    ));

    let mut opencode_go = decode_context();
    opencode_go.provider = "opencode-go".into();
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-opencode-go","model":"fixture","choices":[{"delta":{"reasoning":"think"},"finish_reason":"stop"}]}

data: [DONE]

"#,
        opencode_go,
    );
    let field = terminal_message(&events)
        .replay
        .items
        .iter()
        .find(|item| item.kind.as_str() == OPENAI_CHAT_REASONING_FIELD_KIND)
        .and_then(ReplayItem::as_utf8);
    assert_eq!(field, Some("reasoning_content"));
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// structured details are serialized with `JSON.stringify` on turn two.
#[test]
fn openai_chat_reasoning_details_replay_exact_json() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"reasoning_details":[{"type":"reasoning.summary","id":"s1","index":0,"summary":"first"},{"type":"reasoning.encrypted","id":"e1","data":"cipher"}]},"finish_reason":"stop"}]}

data: [DONE]

"#,
        decode_context(),
    );
    let items = &terminal_message(&events).replay.items;
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].json_bytes().expect("summary detail"),
        br#"{"type":"reasoning.summary","id":"s1","index":0,"summary":"first"}"#
    );
    assert_eq!(
        items[1].json_bytes().expect("encrypted detail"),
        br#"{"type":"reasoning.encrypted","id":"e1","data":"cipher"}"#
    );
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `signedReasoningDetails ?? legacyReasoningDetails` in message conversion.
#[test]
fn openai_chat_block_signature_precedes_legacy_tool_signature() {
    let mut message = assistant_with_thinking_detail();
    let imported = import_legacy_openai_chat_tool_signatures(
        &mut message,
        [(ToolCallId::new("call-1"), legacy_encrypted_detail())],
    )
    .expect("legacy import");
    assert_eq!(imported, 0);
    assert_eq!(message.replay.items.len(), 1);
    assert!(matches!(
        message.replay.items[0].target,
        ReplayTarget::ContentBlock(_)
    ));
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `parseLegacyEncryptedReasoningDetail` in OpenAI message conversion.
#[test]
fn openai_chat_legacy_tool_signature_imports_as_replay_item() {
    let mut message = assistant_with_tool_call();
    let imported = import_legacy_openai_chat_tool_signatures(
        &mut message,
        [
            (ToolCallId::new("malformed"), "{not-json".into()),
            (
                ToolCallId::new("wrong-kind"),
                r#"{"type":"reasoning.summary","id":"summary-1","summary":"ignored"}"#.into(),
            ),
            (
                ToolCallId::new("missing-id"),
                r#"{"type":"reasoning.encrypted","data":"ignored"}"#.into(),
            ),
            (
                ToolCallId::new("invalid-common-field"),
                r#"{"type":"reasoning.encrypted","id":"encrypted-2","data":"ignored","format":null}"#
                    .into(),
            ),
            (ToolCallId::new("call-1"), legacy_encrypted_detail()),
        ],
    )
    .expect("legacy import");
    assert_eq!(imported, 1);
    assert!(matches!(
        &message.replay.items[0].target,
        ReplayTarget::ToolCall(id) if id.as_str() == "call-1"
    ));
    assert_eq!(
        message.replay.items[0].json_bytes().expect("legacy detail"),
        legacy_encrypted_detail().as_bytes()
    );
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `requiresThinkingAsText` branch in `convertMessages`.
#[test]
fn openai_chat_thinking_as_text_compat() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.requires_thinking_as_text = Some(true);
    let mut context = Context::new(None);
    let mut message = assistant_with_thinking_detail();
    message.content.push(ContentBlock::Text {
        id: ContentBlockId::new("answer"),
        text: "visible".into(),
    });
    context.messages.push(Message::Assistant(message));
    let bytes = encode_direct(
        &model,
        &context,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    );
    let wire = std::str::from_utf8(&bytes).expect("wire UTF-8");
    assert!(wire.contains(
        r#""content":[{"type":"text","text":"thought"},{"type":"text","text":"visible"}]"#
    ));
    assert!(wire.contains("reasoning_details"));
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// `requiresReasoningContentOnAssistantMessages` in message conversion.
#[test]
fn openai_chat_reasoning_content_required_compat() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config
        .compat
        .requires_reasoning_content_on_assistant_messages = Some(true);
    let mut context = Context::new(None);
    context
        .messages
        .push(Message::Assistant(assistant_with_tool_call()));
    let bytes = encode_direct(
        &model,
        &context,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    );
    let wire = std::str::from_utf8(&bytes).expect("wire UTF-8");
    assert!(wire.contains(r#""reasoning_content":"""#));
}

/// Architecture v2 part 2 §10.5; pinned Pi basis: `detectCompat`.
#[test]
fn openai_compat_is_detected_from_effective_base_url() {
    let compat = resolve_openai_completions_compat(
        &Url::parse("https://openrouter.ai/api/v1").expect("URL"),
        &OpenAiCompletionsCompat::default(),
    );
    assert_eq!(
        compat.thinking_format,
        Some(OpenAiThinkingFormat::OpenRouter)
    );
    assert_eq!(compat.supports_developer_role, Some(false));
}

/// Architecture v2 part 2 §10.5; pinned Pi basis: explicit compat overlay.
#[test]
fn openai_model_compat_overrides_url_detection() {
    let overrides = OpenAiCompletionsCompat {
        supports_store: Some(true),
        thinking_format: Some(OpenAiThinkingFormat::StringThinking),
        ..Default::default()
    };
    let compat = resolve_openai_completions_compat(
        &Url::parse("https://api.deepseek.com").expect("URL"),
        &overrides,
    );
    assert_eq!(compat.supports_store, Some(true));
    assert_eq!(
        compat.thinking_format,
        Some(OpenAiThinkingFormat::StringThinking)
    );
}

/// Architecture v2 part 2 §10.5; pinned Pi basis: `maxTokensField` branch.
#[test]
fn openai_max_tokens_field_matches_compat() {
    let model = base_fixture_model();
    let context = one_user_context();
    let mut options =
        default_full_options(OpenAiReasoningPlan::disabled(), OrderedJsonObject::new());
    options.max_tokens = Some(17);
    options.max_tokens_field = MaxTokensField::MaxCompletionTokens;
    let wire = encode_options(&model, &context, options);
    assert!(
        std::str::from_utf8(&wire)
            .expect("wire UTF-8")
            .contains(r#""max_completion_tokens":17"#)
    );
}

/// Architecture v2 part 2 §10.5; pinned Pi basis: OpenRouter reasoning shape.
#[test]
fn openai_reasoning_format_matches_compat() {
    let mut model = base_fixture_model();
    model.common.base_url = Url::parse("https://openrouter.ai/api/v1").expect("URL");
    let bytes = encode_direct(
        &model,
        &one_user_context(),
        OpenAiReasoningPlan {
            mode: OpenAiReasoningMode::OpenRouter {
                effort: "high".into(),
            },
            token_budget: None,
        },
        OrderedJsonObject::new(),
    );
    assert!(
        std::str::from_utf8(&bytes)
            .expect("wire UTF-8")
            .contains(r#""reasoning":{"effort":"high"}"#)
    );

    let mut ant_ling = base_fixture_model();
    ant_ling.common.base_url = Url::parse("https://api.ant-ling.com/v1").expect("Ant Ling URL");
    let ApiModelConfig::OpenAiCompletions(config) = &mut ant_ling.api else {
        unreachable!()
    };
    config.compat.thinking_format = Some(OpenAiThinkingFormat::AntLing);

    let mapped_options = lower_simple_options(
        &ant_ling,
        &one_user_context(),
        &SimpleGenerationOptions {
            reasoning: Some(pi_ai::ReasoningLevel::High),
            ..Default::default()
        },
    );
    assert_eq!(
        mapped_options.reasoning.mode,
        OpenAiReasoningMode::ReasoningEffort {
            effort: "high".into(),
            provenance: OpenAiReasoningEffortProvenance::ModelMapping,
        }
    );
    let mapped = encode_options(&ant_ling, &one_user_context(), mapped_options);
    assert!(
        std::str::from_utf8(&mapped)
            .expect("mapped Ant Ling wire")
            .contains(r#""reasoning":{"effort":"high"}"#)
    );

    let mut unmapped_ant_ling = ant_ling.clone();
    let ApiModelConfig::OpenAiCompletions(config) = &mut unmapped_ant_ling.api else {
        unreachable!()
    };
    config.thinking_levels.medium = None;
    let unmapped_options = lower_simple_options(
        &unmapped_ant_ling,
        &one_user_context(),
        &SimpleGenerationOptions {
            reasoning: Some(pi_ai::ReasoningLevel::Medium),
            ..Default::default()
        },
    );
    assert_eq!(
        unmapped_options.reasoning.mode,
        OpenAiReasoningMode::ReasoningEffort {
            effort: "medium".into(),
            provenance: OpenAiReasoningEffortProvenance::RequestedLevel,
        }
    );
    let unmapped = encode_options(&unmapped_ant_ling, &one_user_context(), unmapped_options);
    assert!(
        !std::str::from_utf8(&unmapped)
            .expect("unmapped Ant Ling wire")
            .contains(r#""reasoning":"#)
    );
}

/// Architecture v2 part 2 §10.5; pinned Pi basis:
/// `packages/ai/test/openai-completions-thinking-token-budget.test.ts`.
#[test]
fn openai_thinking_budget_field_matches_compat() {
    let mut model = base_fixture_model();
    model.common.limits.max_output_tokens = 16_384;
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.thinking_format = Some(OpenAiThinkingFormat::Zai);
    config.compat.supports_reasoning_effort = Some(true);
    config.compat.supports_thinking_token_budget = Some(true);
    let simple = SimpleGenerationOptions {
        max_output_tokens: Some(16_384),
        reasoning: Some(pi_ai::ReasoningLevel::Medium),
        thinking_budgets: Some(ThinkingBudgets {
            medium: Some(4_096),
            ..Default::default()
        }),
        ..Default::default()
    };
    let options = lower_simple_options(&model, &one_user_context(), &simple);
    assert_eq!(
        options.reasoning.mode,
        OpenAiReasoningMode::ReasoningEffort {
            effort: "medium".into(),
            provenance: OpenAiReasoningEffortProvenance::ModelMapping,
        }
    );
    assert_eq!(
        options.reasoning.token_budget,
        Some(OpenAiReasoningTokenBudget {
            field: ThinkingTokenBudgetField::ThinkingTokenBudget,
            budget: 4_096,
        })
    );
    let bytes = encode_options(&model, &one_user_context(), options);
    assert!(
        std::str::from_utf8(&bytes)
            .expect("wire UTF-8")
            .contains(
                r#""thinking":{"type":"enabled","clear_thinking":false},"reasoning_effort":"medium","thinking_token_budget":4096"#
            )
    );

    let mut no_budget_field = model.clone();
    let ApiModelConfig::OpenAiCompletions(config) = &mut no_budget_field.api else {
        unreachable!()
    };
    config.compat.supports_thinking_token_budget = Some(false);
    let options = lower_simple_options(&no_budget_field, &one_user_context(), &simple);
    assert_eq!(options.reasoning.token_budget, None);

    let off = lower_simple_options(
        &model,
        &one_user_context(),
        &SimpleGenerationOptions::default(),
    );
    assert_eq!(off.reasoning, OpenAiReasoningPlan::disabled());
    assert!(
        !std::str::from_utf8(&encode_options(&model, &one_user_context(), off))
            .expect("off wire")
            .contains("thinking_token_budget")
    );

    for level in [pi_ai::ReasoningLevel::Xhigh, pi_ai::ReasoningLevel::Max] {
        let extended = lower_simple_options(
            &model,
            &one_user_context(),
            &SimpleGenerationOptions {
                max_output_tokens: Some(16_384),
                reasoning: Some(level),
                thinking_budgets: Some(ThinkingBudgets {
                    high: Some(8_192),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(
            extended.reasoning.token_budget.map(|budget| budget.budget),
            Some(8_192)
        );
    }

    let answer_room = lower_simple_options(
        &model,
        &one_user_context(),
        &SimpleGenerationOptions {
            max_output_tokens: Some(16_384),
            reasoning: Some(pi_ai::ReasoningLevel::High),
            ..Default::default()
        },
    );
    assert_eq!(
        answer_room
            .reasoning
            .token_budget
            .map(|budget| budget.budget),
        Some(15_360)
    );
    let caller_ceiling = lower_simple_options(
        &model,
        &one_user_context(),
        &SimpleGenerationOptions {
            max_output_tokens: Some(4_096),
            reasoning: Some(pi_ai::ReasoningLevel::High),
            thinking_budgets: Some(ThinkingBudgets {
                high: Some(8_192),
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    assert_eq!(
        caller_ceiling
            .reasoning
            .token_budget
            .map(|budget| budget.budget),
        Some(3_072)
    );

    for field in [
        ThinkingTokenBudgetField::ThinkingBudget,
        ThinkingTokenBudgetField::ThinkingBudgetTokens,
    ] {
        let mut field_model = model.clone();
        let ApiModelConfig::OpenAiCompletions(config) = &mut field_model.api else {
            unreachable!()
        };
        config.compat.thinking_format = Some(OpenAiThinkingFormat::Qwen);
        config.compat.thinking_token_budget_field = Some(field);
        let options = lower_simple_options(&field_model, &one_user_context(), &simple);
        assert_eq!(
            options.reasoning.token_budget,
            Some(OpenAiReasoningTokenBudget {
                field,
                budget: 4_096
            })
        );
        let bytes = encode_options(&field_model, &one_user_context(), options);
        let wire = std::str::from_utf8(&bytes).expect("field wire");
        assert!(wire.contains(match field {
            ThinkingTokenBudgetField::ThinkingBudget => r#""thinking_budget":4096"#,
            ThinkingTokenBudgetField::ThinkingBudgetTokens => {
                r#""thinking_budget_tokens":4096"#
            }
            ThinkingTokenBudgetField::ThinkingTokenBudget => unreachable!(),
        }));
        assert!(!wire.contains(r#""thinking_token_budget":4096"#));
    }

    for format in [OpenAiThinkingFormat::Zai, OpenAiThinkingFormat::Qwen] {
        let mut budget_mapped = model.clone();
        let ApiModelConfig::OpenAiCompletions(config) = &mut budget_mapped.api else {
            unreachable!()
        };
        config.compat.thinking_format = Some(format);
        config.thinking_levels.medium =
            Some(LevelSupport::Value(OpenAiThinkingValue::TokenBudget(4_096)));
        let options = lower_simple_options(&budget_mapped, &one_user_context(), &simple);
        assert_eq!(options.reasoning.mode, OpenAiReasoningMode::Enabled);
        let body = encode_options(&budget_mapped, &one_user_context(), options);
        let wire = std::str::from_utf8(&body).expect("budget-mapped wire");
        match format {
            OpenAiThinkingFormat::Zai => {
                assert!(wire.contains(r#""thinking":{"type":"enabled","clear_thinking":false}"#));
            }
            OpenAiThinkingFormat::Qwen => assert!(wire.contains(r#""enable_thinking":true"#)),
            _ => unreachable!(),
        }
        assert!(wire.contains(r#""thinking_token_budget":4096"#));
        assert!(!wire.contains("disabled"));
    }
}

/// Architecture v2 part 2 §3.6; pinned Pi basis:
/// `buildChatTemplateValues` and `clampThinkingBudgetToAnswerRoom`.
#[test]
fn openai_chat_template_budget_variable_uses_clamped_simple_budget() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.thinking_format = Some(OpenAiThinkingFormat::ChatTemplate);
    config.compat.chat_template_kwargs = Some(ChatTemplateValues::from_iter([
        (
            "enable_thinking".into(),
            ChatTemplateKwargValue::Variable(ChatTemplateVariable {
                variable: ChatTemplateVariableName::ThinkingEnabled,
                omit_when_off: None,
            }),
        ),
        (
            "thinking_budget".into(),
            ChatTemplateKwargValue::Variable(ChatTemplateVariable {
                variable: ChatTemplateVariableName::ThinkingBudget,
                omit_when_off: Some(true),
            }),
        ),
    ]));
    let simple = SimpleGenerationOptions {
        max_output_tokens: Some(2_000),
        reasoning: Some(pi_ai::ReasoningLevel::High),
        thinking_budgets: Some(ThinkingBudgets {
            high: Some(9_000),
            ..Default::default()
        }),
        ..Default::default()
    };
    let options = lower_simple_options(&model, &one_user_context(), &simple);
    assert_eq!(
        options.reasoning.mode,
        OpenAiReasoningMode::ChatTemplate {
            kwargs: OrderedJsonObject::from_iter([
                ("enable_thinking", OrderedJsonValue::from(true)),
                ("thinking_budget", OrderedJsonValue::from(976)),
            ]),
            reasoning_effort: None,
        }
    );
    let off = lower_simple_options(
        &model,
        &one_user_context(),
        &SimpleGenerationOptions::default(),
    );
    let off_wire: Value = serde_json::from_slice(&encode_options(&model, &one_user_context(), off))
        .expect("off chat-template wire");
    assert_eq!(
        off_wire["chat_template_kwargs"],
        serde_json::json!({"enable_thinking": false})
    );
}

/// Architecture v2 part 2 §10.5; pinned Pi basis:
/// `packages/ai/test/sampling-options.test.ts` named/overlay provenance.
#[test]
fn openai_sampling_params_merge_after_named_fields() {
    let model = base_fixture_model();
    let prefix = r#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true}"#;

    let mut named = default_full_options(OpenAiReasoningPlan::disabled(), OrderedJsonObject::new());
    named.temperature = Some(0.25);
    assert_eq!(
        encode_options(&model, &one_user_context(), named),
        format!(r#"{prefix},"temperature":0.25,"reasoning_effort":"none"}}"#).into_bytes()
    );

    let overlay = OrderedJsonObject::from_iter([
        ("temperature", OrderedJsonValue::from(0.75)),
        ("top_p", OrderedJsonValue::from(0.6)),
    ]);
    assert_eq!(
        encode_direct(
            &model,
            &one_user_context(),
            OpenAiReasoningPlan::disabled(),
            overlay.clone(),
        ),
        format!(r#"{prefix},"reasoning_effort":"none","temperature":0.75,"top_p":0.6}}"#)
            .into_bytes()
    );

    let mut overridden = default_full_options(OpenAiReasoningPlan::disabled(), overlay);
    overridden.temperature = Some(0.25);
    assert_eq!(
        encode_options(&model, &one_user_context(), overridden),
        format!(r#"{prefix},"temperature":0.75,"reasoning_effort":"none","top_p":0.6}}"#)
            .into_bytes()
    );
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/api/constrained-sampling.ts:makeStrictJsonSchema` and
/// `resolveJsonSchemaStrictSampling`.
#[test]
fn openai_strict_schema_normalization_and_error_policy_match_pi() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_strict_mode = Some(true);
    let mut context = one_user_context();
    context.tools.push(tool_spec(
        "normalized",
        serde_json::json!({
            "type": "object",
            "properties": {
                "required_value": { "type": "string" },
                "optional_value": { "type": "integer" }
            },
            "required": ["required_value"]
        }),
        Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: JsonSchemaStrictMode::Require,
            },
        )),
    ));
    let wire: Value = serde_json::from_slice(&encode_direct(
        &model,
        &context,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    ))
    .expect("wire JSON");
    let function = &wire["tools"][0]["function"];
    assert_eq!(function["strict"], true);
    assert_eq!(
        function["parameters"]["required"],
        serde_json::json!(["required_value", "optional_value"])
    );
    assert_eq!(
        function["parameters"]["properties"]["optional_value"],
        serde_json::json!({"anyOf":[{"type":"integer"},{"type":"null"}]})
    );
    assert_eq!(function["parameters"]["additionalProperties"], false);

    let unsupported = serde_json::json!({
        "type": "object",
        "properties": { "value": { "$ref": "#/$defs/value" } },
        "$defs": { "value": { "type": "string" } }
    });
    context.tools = vec![tool_spec(
        "preferred",
        unsupported.clone(),
        Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema {
                strict: JsonSchemaStrictMode::Prefer,
            },
        )),
    )];
    let wire: Value = serde_json::from_slice(&encode_direct(
        &model,
        &context,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    ))
    .expect("preferred fallback wire");
    assert_eq!(wire["tools"][0]["function"]["strict"], false);
    assert_eq!(wire["tools"][0]["function"]["parameters"], unsupported);

    context.tools[0].constrained_sampling = Some(ConstrainedSampling::Config(
        ConstrainedSamplingConfig::JsonSchema {
            strict: JsonSchemaStrictMode::Require,
        },
    ));
    let error = try_encode_options(
        &model,
        &context,
        default_full_options(OpenAiReasoningPlan::disabled(), OrderedJsonObject::new()),
    )
    .expect_err("required unsupported schema must fail");
    assert!(error.to_string().contains("$defs schemas are unsupported"));

    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_strict_mode = Some(false);
    context.tools[0].parameters = object_schema();
    let error = try_encode_options(
        &model,
        &context,
        default_full_options(OpenAiReasoningPlan::disabled(), OrderedJsonObject::new()),
    )
    .expect_err("required strict mode must fail when the provider does not support it");
    assert!(
        error
            .to_string()
            .contains("requires JSON-schema constrained sampling")
    );
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `packages/ai/src/api/constrained-sampling.ts:resolveGrammarConstrainedSampling`
/// and `packages/ai/src/api/openai-completions.ts:convertTools`.
#[test]
fn openai_grammar_constrained_tool_uses_custom_wire_shape() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.supports_openai_grammar_tools = Some(true);
    let variants = [(GrammarFormat::OpenAiLark, "start: /[a-z]+/".into())]
        .into_iter()
        .collect();
    let mut context = one_user_context();
    context.tools.push(tool_spec(
        "query",
        serde_json::json!({
            "type": "object",
            "properties": { "expression": { "type": "string" } },
            "required": ["expression"]
        }),
        Some(ConstrainedSampling::Config(
            ConstrainedSamplingConfig::Grammar { variants },
        )),
    ));
    let mut replayed = assistant_with_tool_call();
    let ContentBlock::ToolCall { call, .. } = &mut replayed.content[0] else {
        unreachable!()
    };
    call.name = "query".into();
    call.arguments = serde_json::json!({"expression":"alpha"});
    context.messages.push(Message::Assistant(replayed));
    let wire: Value = serde_json::from_slice(&encode_direct(
        &model,
        &context,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    ))
    .expect("grammar wire");
    assert_eq!(wire["tools"][0]["type"], "custom");
    assert_eq!(wire["tools"][0]["custom"]["name"], "query");
    assert_eq!(
        wire["tools"][0]["custom"]["format"]["grammar"],
        serde_json::json!({"syntax":"lark","definition":"start: /[a-z]+/"})
    );
    let replayed_call = wire["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "assistant")
        .expect("replayed assistant");
    assert_eq!(replayed_call["tool_calls"][0]["type"], "custom");
    assert_eq!(replayed_call["tool_calls"][0]["custom"]["name"], "query");
    assert_eq!(replayed_call["tool_calls"][0]["custom"]["input"], "alpha");
}

/// Pinned Pi basis: custom-tool `input` fragments in
/// `packages/ai/src/api/openai-completions.ts` are wrapped into one
/// append-only JSON object using the configured grammar property.
#[test]
fn openai_grammar_custom_input_fragments_are_append_only_json() {
    let events =
        decode_openai_completions_sse(&custom_tool_sse(&["ab", "cd"]), grammar_decode_context());
    let deltas = tool_argument_deltas(&events);
    assert_eq!(deltas, vec![r#"{"expression":"ab"#, "cd", r#""}"#]);
    assert!(matches!(
        terminal_message(&events).content.as_slice(),
        [ContentBlock::ToolCall { call, .. }]
            if call.name == "query"
                && call.arguments == serde_json::json!({"expression":"abcd"})
    ));
}

/// Pinned Pi basis: `appendGrammarToolInputJsonDelta` delegates string
/// escaping to `JSON.stringify` while keeping every emitted delta append-only.
#[test]
fn openai_grammar_custom_input_escapes_json_string_deltas() {
    let input = "a\"\\\nline\tend";
    let events = decode_openai_completions_sse(
        &custom_tool_sse(&["a\"", "\\\n", "line\tend"]),
        grammar_decode_context(),
    );
    let deltas = tool_argument_deltas(&events);
    let joined = deltas.concat();
    assert_eq!(
        joined.as_bytes(),
        serde_json::to_vec(&serde_json::json!({"expression": input}))
            .expect("expected grammar JSON")
    );
    assert!(matches!(
        terminal_message(&events).content.as_slice(),
        [ContentBlock::ToolCall { call, .. }]
            if call.arguments == serde_json::json!({"expression": input})
    ));
}

/// Pinned Pi basis: terminal `finishBlock` walks canonical content order and
/// closes a custom grammar tool immediately before that tool's end event
/// (`packages/ai/src/api/openai-completions.ts:384-428,635-637`).
#[test]
fn openai_grammar_custom_terminal_events_follow_content_order() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-mixed-custom","model":"fixture","choices":[{"delta":{"content":"answer","tool_calls":[{"index":0,"id":"call-custom-1","type":"custom","custom":{"name":"query","input":"alpha"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#,
        grammar_decode_context(),
    );
    let text_block = events
        .iter()
        .find_map(|event| match event {
            AssistantEvent::ContentBlockStarted {
                block_id,
                kind: pi_ai::ContentBlockKind::Text,
                ..
            } => Some(block_id.clone()),
            _ => None,
        })
        .expect("text block");
    let tool_block = events
        .iter()
        .find_map(|event| match event {
            AssistantEvent::ContentBlockStarted {
                block_id,
                kind: pi_ai::ContentBlockKind::ToolCall,
                ..
            } => Some(block_id.clone()),
            _ => None,
        })
        .expect("tool block");
    let text_finished = events
        .iter()
        .position(|event| {
            matches!(event,
                AssistantEvent::ContentBlockFinished { block_id } if block_id == &text_block
            )
        })
        .expect("text finished");
    let tool_closing_delta = events
        .iter()
        .position(|event| {
            matches!(event,
                AssistantEvent::ToolArgumentsDelta { block_id, delta }
                    if block_id == &tool_block && delta == "\"}"
            )
        })
        .expect("custom tool closing delta");
    let tool_finished = events
        .iter()
        .position(|event| {
            matches!(event,
                AssistantEvent::ContentBlockFinished { block_id } if block_id == &tool_block
            )
        })
        .expect("tool finished");

    assert!(text_finished < tool_closing_delta);
    assert_eq!(tool_closing_delta + 1, tool_finished);
}

/// Architecture v2 part 2 §10.8; pinned Pi basis:
/// `getDeferredToolNames`, `getToolsByName`, and Kimi tool-result conversion
/// in `packages/ai/src/api/openai-completions.ts`.
#[test]
fn openai_kimi_deferred_tools_are_filtered_then_reintroduced() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.deferred_tools_mode = Some(DeferredToolsMode::Kimi);
    let mut context = one_user_context();
    context.tools = vec![
        tool_spec("base", object_schema(), None),
        tool_spec("deferred", object_schema(), None),
    ];
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("deferred-result"),
            tool_call_id: ToolCallId::new("defer-call"),
            tool_name: "load_tools".into(),
            content: vec![ToolResultContent::Text {
                id: ContentBlockId::new("deferred-text"),
                text: "loaded".into(),
            }],
            details: None,
            usage: None,
            added_tool_names: vec!["deferred".into()],
            is_error: false,
            timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
        }));
    let wire: Value = serde_json::from_slice(&encode_direct(
        &model,
        &context,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    ))
    .expect("deferred wire");
    assert_eq!(wire["tools"].as_array().expect("top-level tools").len(), 1);
    assert_eq!(wire["tools"][0]["function"]["name"], "base");
    let deferred_message = wire["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "system" && message.get("tools").is_some())
        .expect("deferred system tool message");
    assert_eq!(deferred_message["tools"][0]["function"]["name"], "deferred");
}

/// Architecture v2 part 2 §10.5; pinned Pi basis:
/// `packages/ai/src/api/openai-completions.ts:isOpenAIReasoningDetail`.
#[test]
fn openai_invalid_reasoning_details_rejected() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":4},{"type":"unknown","text":"bad"}]},"finish_reason":"stop"}]}

data: [DONE]

"#,
        decode_context(),
    );
    let message = terminal_message(&events);
    assert!(message.replay.items.is_empty());
}

/// Architecture v2 part 2 §10.5; pinned Pi basis:
/// `packages/ai/src/api/openai-completions.ts:parseChunkUsage`.
#[test]
fn openai_usage_cache_subtraction_and_reasoning_subset() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":20,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":5,"cache_write_tokens":3},"completion_tokens_details":{"reasoning_tokens":4}}}

data: [DONE]

"#,
        decode_context(),
    );
    assert_eq!(
        terminal_message(&events).usage,
        Usage {
            input_tokens: 12,
            output_tokens: 9,
            reasoning_tokens: Some(4),
            cache_read_tokens: Some(5),
            cache_write_tokens: Some(3),
            cache_write_one_hour_tokens: None,
            total_tokens: None,
            source: UsageSource::ProviderReported,
        }
    );
}

/// Architecture v2 part 2 §10.1; pinned Pi basis:
/// `choice.usage` fallback and nullish cached-token precedence in
/// `packages/ai/src/api/openai-completions.ts:parseChunkUsage`.
#[test]
fn openai_usage_null_chunk_falls_back_and_explicit_cached_zero_wins() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","usage":null,"choices":[{"delta":{"content":"ok"},"finish_reason":"stop","usage":{"prompt_tokens":20,"completion_tokens":3,"cached_tokens":7,"prompt_cache_hit_tokens":5,"prompt_tokens_details":{"cached_tokens":0}}}]}

data: [DONE]

"#,
        decode_context(),
    );
    assert_eq!(
        terminal_message(&events).usage,
        Usage {
            input_tokens: 20,
            output_tokens: 3,
            reasoning_tokens: Some(0),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            cache_write_one_hour_tokens: None,
            total_tokens: None,
            source: UsageSource::ProviderReported,
        }
    );
}

/// Architecture v2 part 2 §10.5; pinned Pi basis:
/// `packages/ai/src/api/openai-completions.ts:ensureToolCallBlock`.
#[test]
fn openai_tool_delta_correlation_by_index_then_id() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"tool_calls":[{"id":"call-original","function":{"name":"read","arguments":"{\""}}]},"finish_reason":null}]}

data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"tool_calls":[{"index":7,"id":"call-original","function":{"arguments":"path"}}]},"finish_reason":null}]}

data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"tool_calls":[{"index":7,"id":"call-changed","function":{"arguments":"\":\"Cargo"}}]},"finish_reason":null}]}

data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"tool_calls":[{"id":"call-changed","function":{"arguments":".toml\"}"}}]},"finish_reason":"tool_calls"}]}

data: [DONE]

"#,
        decode_context(),
    );
    assert_eq!(terminal_message(&events).content.len(), 1);
    assert!(matches!(
        &terminal_message(&events).content[0],
        ContentBlock::ToolCall { call, .. }
            if call.id.as_str() == "call-original"
                && call.name == "read"
                && call.arguments == serde_json::json!({"path":"Cargo.toml"})
    ));
}

/// Architecture v2 part 2 §10.1 `stream_response_id_is_preserved`; pinned Pi
/// basis: `output.responseId ||= chunk.id` in
/// `packages/ai/src/api/openai-completions.ts` ignores an empty first ID.
#[test]
fn stream_response_id_is_preserved() {
    let events = decode_openai_completions_sse(
        br#"data: {"id":"","model":"fixture","choices":[{"delta":{"content":"hel"},"finish_reason":null}]}

data: {"id":"chat-late-id","model":"fixture","choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}

data: [DONE]

"#,
        decode_context(),
    );
    let message = terminal_message(&events);
    assert_eq!(message.response_id.as_deref(), Some("chat-late-id"));
    assert!(matches!(
        message.content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "hello"
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                AssistantEvent::ResponseMetadata {
                    response_id: Some(_),
                    ..
                }
            ))
            .count(),
        1
    );
}

/// Architecture v2 part 2 §10.1 `stream_response_model_is_preserved`; pinned
/// Pi basis: `packages/ai/test/openai-completions-response-model.test.ts`.
#[test]
fn stream_response_model_is_preserved() {
    let routed = decode_openai_completions_sse(
        br#"data: {"id":"chat-routed","model":"routed/model","choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}

data: [DONE]

"#,
        decode_context(),
    );
    let routed = terminal_message(&routed);
    assert_eq!(routed.requested_model.as_str(), "fixture");
    assert_eq!(
        routed.response_model.as_ref().map(|model| model.as_str()),
        Some("routed/model")
    );

    for response_model in [Some("fixture"), Some(""), None] {
        let model_field =
            response_model.map_or(String::new(), |model| format!(r#","model":"{model}""#));
        let body = format!(
            "data: {{\"id\":\"chat-echo\"{model_field},\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let events = decode_openai_completions_sse(body.as_bytes(), decode_context());
        assert_eq!(terminal_message(&events).response_model, None);
    }
}

/// Architecture v2 part 2 §10.2; pinned Pi basis:
/// missing finish-reason fallback in `openai-completions.ts`.
#[test]
fn openai_chat_finish_reason_contract() {
    let strict = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"content":"partial"},"finish_reason":null}]}

data: [DONE]

"#,
        decode_context(),
    );
    assert!(matches!(strict.last(), Some(AssistantEvent::Failed { .. })));

    let mut permissive_context = decode_context();
    permissive_context.supports_finish_reason = false;
    let permissive = decode_openai_completions_sse(
        br#"data: {"id":"chat-1","model":"fixture","choices":[{"delta":{"content":"complete"},"finish_reason":null}]}

data: [DONE]

"#,
        permissive_context,
    );
    assert_eq!(
        terminal_message(&permissive).finish.reason,
        AssistantFinishReason::Stop
    );
}

/// Architecture v2 part 2 §2.1; pinned Pi basis:
/// `packages/ai/test/openai-completions-raw-stop-reason.test.ts`.
#[test]
fn openai_chat_raw_stop_reason_is_preserved() {
    let successful = decode_openai_completions_sse(
        br#"data: {"id":"chat-success","choices":[{"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#,
        decode_context(),
    );
    assert_eq!(
        terminal_message(&successful)
            .finish
            .raw_provider_reason
            .as_deref(),
        Some("stop")
    );

    let failed = decode_openai_completions_sse(
        br#"data: {"id":"chat-filtered","choices":[{"delta":{},"finish_reason":"content_filter"}]}

data: [DONE]

"#,
        decode_context(),
    );
    let message = terminal_message(&failed);
    assert_eq!(message.finish.reason, AssistantFinishReason::Error);
    assert_eq!(
        message.finish.raw_provider_reason.as_deref(),
        Some("content_filter")
    );
    assert_eq!(
        message
            .finish
            .error
            .as_ref()
            .map(|error| error.message.as_str()),
        Some("Provider finish_reason: content_filter")
    );
}

/// Architecture v2 part 2 §10.1 `stream_failure_is_terminal_message`; pinned
/// Pi basis: `packages/ai/src/api/openai-completions.ts` stream catch path.
#[test]
fn stream_failure_is_terminal_message() {
    let response = HttpResponse {
        status: 200,
        headers: HeaderMap::new(),
        diagnostics: Vec::new(),
        notify_observers: true,
        body: Box::pin(stream::iter(vec![
            Ok(partial_text_sse()),
            Err(TransportError::new("body", "body disconnected")),
        ])),
    };
    let api = openai_completions_api(Arc::new(OneResponseTransport::new(response)));
    let mut assistant = futures_executor::block_on(api.stream(
        resolved_request(base_fixture_model()),
        CancellationToken::new(),
    ))
    .expect("established stream");
    let events = futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = assistant.next().await {
            events.push(event);
        }
        events
    });
    assert!(matches!(events.last(), Some(AssistantEvent::Failed { .. })));
    assert!(matches!(
        terminal_message(&events).content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "partial"
    ));
}

/// Architecture v2 part 2 §10.1 `stream_cancellation_is_terminal_message`;
/// pinned Pi basis: the post-stream-establishment abort check in
/// `packages/ai/src/api/openai-completions.ts`.
#[test]
fn stream_cancellation_is_terminal_message() {
    let response = HttpResponse {
        status: 200,
        headers: HeaderMap::new(),
        diagnostics: Vec::new(),
        notify_observers: true,
        body: Box::pin(stream::iter(vec![
            Ok(partial_text_sse()),
            Ok(br#"data: {"id":"chat-1","model":"fixture-openai-model","choices":[{"delta":{"content":" ignored"},"finish_reason":"stop"}]}

data: [DONE]

"#.to_vec()),
        ])),
    };
    let api = openai_completions_api(Arc::new(OneResponseTransport::new(response)));
    let cancellation = CancellationToken::new();
    let mut assistant = futures_executor::block_on(
        api.stream(resolved_request(base_fixture_model()), cancellation.clone()),
    )
    .expect("established stream");
    let mut events = Vec::new();
    loop {
        let event = futures_executor::block_on(assistant.next()).expect("partial event");
        let saw_text =
            matches!(&event, AssistantEvent::TextDelta { delta, .. } if delta == "partial");
        events.push(event);
        if saw_text {
            break;
        }
    }
    cancellation.cancel();
    events.push(
        futures_executor::block_on(assistant.next()).expect("cancelled terminal after partial"),
    );
    assert!(matches!(
        events.last(),
        Some(AssistantEvent::Cancelled { .. })
    ));
    assert!(matches!(
        terminal_message(&events).content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "partial"
    ));
    assert!(futures_executor::block_on(assistant.next()).is_none());
}

/// Architecture v2 part 2 §9.2 and §10.1; pinned Pi basis: the same
/// OpenAI-completions body-error commitment contract on the local trait family.
#[test]
fn openai_local_stream_body_error_preserves_partial_content() {
    let response = LocalHttpResponse {
        status: 200,
        headers: HeaderMap::new(),
        diagnostics: Vec::new(),
        notify_observers: true,
        body: Box::pin(stream::iter(vec![
            Ok(partial_text_sse()),
            Err(TransportError::new("body", "local body disconnected")),
        ])),
    };
    let api = local_openai_completions_api(Rc::new(LocalOneResponseTransport::new(response)));
    let mut assistant = futures_executor::block_on(api.stream(
        local_resolved_request(base_fixture_model()),
        CancellationToken::new(),
    ))
    .expect("local established stream");
    let events = futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = assistant.next().await {
            events.push(event);
        }
        events
    });
    assert!(matches!(events.last(), Some(AssistantEvent::Failed { .. })));
    assert!(matches!(
        terminal_message(&events).content.as_slice(),
        [ContentBlock::Text { text, .. }] if text == "partial"
    ));
}

/// Architecture v2 part 2 §5.1; pinned published provider catalog basis.
#[test]
fn openai_provider_catalogs_match_pinned_counts_and_profiles() {
    let deepseek = deepseek_models().expect("DeepSeek catalog");
    let openrouter = openrouter_models().expect("OpenRouter catalog");
    assert_eq!(deepseek.len(), 2);
    assert_eq!(openrouter.len(), 346);
    assert!(deepseek.iter().all(|model| matches!(
        &model.api,
        ApiModelConfig::OpenAiCompletions(config)
            if config.compat.thinking_format == Some(OpenAiThinkingFormat::DeepSeek)
    )));
    assert!(openrouter.iter().all(|model| matches!(
        &model.api,
        ApiModelConfig::OpenAiCompletions(config)
            if config.compat.thinking_format == Some(OpenAiThinkingFormat::OpenRouter)
    )));
    let auto = openrouter
        .iter()
        .find(|model| model.common.model_ref.model.as_str() == "openrouter/auto")
        .expect("dynamic-price auto model");
    assert_eq!(
        auto.common.pricing.default.input,
        MoneyRate::new(-1_000_000_000_000)
    );
    assert_eq!(
        auto.common.pricing.default.output,
        MoneyRate::new(-1_000_000_000_000)
    );
    let cost = auto
        .common
        .pricing
        .calculate_cost(
            &Usage {
                input_tokens: 1,
                output_tokens: 1,
                reasoning_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                cache_write_one_hour_tokens: None,
                total_tokens: None,
                source: UsageSource::ProviderReported,
            },
            Currency::usd(),
            CacheWriteRetention::Default,
        )
        .expect("signed OpenRouter pricing arithmetic");
    assert_eq!(cost.micros, -2_000_000);
    assert!(auto.extensions.is_empty());
}

/// Architecture v2 part 1 §3.5 and part 2 §3.2: provider registrations share
/// one API execution capability rather than duplicating family logic.
#[test]
fn openai_completions_providers_share_api_implementation() {
    let transport: Arc<dyn HttpTransport> = Arc::new(NeverTransport);
    let api = openai_completions_api(Arc::clone(&transport));
    let deepseek = deepseek_provider_with_api(Arc::clone(&api)).expect("DeepSeek registration");
    let openrouter =
        openrouter_provider_with_api(Arc::clone(&api), transport).expect("OpenRouter registration");
    let deepseek_api = deepseek
        .apis
        .get(&"openai-completions".into())
        .expect("DeepSeek API");
    let openrouter_api = openrouter
        .apis
        .get(&"openai-completions".into())
        .expect("OpenRouter API");
    assert!(Arc::ptr_eq(deepseek_api, openrouter_api));
}

/// Pinned Pi basis: `packages/ai/src/providers/openrouter.ts` registers
/// `lazyOAuth(loadOpenRouterOAuth)` alongside environment API-key auth, and
/// `packages/ai/test/openrouter-oauth.test.ts` resolves the stored permanent
/// OAuth key through the text provider.
#[test]
fn openrouter_provider_registers_oauth_alongside_api_key() {
    let transport: Arc<dyn HttpTransport> = Arc::new(NeverTransport);
    let api = openai_completions_api(Arc::clone(&transport));
    let registration =
        openrouter_provider_with_api(api, transport).expect("OpenRouter registration");
    let store = Arc::new(InMemoryCredentialStore::new());
    futures_executor::block_on(store.modify(
        "openrouter".into(),
        CancellationToken::new(),
        |_| async {
            Ok(Some(Credential::OAuth(OAuthCredential {
                access: SecretString::new("sk-or-stored"),
                refresh: SecretString::new(""),
                expires_at: Timestamp::from_unix_millis(9_007_199_254_740_991),
                extra: ProviderOAuthExtra::None,
            })))
        },
    ))
    .expect("store permanent OAuth key");
    let mut request = ResolveAuthRequest::isolated(registration.descriptor.clone(), None);
    request.credential_store = store;
    let resolved =
        futures_executor::block_on(registration.auth.resolve(request, CancellationToken::new()))
            .expect("resolve OpenRouter auth")
            .expect("stored OAuth is supported");
    assert_eq!(
        resolved
            .headers
            .get(http::header::AUTHORIZATION)
            .expect("bearer authorization"),
        "Bearer sk-or-stored"
    );
}

/// Architecture v2 part 2 §6.4 and §10.7; pinned Pi basis:
/// `packages/ai/src/auth/oauth/openrouter.ts` races manual input only when it
/// is available and enforces `LOGIN_TIMEOUT_MS = 5 * 60 * 1000`.
#[test]
fn openrouter_send_oauth_respects_manual_paste_capability_and_deadline() {
    let interaction = Arc::new(NoManualSendInteraction::default());
    let oauth = OpenRouterOAuth::new(Arc::new(NeverTransport)).with_login_timeout(Duration::ZERO);
    let error = futures_executor::block_on(OAuthAuth::login(
        &oauth,
        interaction.clone(),
        CancellationToken::new(),
    ))
    .expect_err("pending loopback login must reach its deadline");
    assert_eq!(error.code(), "openrouter_oauth_timeout");
    assert_eq!(interaction.prompts.load(Ordering::SeqCst), 0);
}

/// Architecture v2 part 2 §6.4, §9.2, and §10.7; local counterpart to
/// `openrouter_send_oauth_respects_manual_paste_capability_and_deadline`.
#[test]
fn openrouter_local_oauth_respects_manual_paste_capability_and_deadline() {
    let interaction = Rc::new(NoManualLocalInteraction::default());
    let oauth =
        LocalOpenRouterOAuth::new(Rc::new(LocalNeverTransport)).with_login_timeout(Duration::ZERO);
    let error = futures_executor::block_on(LocalOAuthAuth::login(
        &oauth,
        interaction.clone(),
        CancellationToken::new(),
    ))
    .expect_err("pending local loopback login must reach its deadline");
    assert_eq!(error.code(), "openrouter_oauth_timeout");
    assert_eq!(interaction.prompts.get(), 0);
}

/// Architecture v2 part 2 §10.4 `headers_transform_can_delete_default`;
/// pinned Pi basis: `models.ts:641-661` makes `transformHeaders` final, while
/// `openai-completions.ts:723-737` applies session defaults before explicit
/// option headers. The Send and Local paths must preserve the same order.
#[test]
fn headers_transform_can_delete_default() {
    let model = session_affinity_model();
    let response = session_affinity_response();

    let send_transport = Arc::new(FixturePipelineTransport::new([response.clone()]));
    let send_registration = ProviderRegistration::builder(model.common.model_ref.provider.clone())
        .base_url(model.common.base_url.clone())
        .models(vec![model.clone()])
        .api(
            OpenAiCompletions::API_ID,
            openai_completions_api(Arc::clone(&send_transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("send affinity registration");
    let send_models = Models::builder()
        .provider(send_registration)
        .header_transform(Arc::new(AffinityHeaderEditor))
        .build()
        .expect("send affinity Models");
    drain_send_runtime(
        &send_models,
        &model,
        session_affinity_options(),
        "send affinity request",
    );
    let send_requests = send_transport.requests.lock().expect("send request lock");
    assert_final_affinity_headers(&send_requests[0].headers);

    let local_transport = Rc::new(LocalFixturePipelineTransport::new([response]));
    let local_registration =
        LocalProviderRegistration::builder(model.common.model_ref.provider.clone())
            .base_url(model.common.base_url.clone())
            .models(vec![model.clone()])
            .api(
                OpenAiCompletions::API_ID,
                local_openai_completions_api(
                    Rc::clone(&local_transport) as Rc<dyn LocalHttpTransport>
                ),
            )
            .build()
            .expect("local affinity registration");
    let local_models = LocalModels::builder()
        .provider(local_registration)
        .header_transform(Rc::new(LocalAffinityHeaderEditor))
        .build()
        .expect("local affinity Models");
    drain_local_runtime(
        &local_models,
        &model,
        session_affinity_options(),
        "local affinity request",
    );
    let local_requests = local_transport.requests.borrow();
    assert_final_affinity_headers(&local_requests[0].headers);
}

/// Architecture v2 part 2 §2.6 and §10.4; pinned Pi basis:
/// `openai-responses.ts:getClientApiKey` accepts a non-empty explicit
/// Authorization header without a separately resolved API key.
#[test]
fn openai_responses_header_only_auth_send() {
    let response = responses_empty_response();
    let transport = Arc::new(FixturePipelineTransport::new([response]));
    let mut registration =
        openai_provider(Arc::clone(&transport) as Arc<dyn HttpTransport>).expect("OpenAI provider");
    registration.auth = Arc::new(AbsentAuth);
    let model = registration.catalog.snapshot()[0].clone();
    let models = Models::builder()
        .provider(registration)
        .build()
        .expect("OpenAI Models");
    let mut headers = HeaderMapSpec::new();
    headers.insert(
        "Authorization".into(),
        Some("Bearer explicit-header-only".into()),
    );
    drain_send_runtime(
        &models,
        &model,
        SimpleGenerationOptions {
            headers,
            ..Default::default()
        },
        "header-only OpenAI Responses request",
    );
    assert_eq!(
        transport.requests.lock().unwrap()[0].headers["authorization"],
        "Bearer explicit-header-only"
    );
}

/// Local trait-family realization of `openai_responses_header_only_auth_send`;
/// pinned Pi also recognizes Cloudflare AI Gateway's auth header.
#[test]
fn openai_responses_header_only_auth_local() {
    let response = responses_empty_response();
    let transport = Rc::new(LocalFixturePipelineTransport::new([response]));
    let mut registration =
        local_openai_provider(Rc::clone(&transport) as Rc<dyn LocalHttpTransport>)
            .expect("local OpenAI provider");
    registration.auth = Rc::new(AbsentAuth);
    let model = registration.catalog.snapshot()[0].clone();
    let models = LocalModels::builder()
        .provider(registration)
        .build()
        .expect("local OpenAI Models");
    let mut headers = HeaderMapSpec::new();
    headers.insert(
        "CF-AIG-Authorization".into(),
        Some("Bearer cloudflare-header-only".into()),
    );
    drain_local_runtime(
        &models,
        &model,
        SimpleGenerationOptions {
            headers,
            ..Default::default()
        },
        "local header-only OpenAI Responses request",
    );
    assert_eq!(
        transport.requests.borrow()[0].headers["cf-aig-authorization"],
        "Bearer cloudflare-header-only"
    );
}

/// Architecture v2 part 2 §2.6; pinned Pi basis:
/// `openai-responses.ts:getClientApiKey` inspects only caller option headers
/// before client construction. Model headers and final header transforms
/// cannot make an otherwise unconfigured request eligible for header-only
/// authentication.
#[test]
fn openai_responses_header_only_auth_requires_explicit_options_send_and_local() {
    let mut model = pi_ai_openai::openai_models().unwrap().remove(0);
    model.common.headers.insert(
        "Authorization".into(),
        Some("Bearer model-header-only".into()),
    );

    let send_transport = Arc::new(FixturePipelineTransport::new([responses_empty_response()]));
    let send_registration = ProviderRegistration::builder("openai")
        .base_url(model.common.base_url.clone())
        .auth(Arc::new(AbsentAuth))
        .models(vec![model.clone()])
        .api(
            OpenAiResponses::API_ID,
            openai_responses_api(Arc::clone(&send_transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .unwrap();
    let send_models = Models::builder()
        .provider(send_registration)
        .build()
        .unwrap();
    let send_result = futures_executor::block_on(send_models.stream_simple(
        ModelRequest {
            model: model.common.model_ref.clone(),
            context: Context::new(None),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ));
    assert!(send_result.is_err());
    assert!(send_transport.requests.lock().unwrap().is_empty());

    let local_model = pi_ai_openai::openai_models().unwrap().remove(0);
    let local_transport = Rc::new(LocalFixturePipelineTransport::new([
        responses_empty_response(),
    ]));
    let local_registration = LocalProviderRegistration::builder("openai")
        .base_url(local_model.common.base_url.clone())
        .auth(Rc::new(AbsentAuth))
        .models(vec![local_model.clone()])
        .api(
            OpenAiResponses::API_ID,
            local_openai_responses_api(Rc::clone(&local_transport) as Rc<dyn LocalHttpTransport>),
        )
        .build()
        .unwrap();
    let local_models = LocalModels::builder()
        .provider(local_registration)
        .header_transform(Rc::new(LocalHeaderOnlyAuthGrant))
        .build()
        .unwrap();
    let local_result = futures_executor::block_on(local_models.stream_simple(
        ModelRequest {
            model: local_model.common.model_ref,
            context: Context::new(None),
            options: SimpleGenerationOptions::default(),
        },
        CancellationToken::new(),
    ));
    assert!(local_result.is_err());
    assert!(local_transport.requests.borrow().is_empty());
}

/// Architecture v2 part 2 §3.2/§9.2/§10.4; pinned Pi basis:
/// `openai-responses.ts:createClient` applies session affinity after model
/// headers and before explicit request headers in both trait families.
#[test]
fn openai_responses_full_affinity_follows_model_headers_send_and_local() {
    let mut model = pi_ai_openai::openai_models().unwrap().remove(0);
    model
        .common
        .headers
        .insert("session_id".into(), Some("model-session".into()));
    model
        .common
        .headers
        .insert("x-client-request-id".into(), Some("model-request".into()));
    let options = OpenAiResponsesOptions {
        max_output_tokens: None,
        temperature: None,
        sampling: OrderedJsonObject::new(),
        reasoning_effort: None,
        reasoning_summary: None,
        service_tier: None,
        tool_choice: None,
        cache_retention: CacheRetention::Short,
        session_id: Some("full-session".into()),
    };
    let mut request_headers = HeaderMapSpec::new();
    request_headers.insert(
        "authorization".into(),
        Some("Bearer explicit-header".into()),
    );
    let request_options = ApiRequestOptions {
        headers: request_headers,
        ..Default::default()
    };

    let send_transport = Arc::new(FixturePipelineTransport::new([responses_empty_response()]));
    let send_registration = ProviderRegistration::builder("openai")
        .base_url(model.common.base_url.clone())
        .auth(Arc::new(AbsentAuth))
        .models(vec![model.clone()])
        .api(
            OpenAiResponses::API_ID,
            openai_responses_api(Arc::clone(&send_transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .unwrap();
    let send_models = Models::builder()
        .provider(send_registration)
        .build()
        .unwrap();
    let send_stream = futures_executor::block_on(
        send_models.stream_api_with_request_options::<OpenAiResponses>(
            model.common.model_ref.clone(),
            Context::new(None),
            options.clone(),
            request_options.clone(),
            CancellationToken::new(),
        ),
    )
    .unwrap();
    futures_executor::block_on(async { send_stream.collect::<Vec<_>>().await });
    let send_headers = &send_transport.requests.lock().unwrap()[0].headers;
    assert_eq!(send_headers["session_id"], "full-session");
    assert_eq!(send_headers["x-client-request-id"], "full-session");

    let local_transport = Rc::new(LocalFixturePipelineTransport::new([
        responses_empty_response(),
    ]));
    let local_registration = LocalProviderRegistration::builder("openai")
        .base_url(model.common.base_url.clone())
        .auth(Rc::new(AbsentAuth))
        .models(vec![model.clone()])
        .api(
            OpenAiResponses::API_ID,
            local_openai_responses_api(Rc::clone(&local_transport) as Rc<dyn LocalHttpTransport>),
        )
        .build()
        .unwrap();
    let local_models = LocalModels::builder()
        .provider(local_registration)
        .build()
        .unwrap();
    let local_stream = futures_executor::block_on(
        local_models.stream_api_with_request_options::<OpenAiResponses>(
            model.common.model_ref,
            Context::new(None),
            options,
            request_options,
            CancellationToken::new(),
        ),
    )
    .unwrap();
    futures_executor::block_on(async { local_stream.collect::<Vec<_>>().await });
    let local_headers = &local_transport.requests.borrow()[0].headers;
    assert_eq!(local_headers["session_id"], "full-session");
    assert_eq!(local_headers["x-client-request-id"], "full-session");
}

fn responses_empty_response() -> Vec<u8> {
    br#"data: {"type":"response.done","response":{"id":"resp_header","status":"completed","output":[]}}

"#
    .to_vec()
}

struct AbsentAuth;

impl AuthResolver for AbsentAuth {
    fn resolve(
        &self,
        _request: ResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<pi_ai::ResolvedAuth>, AuthError>> {
        Box::pin(async { Ok(None) })
    }
}

impl LocalAuthResolver for AbsentAuth {
    fn resolve(
        &self,
        _request: LocalResolveAuthRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<pi_ai::ResolvedAuth>, AuthError>> {
        Box::pin(async { Ok(None) })
    }
}

struct AffinityHeaderEditor;

impl HeaderTransform for AffinityHeaderEditor {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> SendBoxFuture<'a, Result<(), MiddlewareError>> {
        edit_affinity_headers(headers);
        Box::pin(async { Ok(()) })
    }
}

struct LocalAffinityHeaderEditor;

impl LocalHeaderTransform for LocalAffinityHeaderEditor {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        edit_affinity_headers(headers);
        Box::pin(async { Ok(()) })
    }
}

struct LocalHeaderOnlyAuthGrant;

impl LocalHeaderTransform for LocalHeaderOnlyAuthGrant {
    fn transform<'a>(
        &'a self,
        _context: HeaderTransformContext<'a>,
        headers: &'a mut HeaderMap,
    ) -> LocalBoxFuture<'a, Result<(), MiddlewareError>> {
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer transform-header-only"),
        );
        Box::pin(async { Ok(()) })
    }
}

fn edit_affinity_headers(headers: &mut HeaderMap) {
    assert_eq!(
        headers.get("session_id"),
        Some(&HeaderValue::from_static("session-1"))
    );
    assert_eq!(
        headers.get("x-client-request-id"),
        Some(&HeaderValue::from_static("session-1"))
    );
    assert_eq!(
        headers.get("x-session-affinity"),
        Some(&HeaderValue::from_static("explicit-affinity")),
        "explicit request headers must override session defaults before the transform"
    );
    headers.remove("session_id");
    headers.remove("x-session-affinity");
    headers.insert(
        "x-client-request-id",
        HeaderValue::from_static("transformed-request-id"),
    );
}

fn assert_final_affinity_headers(headers: &HeaderMap) {
    assert!(headers.get("session_id").is_none());
    assert!(headers.get("x-session-affinity").is_none());
    assert_eq!(
        headers.get("x-client-request-id"),
        Some(&HeaderValue::from_static("transformed-request-id"))
    );
}

fn session_affinity_model() -> ModelDescriptor {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.send_session_affinity_headers = Some(true);
    config.compat.session_affinity_format = Some(SessionAffinityFormat::OpenAi);
    model
}

fn session_affinity_options() -> SimpleGenerationOptions {
    let mut headers = HeaderMapSpec::new();
    headers.insert(
        "X-Session-Affinity".into(),
        Some("explicit-affinity".into()),
    );
    SimpleGenerationOptions {
        session_id: Some("session-1".into()),
        cache_retention: Some(CacheRetention::Short),
        headers,
        ..Default::default()
    }
}

fn session_affinity_response() -> Vec<u8> {
    br#"data: {"id":"chat-affinity","model":"fixture-openai-model","choices":[{"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#
    .to_vec()
}

fn drain_send_runtime(
    models: &Models,
    model: &ModelDescriptor,
    options: SimpleGenerationOptions,
    message: &str,
) {
    let mut assistant = futures_executor::block_on(ModelRuntime::stream(
        models,
        ModelRequest {
            model: model.common.model_ref.clone(),
            context: one_user_context(),
            options,
        },
        CancellationToken::new(),
    ))
    .expect(message);
    futures_executor::block_on(async move { while assistant.next().await.is_some() {} });
}

fn drain_local_runtime(
    models: &LocalModels,
    model: &ModelDescriptor,
    options: SimpleGenerationOptions,
    message: &str,
) {
    let mut assistant = futures_executor::block_on(LocalModelRuntime::stream(
        models,
        ModelRequest {
            model: model.common.model_ref.clone(),
            context: one_user_context(),
            options,
        },
        CancellationToken::new(),
    ))
    .expect(message);
    futures_executor::block_on(async move { while assistant.next().await.is_some() {} });
}

struct NeverTransport;

impl HttpTransport for NeverTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::new("unused", "transport must not run")) })
    }
}

struct LocalNeverTransport;

impl LocalHttpTransport for LocalNeverTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async { Err(TransportError::new("unused", "transport must not run")) })
    }
}

#[derive(Default)]
struct NoManualSendInteraction {
    prompts: AtomicUsize,
}

impl AuthInteraction for NoManualSendInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities {
            loopback_http: true,
            manual_paste: false,
            ..AuthHostCapabilities::default()
        }
    }

    fn prompt(
        &self,
        _prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(AuthInteractionError::Cancelled) })
    }

    fn notify(&self, _event: AuthEvent) -> Result<(), AuthInteractionError> {
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Box<dyn RedirectReceiver>, AuthInteractionError>> {
        Box::pin(async { Ok(Box::new(PendingSendReceiver::new()) as Box<dyn RedirectReceiver>) })
    }
}

struct PendingSendReceiver {
    redirect_uri: Url,
}

impl PendingSendReceiver {
    fn new() -> Self {
        Self {
            redirect_uri: Url::parse("http://127.0.0.1:43123/oauth/callback/test")
                .expect("send redirect URI"),
        }
    }
}

impl RedirectReceiver for PendingSendReceiver {
    fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    fn receive(
        self: Box<Self>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(AuthInteractionError::Cancelled)
        })
    }
}

#[derive(Default)]
struct NoManualLocalInteraction {
    prompts: Cell<usize>,
}

impl LocalAuthInteraction for NoManualLocalInteraction {
    fn capabilities(&self) -> AuthHostCapabilities {
        AuthHostCapabilities {
            loopback_http: true,
            manual_paste: false,
            ..AuthHostCapabilities::default()
        }
    }

    fn prompt(
        &self,
        _prompt: AuthPrompt,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<AuthAnswer, AuthInteractionError>> {
        self.prompts.set(self.prompts.get() + 1);
        Box::pin(async { Err(AuthInteractionError::Cancelled) })
    }

    fn notify(&self, _event: AuthEvent) -> Result<(), AuthInteractionError> {
        Ok(())
    }

    fn create_redirect_receiver(
        &self,
        _request: RedirectReceiverRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Box<dyn LocalRedirectReceiver>, AuthInteractionError>> {
        Box::pin(async {
            Ok(Box::new(PendingLocalReceiver::new()) as Box<dyn LocalRedirectReceiver>)
        })
    }
}

struct PendingLocalReceiver {
    redirect_uri: Url,
}

impl PendingLocalReceiver {
    fn new() -> Self {
        Self {
            redirect_uri: Url::parse("http://127.0.0.1:43124/oauth/callback/test")
                .expect("local redirect URI"),
        }
    }
}

impl LocalRedirectReceiver for PendingLocalReceiver {
    fn redirect_uri(&self) -> &Url {
        &self.redirect_uri
    }

    fn receive(
        self: Box<Self>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'static, Result<RedirectArrival, AuthInteractionError>> {
        Box::pin(async move {
            cancellation.cancelled().await;
            Err(AuthInteractionError::Cancelled)
        })
    }
}

struct OneResponseTransport {
    response: Mutex<Option<HttpResponse>>,
}

impl OneResponseTransport {
    fn new(response: HttpResponse) -> Self {
        Self {
            response: Mutex::new(Some(response)),
        }
    }
}

impl HttpTransport for OneResponseTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let response = self
            .response
            .lock()
            .expect("response lock")
            .take()
            .ok_or_else(|| TransportError::new("exhausted", "response already consumed"));
        Box::pin(async move { response })
    }
}

struct LocalOneResponseTransport {
    response: RefCell<Option<LocalHttpResponse>>,
}

impl LocalOneResponseTransport {
    fn new(response: LocalHttpResponse) -> Self {
        Self {
            response: RefCell::new(Some(response)),
        }
    }
}

impl LocalHttpTransport for LocalOneResponseTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        let response = self
            .response
            .borrow_mut()
            .take()
            .ok_or_else(|| TransportError::new("exhausted", "local response already consumed"));
        Box::pin(async move { response })
    }
}

fn resolved_request(model: ModelDescriptor) -> ResolvedApiRequest {
    let endpoint = model.common.base_url.clone();
    ResolvedApiRequest {
        model,
        context: one_user_context(),
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: pi_ai::ApiRequestOptions::default(),
        endpoint,
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        api_key: None,
        api: OpenAiCompletions::API_ID.into(),
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(DefaultRetryClassifier::default()),
    }
}

fn local_resolved_request(model: ModelDescriptor) -> LocalResolvedApiRequest {
    let endpoint = model.common.base_url.clone();
    LocalResolvedApiRequest {
        model,
        context: one_user_context(),
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: pi_ai::ApiRequestOptions::default(),
        endpoint,
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        api_key: None,
        api: OpenAiCompletions::API_ID.into(),
        payload_transforms: Rc::from([]),
        response_observers: Rc::from([]),
        attempt_middleware: Rc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Rc::new(LocalDefaultRetryClassifier::default()),
    }
}

fn partial_text_sse() -> Vec<u8> {
    br#"data: {"id":"chat-1","model":"fixture-openai-model","choices":[{"delta":{"content":"partial"},"finish_reason":null}]}

"#
    .to_vec()
}

fn custom_tool_sse(fragments: &[&str]) -> Vec<u8> {
    let mut body = String::new();
    for fragment in fragments {
        let chunk = serde_json::json!({
            "id": "chat-custom-1",
            "model": "fixture",
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-custom-1",
                        "type": "custom",
                        "custom": {"name": "query", "input": fragment}
                    }]
                },
                "finish_reason": null
            }]
        });
        body.push_str("data: ");
        body.push_str(&serde_json::to_string(&chunk).expect("custom chunk JSON"));
        body.push_str("\n\n");
    }
    body.push_str(
        "data: {\"id\":\"chat-custom-1\",\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
    );
    body.into_bytes()
}

fn tool_argument_deltas(events: &[AssistantEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            AssistantEvent::ToolArgumentsDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

struct FixturePipelineTransport {
    responses: Mutex<VecDeque<Vec<u8>>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FixturePipelineTransport {
    fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl HttpTransport for FixturePipelineTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.requests
            .lock()
            .expect("fixture request lock")
            .push(request);
        let response = self
            .responses
            .lock()
            .expect("fixture response lock")
            .pop_front()
            .expect("fixture pipeline response");
        Box::pin(async move { Ok(HttpResponse::from_bytes(200, HeaderMap::new(), response)) })
    }
}

struct LocalFixturePipelineTransport {
    responses: RefCell<VecDeque<Vec<u8>>>,
    requests: RefCell<Vec<HttpRequest>>,
}

impl LocalFixturePipelineTransport {
    fn new(responses: impl IntoIterator<Item = Vec<u8>>) -> Self {
        Self {
            responses: RefCell::new(responses.into_iter().collect()),
            requests: RefCell::new(Vec::new()),
        }
    }
}

impl LocalHttpTransport for LocalFixturePipelineTransport {
    fn execute(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        self.requests.borrow_mut().push(request);
        let response = self
            .responses
            .borrow_mut()
            .pop_front()
            .expect("local fixture pipeline response");
        Box::pin(async move {
            Ok(LocalHttpResponse::from_bytes(
                200,
                HeaderMap::new(),
                response,
            ))
        })
    }
}

fn assert_fixture_case(case_dir: &Path) {
    let canonical: Value = serde_json::from_slice(
        &fs::read(case_dir.join("canonical.json")).expect("canonical fixture"),
    )
    .expect("canonical JSON");
    let model = fixture_model(&canonical["model"]);
    let mut context = fixture_context(&canonical["context"], &model);
    let expected_turn_one = fs::read(case_dir.join("request-turn-1.body.json")).expect("turn one");
    let actual_turn_one = encode_fixture(&model, &context, &canonical);
    assert_eq!(
        actual_turn_one,
        expected_turn_one,
        "turn-one body mismatch for {}",
        case_name(case_dir)
    );

    let response = fs::read(case_dir.join("response-turn-1.sse")).expect("response SSE");
    let compat = fixture_compat(&canonical["model"]["compat"]);
    let grammar_tool_input_properties =
        openai_grammar_tool_input_properties(&context, &compat).expect("fixture grammar tools");
    let events = decode_openai_completions_sse(
        &response,
        OpenAiCompletionsDecodeContext {
            message_id: MessageId::new("fixture-turn-one-assistant"),
            provider: model.common.model_ref.provider.clone(),
            requested_model: model.common.model_ref.model.clone(),
            timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
            supports_finish_reason: compat.supports_finish_reason.unwrap_or(true),
            grammar_tool_input_properties,
        },
    );
    let assistant = terminal_message(&events).clone();
    let persisted = serde_json::to_vec(&assistant).expect("persist assistant");
    let assistant: AssistantMessage =
        serde_json::from_slice(&persisted).expect("restore assistant");
    append_fixture_turn_two(&mut context, assistant, &canonical, &model);

    let expected_turn_two = fs::read(case_dir.join("request-turn-2.body.json")).expect("turn two");
    let actual_turn_two = encode_fixture(&model, &context, &canonical);
    assert_eq!(
        actual_turn_two,
        expected_turn_two,
        "turn-two body mismatch for {}",
        case_name(case_dir)
    );

    // `stream` fixtures exercise the family-specific full-options entrypoint
    // above. `streamSimple` fixtures additionally traverse Models and the
    // unmodified default HTTP handler so the wire gate observes the actual
    // lowered payload rather than replacing it with a pre-encoded golden.
    if canonical["entrypoint"] != "streamSimple"
        || !fixture_tool_choice_is_simple(&canonical["options"]["toolChoice"])
    {
        return;
    }

    let transport = Arc::new(FixturePipelineTransport::new([response.clone(), response]));
    let mut provider_headers = HeaderMapSpec::new();
    provider_headers.insert("authorization".into(), Some(FIXTURE_AUTHORIZATION.into()));
    let registration = ProviderRegistration::builder(model.common.model_ref.provider.clone())
        .display_name("OpenAI fixture provider")
        .base_url(model.common.base_url.clone())
        .headers(provider_headers)
        .models(vec![model.clone()])
        .api(
            OpenAiCompletions::API_ID,
            openai_completions_api(Arc::clone(&transport) as Arc<dyn HttpTransport>),
        )
        .build()
        .expect("fixture provider registration");
    let models = Models::builder()
        .provider(registration)
        .build()
        .expect("fixture Models pipeline");

    let mut pipeline_context = fixture_context(&canonical["context"], &model);
    let turn_one_events = run_fixture_pipeline(
        &models,
        &model,
        pipeline_context.clone(),
        fixture_simple_options(&canonical["options"]),
    );
    let first_assistant = terminal_message(&turn_one_events).clone();
    let persisted = serde_json::to_vec(&first_assistant).expect("persist pipeline assistant");
    let first_assistant = serde_json::from_slice(&persisted).expect("restore pipeline assistant");
    append_fixture_turn_two(&mut pipeline_context, first_assistant, &canonical, &model);
    let _turn_two_events = run_fixture_pipeline(
        &models,
        &model,
        pipeline_context,
        fixture_simple_options(&canonical["options"]),
    );

    let requests = transport.requests.lock().expect("fixture request lock");
    assert_eq!(requests.len(), 2, "fixture pipeline must send both turns");
    assert_request_capture(case_dir, 1, &requests[0], &expected_turn_one);
    assert_request_capture(case_dir, 2, &requests[1], &expected_turn_two);
}

fn run_fixture_pipeline(
    models: &Models,
    model: &ModelDescriptor,
    context: Context,
    options: SimpleGenerationOptions,
) -> Vec<AssistantEvent> {
    let mut stream = futures_executor::block_on(ModelRuntime::stream(
        models,
        ModelRequest {
            model: model.common.model_ref.clone(),
            context,
            options,
        },
        CancellationToken::new(),
    ))
    .expect("fixture request establishes a stream");
    futures_executor::block_on(async move {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
}

fn append_fixture_turn_two(
    context: &mut Context,
    assistant: AssistantMessage,
    canonical: &Value,
    model: &ModelDescriptor,
) {
    context.messages.push(Message::Assistant(assistant));
    let first_index = context.messages.len();
    for (offset, message) in canonical["turnTwoAppend"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        context
            .messages
            .push(fixture_message(message, first_index + offset, model));
    }
}

fn assert_request_capture(case_dir: &Path, turn: u8, request: &HttpRequest, expected_body: &[u8]) {
    let capture: Value = serde_json::from_slice(
        &fs::read(case_dir.join(format!("request-turn-{turn}.headers.json")))
            .expect("request metadata capture"),
    )
    .expect("request metadata JSON");
    assert_eq!(capture["schemaVersion"], 1);
    assert_eq!(
        request.method.as_str(),
        capture["method"].as_str().expect("method")
    );
    assert_eq!(request.url.path(), capture["path"].as_str().expect("path"));
    assert_eq!(
        request.url.query(),
        capture.get("query").and_then(Value::as_str),
        "query mismatch for {} turn {turn}",
        case_name(case_dir)
    );
    assert_eq!(
        request.body,
        expected_body,
        "pipeline body mismatch for {} turn {turn}",
        case_name(case_dir)
    );

    for name in capture["omittedRuntimeHeaders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        assert!(
            request.headers.get(name).is_none(),
            "runtime header {name} was not omitted for {} turn {turn}",
            case_name(case_dir)
        );
    }

    let mut actual_headers = BTreeMap::new();
    for (name, value) in &request.headers {
        let value = value.to_str().expect("fixture header UTF-8");
        actual_headers.insert(
            name.as_str().to_owned(),
            if is_sensitive_header(name.as_str()) {
                "[REDACTED]".to_owned()
            } else {
                value.to_owned()
            },
        );
    }
    let expected_headers = capture["headers"]
        .as_object()
        .expect("captured headers")
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                value.as_str().expect("captured header string").to_owned(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        actual_headers,
        expected_headers,
        "logical header mismatch for {} turn {turn}",
        case_name(case_dir)
    );
    assert_eq!(
        request
            .headers
            .get(http::header::AUTHORIZATION)
            .expect("raw authorization header"),
        FIXTURE_AUTHORIZATION
    );
    let debug = format!("{request:?}");
    assert!(!debug.contains(FIXTURE_AUTHORIZATION));
    assert!(debug.to_ascii_lowercase().contains("redacted"));

    let metadata: Value = serde_json::from_slice(
        &fs::read(case_dir.join("metadata.json")).expect("capture metadata"),
    )
    .expect("capture metadata JSON");
    assert_eq!(metadata["secretsRedacted"], true);
    assert_eq!(
        expected_headers.get("authorization").map(String::as_str),
        Some("[REDACTED]")
    );
}

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie" | "set-cookie"
    )
}

fn assert_native_tool_choice_wire_bodies() {
    let model = base_fixture_model();
    let context = one_user_context();
    let named_function = OrderedJsonObject::from_iter([
        ("type", OrderedJsonValue::from("function")),
        (
            "function",
            OrderedJsonObject::from_iter([("name", OrderedJsonValue::from("read_file"))]).into(),
        ),
    ]);
    let cases = [
        (
            OpenAiCompletionsToolChoice::Auto,
            br#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"tool_choice":"auto","reasoning_effort":"none"}"#.as_slice(),
        ),
        (
            OpenAiCompletionsToolChoice::None,
            br#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"tool_choice":"none","reasoning_effort":"none"}"#.as_slice(),
        ),
        (
            OpenAiCompletionsToolChoice::Required,
            br#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"tool_choice":"required","reasoning_effort":"none"}"#.as_slice(),
        ),
        (
            OpenAiCompletionsToolChoice::Function {
                name: "read_file".into(),
            },
            br#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"tool_choice":{"type":"function","function":{"name":"read_file"}},"reasoning_effort":"none"}"#.as_slice(),
        ),
        (
            OpenAiCompletionsToolChoice::Custom {
                name: "grammar".into(),
            },
            br#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"tool_choice":{"type":"custom","custom":{"name":"grammar"}},"reasoning_effort":"none"}"#.as_slice(),
        ),
        (
            OpenAiCompletionsToolChoice::AllowedTools {
                mode: OpenAiAllowedToolsMode::Required,
                tools: OrderedJsonArray::from_iter([named_function]),
            },
            br#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"tool_choice":{"type":"allowed_tools","allowed_tools":{"mode":"required","tools":[{"type":"function","function":{"name":"read_file"}}]}},"reasoning_effort":"none"}"#.as_slice(),
        ),
    ];

    for (choice, expected) in cases {
        let options = OpenAiCompletionsOptions {
            tool_choice: Some(choice),
            ..default_full_options(OpenAiReasoningPlan::disabled(), OrderedJsonObject::new())
        };
        assert_eq!(encode_options(&model, &context, options), expected);
    }
}

fn assert_baseten_wire_case() {
    let mut model = base_fixture_model();
    model.common.model_ref = ModelRef::new("baseten", "zai-org/GLM-5.2");
    model.common.display_name = "GLM 5.2 (Baseten)".into();
    model.common.base_url = Url::parse("https://inference.baseten.co/v1").expect("Baseten URL");
    model.common.limits = ModelLimits {
        context_window: 1_048_576,
        max_output_tokens: 262_144,
    };
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.thinking_format = Some(OpenAiThinkingFormat::Baseten);
    config.compat.supports_reasoning_effort = Some(true);
    config.compat.max_tokens_field = Some(MaxTokensField::MaxTokens);
    config.compat.chat_template_args = Some(ChatTemplateValues::from_iter([(
        "enable_thinking".into(),
        ChatTemplateKwargValue::Variable(ChatTemplateVariable {
            variable: ChatTemplateVariableName::ThinkingEnabled,
            omit_when_off: None,
        }),
    )]));
    let options = lower_simple_options(
        &model,
        &one_user_context(),
        &SimpleGenerationOptions {
            reasoning: Some(pi_ai::ReasoningLevel::High),
            ..Default::default()
        },
    );
    let actual = encode_options(&model, &one_user_context(), options);
    let expected = br#"{"model":"zai-org/GLM-5.2","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"max_tokens":262144,"chat_template_args":{"enable_thinking":true},"reasoning_effort":"high"}"#;
    assert_eq!(
        actual, expected,
        "Baseten must retain both chat_template_args and reasoning_effort"
    );
}

/// Pinned Pi basis: `openai-completions.ts:795-805` and
/// `openai-completions-empty-tools.test.ts`. History-only requests retain the
/// proxy-compatibility `tools: []` marker, but `tool_stream` belongs only to a
/// non-empty active tool list.
fn assert_history_only_tools_omit_tool_stream() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.zai_tool_stream = Some(true);

    let mut history_only = Context::new(None);
    history_only
        .messages
        .push(Message::Assistant(assistant_with_tool_call()));
    let body = encode_direct(
        &model,
        &history_only,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    );
    let wire = std::str::from_utf8(&body).expect("history-only wire UTF-8");
    assert!(wire.contains(r#""tools":[]"#));
    assert!(!wire.contains(r#""tool_stream""#));

    let mut active = history_only;
    active
        .tools
        .push(tool_spec("read_file", object_schema(), None));
    let body = encode_direct(
        &model,
        &active,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    );
    let wire = std::str::from_utf8(&body).expect("active-tools wire UTF-8");
    assert!(wire.contains(r#""tool_stream":true"#));
}

/// Pinned Pi basis: `buildParams` tests `model.baseUrl` for direct-OpenAI
/// prompt-cache eligibility, while architecture §3.6 uses the effective URL
/// only for compatibility resolution.
fn assert_prompt_cache_uses_catalog_model_base_url() {
    let mut model = base_fixture_model();
    model.common.base_url = Url::parse("https://api.openai.com/v1").expect("OpenAI URL");
    let ApiModelConfig::OpenAiCompletions(config) = &model.api else {
        unreachable!()
    };
    let typed = TypedModelDescriptor::<OpenAiCompletions> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    };
    let effective_gateway =
        Url::parse("https://credential-gateway.example/v1").expect("gateway URL");
    let compat = OpenAiCompletions::resolve_compat(&effective_gateway, &config.compat)
        .expect("gateway compatibility");
    let options = OpenAiCompletionsOptions {
        session_id: Some("gateway-session".into()),
        ..default_full_options(OpenAiReasoningPlan::disabled(), OrderedJsonObject::new())
    };
    let wire = OpenAiCompletions::encode(
        EncodeContext {
            model: &typed,
            context: &one_user_context(),
            compat: &compat,
            effective_base_url: &effective_gateway,
        },
        &options,
    )
    .expect("encode credential-gateway request");
    let actual = OrderedJsonWriter::to_vec(&wire.into()).expect("ordered request JSON");
    let expected = br#"{"model":"fixture-openai-model","messages":[{"role":"user","content":"hello"}],"stream":true,"prompt_cache_key":"gateway-session","stream_options":{"include_usage":true},"reasoning_effort":"none"}"#;
    assert_eq!(actual, expected);
}

/// Pinned Pi basis: `openai-completions.ts:783-789` uses a truthiness check,
/// so an API-specific `maxTokens: 0` is omitted rather than serialized.
fn assert_zero_max_tokens_is_omitted() {
    let model = base_fixture_model();
    let options = OpenAiCompletionsOptions {
        max_tokens: Some(0),
        max_tokens_field: MaxTokensField::MaxCompletionTokens,
        ..default_full_options(OpenAiReasoningPlan::disabled(), OrderedJsonObject::new())
    };
    let body: Value = serde_json::from_slice(&encode_options(&model, &one_user_context(), options))
        .expect("zero max-token wire");
    assert!(body.get("max_tokens").is_none());
    assert!(body.get("max_completion_tokens").is_none());
}

/// Pinned Pi basis: `openai-completions.ts:1325-1326,1417` continues past an
/// omitted empty assistant before updating `lastRole`.
fn assert_empty_assistant_omission_preserves_tool_result_bridge() {
    let mut model = base_fixture_model();
    let ApiModelConfig::OpenAiCompletions(config) = &mut model.api else {
        unreachable!()
    };
    config.compat.requires_assistant_after_tool_result = Some(true);

    let mut empty_assistant = assistant_with_tool_call();
    empty_assistant.content.clear();
    empty_assistant.finish.reason = AssistantFinishReason::Stop;
    empty_assistant.finish.raw_provider_reason = Some("stop".into());

    let mut context = Context::new(None);
    context
        .messages
        .push(Message::ToolResult(ToolResultMessage {
            id: MessageId::new("bridge-result"),
            tool_call_id: ToolCallId::new("call-bridge"),
            tool_name: "read_file".into(),
            content: vec![ToolResultContent::Text {
                id: ContentBlockId::new("bridge-result-text"),
                text: "done".into(),
            }],
            details: None,
            usage: None,
            added_tool_names: Vec::new(),
            is_error: false,
            timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
        }));
    context.messages.push(Message::Assistant(empty_assistant));
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("bridge-user"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("bridge-user-text"),
            text: "after".into(),
        }],
        timestamp: Timestamp::from_unix_millis(1_700_000_000_001),
    }));

    let body: Value = serde_json::from_slice(&encode_direct(
        &model,
        &context,
        OpenAiReasoningPlan::disabled(),
        OrderedJsonObject::new(),
    ))
    .expect("bridge wire");
    let messages = body["messages"].as_array().expect("bridge messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "tool");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "I have processed the tool results.");
    assert_eq!(messages[2]["role"], "user");
}

fn encode_fixture(model: &ModelDescriptor, context: &Context, canonical: &Value) -> Vec<u8> {
    let ApiModelConfig::OpenAiCompletions(config) = &model.api else {
        unreachable!()
    };
    let typed = TypedModelDescriptor::<OpenAiCompletions> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    };
    let compat = OpenAiCompletions::resolve_compat(&model.common.base_url, &config.compat)
        .expect("resolve compat");
    let projected = transform_context_for_model(
        context,
        model,
        &Default::default(),
        &OpenAiCompletionsHandoff,
    )
    .expect("handoff")
    .context;
    let simple = fixture_simple_options(&canonical["options"]);
    let mut options = if canonical["entrypoint"] == "streamSimple" {
        let estimate = estimate_context_tokens(&projected).expect("estimate");
        let available = model
            .common
            .limits
            .context_window
            .saturating_sub(estimate.tokens)
            .saturating_sub(CONTEXT_SAFETY_TOKENS);
        OpenAiCompletions::lower_simple(
            SimpleLoweringContext {
                model: &typed,
                compat: &compat,
                effective_base_url: &model.common.base_url,
                estimated_input_tokens: estimate.tokens,
                available_context_tokens: available,
            },
            &simple,
            &Default::default(),
        )
        .expect("lower simple")
    } else {
        fixture_full_options(&typed, &compat, &simple)
    };
    options.tool_choice = fixture_openai_tool_choice(&canonical["options"]["toolChoice"]);
    let wire = OpenAiCompletions::encode(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compat,
            effective_base_url: &model.common.base_url,
        },
        &options,
    )
    .expect("encode");
    OrderedJsonWriter::to_vec(&wire.into()).expect("write ordered JSON")
}

fn fixture_full_options(
    model: &TypedModelDescriptor<OpenAiCompletions>,
    compat: &OpenAiCompletionsCompat,
    simple: &SimpleGenerationOptions,
) -> OpenAiCompletionsOptions {
    let mut sampling = model.config.sampling_defaults.clone();
    for (name, value) in &simple.sampling {
        sampling.insert(name.clone(), value.clone());
    }
    OpenAiCompletionsOptions {
        max_tokens: simple.max_output_tokens,
        max_tokens_field: compat
            .max_tokens_field
            .unwrap_or(MaxTokensField::MaxCompletionTokens),
        reasoning: OpenAiReasoningPlan::disabled(),
        temperature: simple.temperature,
        sampling,
        tool_choice: simple.tool_choice.map(Into::into),
        cache_retention: simple.cache_retention.unwrap_or_default(),
        session_id: simple.session_id.clone(),
    }
}

fn fixture_model(value: &Value) -> ModelDescriptor {
    let input = value["input"]
        .as_array()
        .expect("input")
        .iter()
        .map(|value| match value.as_str().expect("modality") {
            "text" => Modality::Text,
            "image" => Modality::Image,
            other => panic!("unexpected modality {other}"),
        })
        .collect::<BTreeSet<_>>();
    let mut headers = HeaderMapSpec::new();
    for (name, value) in value["headers"].as_object().into_iter().flatten() {
        headers.insert(name.clone(), value.as_str().map(str::to_owned));
    }
    let pricing = |name: &str| MoneyRate::new(decimal_rate(&value["cost"][name]));
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(
                value["provider"].as_str().expect("provider"),
                value["id"].as_str().expect("model id"),
            ),
            display_name: value["name"].as_str().expect("model name").into(),
            base_url: Url::parse(
                &value["baseUrl"]
                    .as_str()
                    .expect("base URL")
                    .replace("<injected-port>", "4000"),
            )
            .expect("URL"),
            modalities: ModalityCapabilities {
                input,
                output: BTreeSet::from([Modality::Text]),
            },
            limits: ModelLimits {
                context_window: value["contextWindow"].as_u64().expect("context window"),
                max_output_tokens: value["maxTokens"].as_u64().expect("max tokens") as u32,
            },
            pricing: ModelPricing {
                default: TokenPriceRates {
                    input: pricing("input"),
                    output: pricing("output"),
                    cache_read: pricing("cacheRead"),
                    cache_write: pricing("cacheWrite"),
                },
                request_wide_tiers: Vec::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            reasoning: value["reasoning"].as_bool().expect("reasoning"),
            headers,
        },
        api: ApiModelConfig::OpenAiCompletions(OpenAiCompletionsModelConfig {
            compat: fixture_compat(&value["compat"]),
            thinking_levels: fixture_thinking_levels(&value["thinkingLevelMap"]),
            sampling_defaults: ordered_object(&value["samplingParams"]),
        }),
        extensions: Default::default(),
    }
}

fn fixture_compat(value: &Value) -> OpenAiCompletionsCompat {
    let get = |name: &str| value.get(name).and_then(Value::as_bool);
    OpenAiCompletionsCompat {
        supports_store: get("supportsStore"),
        supports_developer_role: get("supportsDeveloperRole"),
        supports_reasoning_effort: get("supportsReasoningEffort"),
        supports_usage_in_streaming: get("supportsUsageInStreaming"),
        supports_finish_reason: get("supportsFinishReason"),
        max_tokens_field: value
            .get("maxTokensField")
            .and_then(Value::as_str)
            .map(|field| match field {
                "max_tokens" => MaxTokensField::MaxTokens,
                "max_completion_tokens" => MaxTokensField::MaxCompletionTokens,
                other => panic!("unknown max tokens field {other}"),
            }),
        requires_tool_result_name: get("requiresToolResultName"),
        requires_assistant_after_tool_result: get("requiresAssistantAfterToolResult"),
        requires_thinking_as_text: get("requiresThinkingAsText"),
        requires_reasoning_content_on_assistant_messages: get(
            "requiresReasoningContentOnAssistantMessages",
        ),
        thinking_format: value
            .get("thinkingFormat")
            .map(|value| serde_json::from_value(value.clone()).expect("thinking format")),
        cache_control_format: value.get("cacheControlFormat").map(|value| {
            match value.as_str().expect("cache format") {
                "anthropic" => CacheControlFormat::Anthropic,
                other => panic!("unknown cache format {other}"),
            }
        }),
        send_session_affinity_headers: get("sendSessionAffinityHeaders"),
        session_affinity_format: value.get("sessionAffinityFormat").map(|value| {
            match value.as_str().expect("session format") {
                "openai" => SessionAffinityFormat::OpenAi,
                "openrouter" => SessionAffinityFormat::OpenRouter,
                other => panic!("unknown session format {other}"),
            }
        }),
        supports_strict_mode: get("supportsStrictMode"),
        supports_long_cache_retention: get("supportsLongCacheRetention"),
        ..Default::default()
    }
}

fn fixture_thinking_levels(value: &Value) -> ThinkingLevelMap<OpenAiThinkingValue> {
    let map = |name| match value.get(name) {
        None => None,
        Some(Value::Null) => Some(LevelSupport::Unsupported),
        Some(Value::String(value)) => Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
            value.clone(),
        ))),
        Some(other) => panic!("unexpected thinking map {other}"),
    };
    ThinkingLevelMap {
        off: map("off"),
        minimal: map("minimal"),
        low: map("low"),
        medium: map("medium"),
        high: map("high"),
        xhigh: map("xhigh"),
        max: map("max"),
    }
}

fn fixture_tool_choice_is_simple(value: &Value) -> bool {
    value.is_null() || matches!(value.as_str(), Some("auto" | "none"))
}

fn fixture_openai_tool_choice(value: &Value) -> Option<OpenAiCompletionsToolChoice> {
    if value.is_null() {
        return None;
    }
    if let Some(value) = value.as_str() {
        return Some(match value {
            "auto" => OpenAiCompletionsToolChoice::Auto,
            "none" => OpenAiCompletionsToolChoice::None,
            "required" => OpenAiCompletionsToolChoice::Required,
            other => panic!("unknown OpenAI tool choice {other}"),
        });
    }

    let kind = value["type"].as_str().expect("OpenAI tool-choice type");
    Some(match kind {
        "function" => OpenAiCompletionsToolChoice::Function {
            name: value["function"]["name"]
                .as_str()
                .expect("named function tool-choice name")
                .into(),
        },
        "custom" => OpenAiCompletionsToolChoice::Custom {
            name: value["custom"]["name"]
                .as_str()
                .expect("named custom tool-choice name")
                .into(),
        },
        "allowed_tools" => {
            let allowed = &value["allowed_tools"];
            let mode = match allowed["mode"].as_str().expect("allowed-tools mode") {
                "auto" => OpenAiAllowedToolsMode::Auto,
                "required" => OpenAiAllowedToolsMode::Required,
                other => panic!("unknown allowed-tools mode {other}"),
            };
            let OrderedJsonValue::Array(tools) = OrderedJsonValue::from(allowed["tools"].clone())
            else {
                panic!("allowed-tools tool references are not an array")
            };
            OpenAiCompletionsToolChoice::AllowedTools { mode, tools }
        }
        other => panic!("unknown OpenAI tool-choice object type {other}"),
    })
}

fn fixture_simple_options(value: &Value) -> SimpleGenerationOptions {
    let mut headers = HeaderMapSpec::new();
    for (name, value) in value["headers"].as_object().into_iter().flatten() {
        headers.insert(name.clone(), value.as_str().map(str::to_owned));
    }
    SimpleGenerationOptions {
        max_output_tokens: value
            .get("maxTokens")
            .and_then(Value::as_u64)
            .map(|v| v as u32),
        temperature: value
            .get("temperature")
            .and_then(Value::as_f64)
            .map(|v| v as f32),
        reasoning: value
            .get("reasoning")
            .and_then(Value::as_str)
            .map(|value| serde_json::from_value(Value::String(value.into())).expect("reasoning")),
        sampling: ordered_object(&value["samplingParams"]),
        session_id: value
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        cache_retention: value
            .get("cacheRetention")
            .and_then(Value::as_str)
            .map(|retention| match retention {
                "none" => CacheRetention::None,
                "short" => CacheRetention::Short,
                "long" => CacheRetention::Long,
                other => panic!("unknown retention {other}"),
            }),
        tool_choice: match value.get("toolChoice").and_then(Value::as_str) {
            Some("auto") => Some(ToolChoice::Auto),
            Some("none") => Some(ToolChoice::None),
            Some("required") | None => None,
            Some(other) => panic!("unknown simple tool choice {other}"),
        },
        headers,
        max_retries: value
            .get("maxRetries")
            .and_then(Value::as_u64)
            .map(|value| u32::try_from(value).expect("max retries fits u32")),
        timeout_ms: value.get("timeoutMs").and_then(Value::as_u64),
        ..Default::default()
    }
}

fn fixture_context(value: &Value, model: &ModelDescriptor) -> Context {
    Context {
        schema_version: 1,
        system_prompt: value
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        messages: value["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .enumerate()
            .map(|(index, value)| fixture_message(value, index, model))
            .collect(),
        tools: value["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .map(fixture_tool)
            .collect(),
    }
}

fn fixture_message(value: &Value, index: usize, model: &ModelDescriptor) -> Message {
    let id = MessageId::new(format!("fixture-message-{index}"));
    let timestamp =
        Timestamp::from_unix_millis(value["timestamp"].as_i64().unwrap_or(1_700_000_000_000));
    match value["role"].as_str().expect("role") {
        "user" => Message::User(UserMessage {
            id,
            content: fixture_content(&value["content"], index),
            timestamp,
        }),
        "assistant" => Message::Assistant(fixture_assistant(value, id, timestamp, model)),
        "toolResult" => Message::ToolResult(fixture_tool_result(value, id, timestamp, index)),
        other => panic!("unknown fixture role {other}"),
    }
}

fn fixture_content(value: &Value, message_index: usize) -> Vec<ContentBlock> {
    if let Some(text) = value.as_str() {
        return vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("fixture-block-{message_index}-0")),
            text: text.into(),
        }];
    }
    value
        .as_array()
        .expect("content")
        .iter()
        .enumerate()
        .map(|(block_index, block)| {
            let id = ContentBlockId::new(format!("fixture-block-{message_index}-{block_index}"));
            match block["type"].as_str().expect("content type") {
                "text" => ContentBlock::Text {
                    id,
                    text: block["text"].as_str().expect("text").into(),
                },
                "image" => ContentBlock::Image {
                    id,
                    data: block["data"].as_str().expect("data").into(),
                    mime_type: block["mimeType"].as_str().expect("mime type").into(),
                },
                "thinking" => ContentBlock::Thinking {
                    id,
                    text: block["thinking"].as_str().expect("thinking").into(),
                    redacted: false,
                    replay_item: None,
                },
                "toolCall" => ContentBlock::ToolCall {
                    id,
                    call: ToolCall {
                        id: ToolCallId::new(block["id"].as_str().expect("tool call id")),
                        name: block["name"].as_str().expect("tool name").into(),
                        arguments: block["arguments"].clone(),
                    },
                },
                other => panic!("unknown content type {other}"),
            }
        })
        .collect()
}

fn fixture_assistant(
    value: &Value,
    id: MessageId,
    timestamp: Timestamp,
    model: &ModelDescriptor,
) -> AssistantMessage {
    let mut content = fixture_content(&value["content"], 10_000);
    let provider = value["provider"]
        .as_str()
        .unwrap_or(model.common.model_ref.provider.as_str());
    let api = value["api"].as_str().unwrap_or("openai-completions");
    let requested_model = value["model"]
        .as_str()
        .unwrap_or(model.common.model_ref.model.as_str());
    let source = ReplayScope::new(provider, api, requested_model, requested_model);
    let mut replay = ReplayEnvelope::new(source);
    for (content_index, block) in content.iter_mut().enumerate() {
        let ContentBlock::Thinking {
            id: block_id,
            replay_item,
            ..
        } = block
        else {
            continue;
        };
        let source_block = value["content"]
            .as_array()
            .and_then(|blocks| blocks.get(content_index));
        let Some(signature) = source_block
            .and_then(|block| block.get("thinkingSignature"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if let Ok(Value::Array(details)) = serde_json::from_str::<Value>(signature) {
            for detail in details {
                let ordered = OrderedJsonWriter::to_vec(&OrderedJsonValue::from(detail))
                    .expect("detail bytes");
                let item_id = ReplayItemId::new(format!("fixture-replay-{}", replay.items.len()));
                if replay_item.is_none() {
                    *replay_item = Some(item_id.clone());
                }
                replay.items.push(ReplayItem {
                    id: item_id,
                    ordinal: replay.items.len() as u32,
                    target: ReplayTarget::ContentBlock(block_id.clone()),
                    kind: ReplayKind::new(OPENAI_CHAT_REASONING_DETAIL_KIND),
                    applicability: ReplayApplicability::ExactProviderApiModel,
                    completeness: ReplayCompleteness::Complete,
                    payload: pi_ai::OpaquePayload::JsonBytes(ordered),
                });
            }
        } else {
            replay.items.push(ReplayItem {
                id: ReplayItemId::new(format!("fixture-replay-{}", replay.items.len())),
                ordinal: replay.items.len() as u32,
                target: ReplayTarget::ContentBlock(block_id.clone()),
                kind: ReplayKind::new(OPENAI_CHAT_REASONING_FIELD_KIND),
                applicability: ReplayApplicability::ExactProviderApiModel,
                completeness: ReplayCompleteness::Complete,
                payload: pi_ai::OpaquePayload::Utf8(signature.into()),
            });
        }
    }
    AssistantMessage {
        id,
        provider: provider.into(),
        api: api.into(),
        requested_model: requested_model.into(),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        content,
        replay,
        usage: fixture_usage(&value["usage"]),
        cost: None,
        finish: AssistantFinish {
            reason: match value["stopReason"].as_str().unwrap_or("stop") {
                "stop" => AssistantFinishReason::Stop,
                "length" => AssistantFinishReason::Length,
                "toolUse" => AssistantFinishReason::ToolUse,
                "error" => AssistantFinishReason::Error,
                "aborted" => AssistantFinishReason::Aborted,
                other => panic!("unknown stop reason {other}"),
            },
            raw_provider_reason: None,
            error: None,
        },
        timestamp,
    }
}

fn fixture_tool_result(
    value: &Value,
    id: MessageId,
    timestamp: Timestamp,
    message_index: usize,
) -> ToolResultMessage {
    let content = value["content"]
        .as_array()
        .expect("tool content")
        .iter()
        .enumerate()
        .map(|(block_index, block)| {
            let id =
                ContentBlockId::new(format!("fixture-tool-block-{message_index}-{block_index}"));
            match block["type"].as_str().expect("tool block type") {
                "text" => ToolResultContent::Text {
                    id,
                    text: block["text"].as_str().expect("tool text").into(),
                },
                "image" => ToolResultContent::Image {
                    id,
                    data: block["data"].as_str().expect("tool data").into(),
                    mime_type: block["mimeType"].as_str().expect("tool mime").into(),
                },
                other => panic!("unknown tool result type {other}"),
            }
        })
        .collect();
    ToolResultMessage {
        id,
        tool_call_id: ToolCallId::new(value["toolCallId"].as_str().expect("tool call id")),
        tool_name: value["toolName"].as_str().unwrap_or("").into(),
        content,
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: value["isError"].as_bool().unwrap_or(false),
        timestamp,
    }
}

fn fixture_tool(value: &Value) -> ToolSpec {
    let constrained_sampling = value.get("constrainedSampling").map(|value| {
        let strict = match value["strict"].as_str().expect("strict") {
            "require" => JsonSchemaStrictMode::Require,
            "prefer" => JsonSchemaStrictMode::Prefer,
            other => panic!("unknown strict mode {other}"),
        };
        ConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict })
    });
    ToolSpec {
        schema_version: 1,
        name: value["name"].as_str().expect("tool name").into(),
        description: value["description"].as_str().expect("description").into(),
        parameters: value["parameters"].clone(),
        constrained_sampling,
    }
}

fn fixture_usage(value: &Value) -> Usage {
    Usage {
        input_tokens: value["input"].as_u64().unwrap_or(0),
        output_tokens: value["output"].as_u64().unwrap_or(0),
        reasoning_tokens: value.get("reasoning").and_then(Value::as_u64),
        cache_read_tokens: value.get("cacheRead").and_then(Value::as_u64),
        cache_write_tokens: value.get("cacheWrite").and_then(Value::as_u64),
        cache_write_one_hour_tokens: value.get("cacheWrite1h").and_then(Value::as_u64),
        total_tokens: value.get("totalTokens").and_then(Value::as_u64),
        source: UsageSource::Unknown,
    }
}

fn ordered_object(value: &Value) -> OrderedJsonObject {
    if value.is_null() {
        return OrderedJsonObject::new();
    }
    let source = serde_json::to_string(value).expect("serialize ordered object");
    match parse_ordered_json(&source).expect("parse ordered object") {
        OrderedJsonValue::Object(value) => value,
        _ => panic!("sampling params are not an object"),
    }
}

fn decimal_rate(value: &Value) -> i128 {
    let value = value.as_number().expect("cost").to_string();
    let (whole, fraction) = value.split_once('.').unwrap_or((&value, ""));
    whole.parse::<i128>().expect("whole") * 1_000_000
        + if fraction.is_empty() {
            0
        } else {
            fraction.parse::<i128>().expect("fraction") * 10_i128.pow((6 - fraction.len()) as u32)
        }
}

fn terminal_message(events: &[AssistantEvent]) -> &AssistantMessage {
    events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("terminal assistant event")
}

fn base_fixture_model() -> ModelDescriptor {
    let canonical: Value = serde_json::from_slice(
        &fs::read(Path::new(FIXTURE_ROOT).join("text-only/canonical.json")).expect("base fixture"),
    )
    .expect("base fixture JSON");
    fixture_model(&canonical["model"])
}

fn one_user_context() -> Context {
    let mut context = Context::new(None);
    context.messages.push(Message::User(UserMessage {
        id: MessageId::new("user-1"),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new("user-text-1"),
            text: "hello".into(),
        }],
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    }));
    context
}

fn default_full_options(
    reasoning: OpenAiReasoningPlan,
    sampling: OrderedJsonObject,
) -> OpenAiCompletionsOptions {
    OpenAiCompletionsOptions {
        max_tokens: None,
        max_tokens_field: MaxTokensField::MaxTokens,
        reasoning,
        temperature: None,
        sampling,
        tool_choice: Default::default(),
        cache_retention: CacheRetention::Short,
        session_id: None,
    }
}

fn encode_direct(
    model: &ModelDescriptor,
    context: &Context,
    reasoning: OpenAiReasoningPlan,
    sampling: OrderedJsonObject,
) -> Vec<u8> {
    encode_options(model, context, default_full_options(reasoning, sampling))
}

fn encode_options(
    model: &ModelDescriptor,
    context: &Context,
    options: OpenAiCompletionsOptions,
) -> Vec<u8> {
    try_encode_options(model, context, options).expect("encode options")
}

fn try_encode_options(
    model: &ModelDescriptor,
    context: &Context,
    options: OpenAiCompletionsOptions,
) -> Result<Vec<u8>, pi_ai::EncodeError> {
    let ApiModelConfig::OpenAiCompletions(config) = &model.api else {
        unreachable!()
    };
    let typed = TypedModelDescriptor::<OpenAiCompletions> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    };
    let compat =
        OpenAiCompletions::resolve_compat(&model.common.base_url, &config.compat).expect("compat");
    let wire = OpenAiCompletions::encode(
        EncodeContext {
            model: &typed,
            context,
            compat: &compat,
            effective_base_url: &model.common.base_url,
        },
        &options,
    )?;
    OrderedJsonWriter::to_vec(&wire.into()).map_err(|error| pi_ai::EncodeError::InvalidRequest {
        message: format!("ordered wire encoding failed: {error}"),
    })
}

fn lower_simple_options(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
) -> OpenAiCompletionsOptions {
    let ApiModelConfig::OpenAiCompletions(config) = &model.api else {
        unreachable!()
    };
    let typed = TypedModelDescriptor::<OpenAiCompletions> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: Default::default(),
    };
    let compat = OpenAiCompletions::resolve_compat(&model.common.base_url, &config.compat)
        .expect("resolve compat");
    let estimate = estimate_context_tokens(context).expect("estimate context");
    let available = model
        .common
        .limits
        .context_window
        .saturating_sub(estimate.tokens)
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    OpenAiCompletions::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compat,
            effective_base_url: &model.common.base_url,
            estimated_input_tokens: estimate.tokens,
            available_context_tokens: available,
        },
        simple,
        &Default::default(),
    )
    .expect("lower simple")
}

fn object_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {},
        "required": []
    })
}

fn tool_spec(
    name: &str,
    parameters: Value,
    constrained_sampling: Option<ConstrainedSampling>,
) -> ToolSpec {
    ToolSpec {
        schema_version: 1,
        name: name.into(),
        description: format!("{name} tool"),
        parameters,
        constrained_sampling,
    }
}

fn assistant_with_thinking_detail() -> AssistantMessage {
    let block_id = ContentBlockId::new("thinking-1");
    let item_id = ReplayItemId::new("reasoning-detail-1");
    AssistantMessage {
        id: MessageId::new("assistant-1"),
        provider: "fixture-openai".into(),
        api: "openai-completions".into(),
        requested_model: "fixture-openai-model".into(),
        response_model: None,
        response_id: Some("chat-1".into()),
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::Thinking {
            id: block_id.clone(),
            text: "thought".into(),
            redacted: false,
            replay_item: Some(item_id.clone()),
        }],
        replay: ReplayEnvelope {
            schema_version: 1,
            source: ReplayScope::new(
                "fixture-openai",
                "openai-completions",
                "fixture-openai-model",
                "fixture-openai-model",
            ),
            items: vec![ReplayItem {
                id: item_id,
                ordinal: 0,
                target: ReplayTarget::ContentBlock(block_id),
                kind: ReplayKind::new(OPENAI_CHAT_REASONING_DETAIL_KIND),
                applicability: ReplayApplicability::ExactProviderApiModel,
                completeness: ReplayCompleteness::Complete,
                payload: pi_ai::OpaquePayload::JsonBytes(
                    br#"{"type":"reasoning.text","id":"r1","text":"thought","signature":"sig"}"#
                        .to_vec(),
                ),
            }],
        },
        usage: Usage::zero(UsageSource::ProviderReported),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: Some("stop".into()),
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    }
}

fn assistant_with_tool_call() -> AssistantMessage {
    AssistantMessage {
        id: MessageId::new("assistant-tool"),
        provider: "fixture-openai".into(),
        api: "openai-completions".into(),
        requested_model: "fixture-openai-model".into(),
        response_model: None,
        response_id: Some("chat-tool".into()),
        end_turn: None,
        diagnostics: Vec::new(),
        content: vec![ContentBlock::ToolCall {
            id: ContentBlockId::new("tool-block-1"),
            call: ToolCall {
                id: ToolCallId::new("call-1"),
                name: "read_file".into(),
                arguments: serde_json::json!({"path":"Cargo.toml"}),
            },
        }],
        replay: ReplayEnvelope::new(ReplayScope::new(
            "fixture-openai",
            "openai-completions",
            "fixture-openai-model",
            "fixture-openai-model",
        )),
        usage: Usage::zero(UsageSource::ProviderReported),
        cost: None,
        finish: AssistantFinish {
            reason: AssistantFinishReason::ToolUse,
            raw_provider_reason: Some("tool_calls".into()),
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    }
}

fn legacy_encrypted_detail() -> String {
    r#"{"type":"reasoning.encrypted","id":"encrypted-1","data":"cipher"}"#.into()
}

fn decode_context() -> OpenAiCompletionsDecodeContext {
    OpenAiCompletionsDecodeContext {
        message_id: MessageId::new("decoder-message"),
        provider: "fixture-provider".into(),
        requested_model: "fixture".into(),
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
        supports_finish_reason: true,
        grammar_tool_input_properties: BTreeMap::new(),
    }
}

fn grammar_decode_context() -> OpenAiCompletionsDecodeContext {
    OpenAiCompletionsDecodeContext {
        grammar_tool_input_properties: BTreeMap::from([("query".into(), "expression".into())]),
        ..decode_context()
    }
}

fn case_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_owned()
}
