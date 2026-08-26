use agentprism_ai::*;
use futures_executor::block_on;
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::{json, value::RawValue};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use url::Url;

const SIMPLE_BASIS: &str = "packages/ai/src/api/simple-options.ts:1-95; packages/ai/src/api/anthropic-messages.ts:832-880; packages/ai/src/utils/estimate.ts:1-143; packages/ai/test/sampling-options.test.ts:1-128; architecture v2 part 2 §10.5";
const REASONING_BASIS: &str = "packages/ai/src/models.ts:881-932; packages/ai/test/max-thinking.test.ts:1-89; architecture v2 part 2 §§3.7, 10.5";
const HANDOFF_BASIS: &str = "packages/ai/src/api/transform-messages.ts:1-223; packages/ai/test/transform-messages-copilot-openai-to-anthropic.test.ts:1-191; architecture v2 part 2 §10.6";

macro_rules! pi_basis {
    ($basis:expr) => {
        let _pi_basis = $basis;
    };
}

fn pricing() -> ModelPricing {
    ModelPricing {
        default: TokenPriceRates::default(),
        request_wide_tiers: Vec::new(),
        cache_write_retention: CacheWriteRetentionPricing::default(),
    }
}

fn target_model(provider: &str, api: &str, model: &str, supports_images: bool) -> ModelDescriptor {
    let mut input = BTreeSet::from([Modality::Text]);
    if supports_images {
        input.insert(Modality::Image);
    }
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, model),
            display_name: model.to_owned(),
            base_url: Url::parse("https://target.example/v1").unwrap(),
            modalities: ModalityCapabilities {
                input,
                output: BTreeSet::from([Modality::Text]),
            },
            limits: ModelLimits {
                context_window: 16_384,
                max_output_tokens: 4_096,
            },
            pricing: pricing(),
            reasoning: true,
            headers: HeaderMapSpec::new(),
        },
        api: ApiModelConfig::Custom(CustomApiModelConfig {
            api: ApiId::new(api),
            schema_version: 1,
            value: RawValue::from_string("{}".to_owned()).unwrap(),
        }),
        extensions: ExtensionMap::new(),
    }
}

fn sampling_model(defaults: OrderedJsonObject) -> ModelDescriptor {
    let mut model = target_model("provider", "openai-completions", "model", true);
    model.api = ApiModelConfig::OpenAiCompletions(OpenAiCompletionsModelConfig {
        compat: OpenAiCompletionsCompat::default(),
        thinking_levels: ThinkingLevelMap::default(),
        sampling_defaults: defaults,
    });
    model
}

fn text(id: &str, value: &str) -> ContentBlock {
    ContentBlock::Text {
        id: ContentBlockId::new(id),
        text: value.to_owned(),
    }
}

fn thinking(id: &str, value: &str, redacted: bool) -> ContentBlock {
    ContentBlock::Thinking {
        id: ContentBlockId::new(id),
        text: value.to_owned(),
        redacted,
        replay_item: None,
    }
}

fn image(id: &str) -> ContentBlock {
    ContentBlock::Image {
        id: ContentBlockId::new(id),
        data: "aGVsbG8=".to_owned(),
        mime_type: "image/png".to_owned(),
    }
}

fn tool_call(block_id: &str, call_id: &str, name: &str) -> ContentBlock {
    ContentBlock::ToolCall {
        id: ContentBlockId::new(block_id),
        call: ToolCall {
            id: ToolCallId::new(call_id),
            name: name.to_owned(),
            arguments: json!({}),
        },
    }
}

fn user(id: &str, content: Vec<ContentBlock>) -> Message {
    Message::User(UserMessage {
        id: MessageId::new(id),
        content,
        timestamp: Timestamp::from_unix_millis(1),
    })
}

fn assistant(
    id: &str,
    provider: &str,
    api: &str,
    model: &str,
    content: Vec<ContentBlock>,
    finish: AssistantFinishReason,
) -> AssistantMessage {
    AssistantMessage {
        id: MessageId::new(id),
        provider: ProviderId::new(provider),
        api: ApiId::new(api),
        requested_model: ModelId::new(model),
        response_model: None,
        response_id: None,
        end_turn: None,
        diagnostics: Vec::new(),
        deferred: None,
        content,
        replay: ReplayEnvelope::new(ReplayScope::new(provider, api, model, model)),
        usage: Usage::zero(UsageSource::Unknown),
        cost: None,
        finish: AssistantFinish {
            reason: finish,
            raw_provider_reason: None,
            error: None,
        },
        timestamp: Timestamp::from_unix_millis(2),
    }
}

fn tool_result(id: &str, call_id: &str, tool_name: &str) -> Message {
    Message::ToolResult(ToolResultMessage {
        id: MessageId::new(id),
        tool_call_id: ToolCallId::new(call_id),
        tool_name: tool_name.to_owned(),
        content: vec![ToolResultContent::Text {
            id: ContentBlockId::new(format!("{id}-content")),
            text: "result".to_owned(),
        }],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: false,
        timestamp: Timestamp::from_unix_millis(3),
    })
}

fn context(messages: Vec<Message>) -> Context {
    Context {
        schema_version: CONTEXT_SCHEMA_VERSION,
        system_prompt: None,
        messages,
        tools: Vec::new(),
    }
}

fn replay_item(
    id: &str,
    ordinal: u32,
    target: ReplayTarget,
    kind: &str,
    applicability: ReplayApplicability,
) -> ReplayItem {
    ReplayItem {
        id: ReplayItemId::new(id),
        ordinal,
        target,
        kind: ReplayKind::new(kind),
        applicability,
        completeness: ReplayCompleteness::Complete,
        payload: OpaquePayload::Utf8("opaque".to_owned()),
    }
}

#[derive(Clone, Copy, Debug)]
struct TestApiHandoff {
    recognize_replay: bool,
    sanitize_ids: bool,
}

impl ToolCallIdPolicy for TestApiHandoff {
    fn normalize(
        &self,
        original: &ToolCallId,
        _source: &ModelFingerprint,
        _target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        if self.sanitize_ids {
            Ok(ToolCallId::new(
                original
                    .as_str()
                    .chars()
                    .map(|character| {
                        if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                            character
                        } else {
                            '_'
                        }
                    })
                    .take(64)
                    .collect::<String>(),
            ))
        } else {
            Ok(original.clone())
        }
    }
}

impl ApiFamilyHandoff for TestApiHandoff {
    fn recognizes_replay_kind(&self, _kind: &ReplayKind) -> bool {
        self.recognize_replay
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        self
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}

const ALL_REPLAY: TestApiHandoff = TestApiHandoff {
    recognize_replay: true,
    sanitize_ids: false,
};
const SANITIZE_IDS: TestApiHandoff = TestApiHandoff {
    recognize_replay: true,
    sanitize_ids: true,
};

#[derive(Clone, Copy, Debug)]
struct SourceSensitiveIds;

impl ToolCallIdPolicy for SourceSensitiveIds {
    fn normalize(
        &self,
        original: &ToolCallId,
        source: &ModelFingerprint,
        _target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        Ok(ToolCallId::new(format!(
            "{}__{}",
            source.model.as_str(),
            original.as_str()
        )))
    }
}

impl ApiFamilyHandoff for SourceSensitiveIds {
    fn recognizes_replay_kind(&self, _kind: &ReplayKind) -> bool {
        true
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        self
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
struct SelectiveIds;

impl ToolCallIdPolicy for SelectiveIds {
    fn normalize(
        &self,
        original: &ToolCallId,
        source: &ModelFingerprint,
        _target: &ModelFingerprint,
    ) -> Result<ToolCallId, HandoffError> {
        if source.model.as_str() == "changed" {
            Ok(ToolCallId::new(format!("changed__{}", original.as_str())))
        } else {
            Ok(original.clone())
        }
    }
}

impl ApiFamilyHandoff for SelectiveIds {
    fn recognizes_replay_kind(&self, _kind: &ReplayKind) -> bool {
        true
    }

    fn tool_call_id_policy(&self) -> &dyn ToolCallIdPolicy {
        self
    }

    fn final_shape(
        &self,
        _context: &mut Context,
        _report: &mut HandoffReport,
    ) -> Result<(), HandoffError> {
        Ok(())
    }
}

fn project(
    source: &Context,
    target: &ModelDescriptor,
    policy: &HandoffPolicy,
    hooks: &dyn ApiFamilyHandoff,
) -> HandoffResult {
    transform_context_for_model(source, target, policy, hooks).unwrap()
}

#[test]
fn simple_context_reserves_4096_tokens() {
    pi_basis!(SIMPLE_BASIS);
    let numeric_arguments = json!({"large": 1e20, "negative_zero": -0.0});
    assert_eq!(
        json_stringify_compatible(&numeric_arguments).unwrap(),
        r#"{"large":100000000000000000000,"negative_zero":0}"#
    );
    let numeric_call = Message::Assistant(assistant(
        "numeric-assistant",
        "openai",
        "openai-completions",
        "model",
        vec![ContentBlock::ToolCall {
            id: ContentBlockId::new("numeric-call-block"),
            call: ToolCall {
                id: ToolCallId::new("numeric-call"),
                name: "x".to_owned(),
                arguments: numeric_arguments,
            },
        }],
        AssistantFinishReason::ToolUse,
    ));
    assert_eq!(estimate_message_tokens(&numeric_call).unwrap(), 13);

    let numeric_schema_context = Context {
        tools: vec![ToolSpec {
            schema_version: 1,
            name: "schema".to_owned(),
            description: String::new(),
            parameters: json!({"maximum": 1e20, "multipleOf": -0.0}),
            constrained_sampling: None,
        }],
        ..Context::new(None)
    };
    assert_eq!(
        estimate_context_tokens(&numeric_schema_context)
            .unwrap()
            .tokens,
        25
    );

    let mut model = sampling_model(OrderedJsonObject::new());
    model.common.limits = ModelLimits {
        context_window: 9_096,
        max_output_tokens: 8_000,
    };
    let source = context(vec![user("user", vec![text("text", &"a".repeat(4_000))])]);
    let plan = plan_common(
        &model,
        &source,
        &SimpleGenerationOptions::default(),
        &PiTokenEstimator,
    )
    .unwrap();
    assert_eq!(plan.max_output_tokens, 4_000);

    // Pinned Pi ignores usage attached to an assistant older than a message
    // inserted before it, then accepts usage again after a newer response.
    let mut summary = match user("summary", vec![text("summary-text", "summary")]) {
        Message::User(message) => message,
        _ => unreachable!(),
    };
    summary.timestamp = Timestamp::from_unix_millis(200);
    let mut stale = assistant(
        "stale",
        "openai",
        "openai-responses",
        "test-model",
        vec![text("stale-text", "kept")],
        AssistantFinishReason::Stop,
    );
    stale.timestamp = Timestamp::from_unix_millis(100);
    stale.usage.input_tokens = 9_500;
    let mut trailing = match user("trailing", vec![text("trailing-text", &"x".repeat(4_000))]) {
        Message::User(message) => message,
        _ => unreachable!(),
    };
    trailing.timestamp = Timestamp::from_unix_millis(300);
    let stale_context = Context {
        system_prompt: Some("system".to_owned()),
        messages: vec![
            Message::User(summary),
            Message::Assistant(stale),
            Message::User(trailing),
        ],
        ..Context::new(None)
    };
    assert_eq!(
        estimate_context_tokens(&stale_context).unwrap(),
        ContextUsageEstimate {
            tokens: 1_005,
            usage_tokens: 0,
            trailing_tokens: 1_005,
            last_usage_index: None,
        }
    );

    let mut fresh = assistant(
        "fresh",
        "openai",
        "openai-responses",
        "test-model",
        vec![text("fresh-text", "kept")],
        AssistantFinishReason::Stop,
    );
    fresh.timestamp = Timestamp::from_unix_millis(400);
    fresh.usage.input_tokens = 2_000;
    let mut tail = match user("tail", vec![text("tail-text", "tail")]) {
        Message::User(message) => message,
        _ => unreachable!(),
    };
    tail.timestamp = Timestamp::from_unix_millis(500);
    let mut fresh_context = stale_context;
    fresh_context
        .messages
        .extend([Message::Assistant(fresh), Message::User(tail)]);
    assert_eq!(
        estimate_context_tokens(&fresh_context).unwrap(),
        ContextUsageEstimate {
            tokens: 2_001,
            usage_tokens: 2_000,
            trailing_tokens: 1,
            last_usage_index: Some(3),
        }
    );

    // packages/ai/test/deferred-tools.test.ts: definitions marked after the
    // latest provider-usage checkpoint are charged as trailing context.
    let plain = estimate_context_tokens(&fresh_context).unwrap();
    let mut marked = fresh_context;
    marked.tools.push(ToolSpec {
        schema_version: 1,
        name: "late_tool".into(),
        description: "x".repeat(4_000),
        parameters: json!({"type":"object","properties":{"value":{"type":"string"}}}),
        constrained_sampling: None,
    });
    marked.messages.push(Message::ToolResult(ToolResultMessage {
        id: MessageId::new("late-marker"),
        tool_call_id: ToolCallId::new("loader"),
        tool_name: "load_tools".into(),
        content: Vec::new(),
        details: None,
        usage: None,
        added_tool_names: vec!["late_tool".into()],
        is_error: false,
        timestamp: Timestamp::from_unix_millis(600),
    }));
    let marked = estimate_context_tokens(&marked).unwrap();
    assert!(marked.tokens > plain.tokens + 500);
    assert!(marked.trailing_tokens > plain.trailing_tokens + 500);
}

#[test]
fn simple_context_clamp_never_returns_zero() {
    pi_basis!(SIMPLE_BASIS);
    let mut model = sampling_model(OrderedJsonObject::new());
    model.common.limits.context_window = 1;
    let source = context(vec![user("user", vec![text("text", "already full")])]);
    assert_eq!(
        plan_common(
            &model,
            &source,
            &SimpleGenerationOptions::default(),
            &PiTokenEstimator,
        )
        .unwrap()
        .max_output_tokens,
        1
    );
}

#[test]
fn simple_max_output_respects_model_limit() {
    pi_basis!(SIMPLE_BASIS);
    let mut model = sampling_model(OrderedJsonObject::new());
    let options = SimpleGenerationOptions {
        max_output_tokens: Some(10_000),
        ..SimpleGenerationOptions::default()
    };
    // Pinned Pi uses the catalog maximum as the default, but does not cap an
    // explicit request to it when positive remaining context permits more.
    assert_eq!(
        plan_common(&model, &context(Vec::new()), &options, &PiTokenEstimator)
            .unwrap()
            .max_output_tokens,
        10_000
    );
    assert_eq!(
        plan_common(
            &model,
            &context(Vec::new()),
            &SimpleGenerationOptions::default(),
            &PiTokenEstimator,
        )
        .unwrap()
        .max_output_tokens,
        model.common.limits.max_output_tokens
    );

    // A nonpositive catalog context window bypasses estimation and context
    // clamping entirely, including for an explicit cap above the model value.
    model.common.limits.context_window = 0;
    assert_eq!(
        plan_common(&model, &context(Vec::new()), &options, &PiTokenEstimator)
            .unwrap()
            .max_output_tokens,
        10_000
    );
}

#[test]
fn simple_model_sampling_defaults_apply() {
    pi_basis!(SIMPLE_BASIS);
    let defaults = OrderedJsonObject::from_iter([
        ("temperature".to_owned(), json!(0.25)),
        ("top_p".to_owned(), json!(0.8)),
        ("seed".to_owned(), json!(7)),
        ("top_k".to_owned(), json!(40)),
    ]);
    let plan = plan_common(
        &sampling_model(defaults),
        &context(Vec::new()),
        &SimpleGenerationOptions::default(),
        &PiTokenEstimator,
    )
    .unwrap();
    assert_eq!(plan.sampling.temperature, None);
    assert_eq!(plan.sampling.top_p, None);
    assert_eq!(plan.sampling.seed, None);
    assert_eq!(
        plan.sampling.additional.get("temperature"),
        Some(&OrderedJsonValue::from(json!(0.25)))
    );
    assert_eq!(
        plan.sampling.additional.get("top_p"),
        Some(&OrderedJsonValue::from(json!(0.8)))
    );
    assert_eq!(
        plan.sampling.additional.get("seed"),
        Some(&OrderedJsonValue::from(json!(7)))
    );
    assert_eq!(
        plan.sampling.additional.get("top_k"),
        Some(&OrderedJsonValue::from(json!(40)))
    );
}

#[test]
fn simple_request_sampling_overrides_model_defaults() {
    pi_basis!(SIMPLE_BASIS);
    let defaults = OrderedJsonObject::from_iter([
        ("temperature".to_owned(), json!(1)),
        ("top_p".to_owned(), json!(0.8)),
        ("seed".to_owned(), json!(7)),
        ("min_p".to_owned(), json!(0.05)),
    ]);
    let options = SimpleGenerationOptions {
        temperature: Some(0.0),
        top_p: Some(0.4),
        seed: Some(99),
        sampling: OrderedJsonObject::from_iter([
            ("temperature".to_owned(), json!(0.75)),
            ("top_p".to_owned(), json!(0.6)),
            ("top_k".to_owned(), json!(20)),
        ]),
        ..SimpleGenerationOptions::default()
    };
    let plan = plan_common(
        &sampling_model(defaults),
        &context(Vec::new()),
        &options,
        &PiTokenEstimator,
    )
    .unwrap();
    // Named fields remain named; model/request samplingParams stay in a
    // separate later overlay. An OpenAI-family encoder therefore emits 0.75,
    // not the named 0.0 or the catalog 1.0.
    assert_eq!(plan.sampling.temperature, Some(0.0));
    assert_eq!(plan.sampling.top_p, Some(0.4));
    assert_eq!(plan.sampling.seed, Some(99));
    assert_eq!(
        plan.sampling.additional.get("temperature"),
        Some(&OrderedJsonValue::from(json!(0.75)))
    );
    assert_eq!(
        plan.sampling.additional.get("top_p"),
        Some(&OrderedJsonValue::from(json!(0.6)))
    );
    assert_eq!(
        plan.sampling.additional.get("top_k"),
        Some(&OrderedJsonValue::from(json!(20)))
    );
    assert_eq!(
        plan.sampling.additional.get("min_p"),
        Some(&OrderedJsonValue::from(json!(0.05)))
    );
}

struct PatchApi;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Patch {
    max_output_tokens: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct PatchWire;

impl ApiFamily for PatchApi {
    const API_ID: &'static str = "patch-api";
    type Compat = ();
    type ModelConfig = ();
    type FullOptions = u32;
    type OptionsPatch = Patch;
    type WireRequest = PatchWire;

    fn resolve_compat(
        _effective_base_url: &Url,
        _model_overrides: &Self::Compat,
    ) -> Result<Self::Compat, LoweringError> {
        Ok(())
    }

    fn lower_simple(
        context: SimpleLoweringContext<'_, Self>,
        simple: &SimpleGenerationOptions,
        patch: &Self::OptionsPatch,
    ) -> Result<Self::FullOptions, LoweringError> {
        let model = ModelDescriptor {
            common: context.model.common.clone(),
            api: ApiModelConfig::Custom(CustomApiModelConfig {
                api: ApiId::new(Self::API_ID),
                schema_version: 1,
                value: RawValue::from_string("{}".to_owned()).unwrap(),
            }),
            extensions: context.model.extensions.clone(),
        };
        let common = plan_common(&model, &Context::new(None), simple, &PiTokenEstimator)?;
        Ok(patch.max_output_tokens.unwrap_or(common.max_output_tokens))
    }

    fn encode(
        _context: EncodeContext<'_, Self>,
        _options: &Self::FullOptions,
    ) -> Result<Self::WireRequest, EncodeError> {
        Ok(PatchWire)
    }
}

#[test]
fn simple_api_patch_overrides_common_simple_field() {
    pi_basis!(SIMPLE_BASIS);
    let model = target_model("provider", PatchApi::API_ID, "model", true);
    let typed = TypedModelDescriptor::<PatchApi> {
        common: model.common,
        config: (),
        extensions: model.extensions,
    };
    let compat = ();
    let endpoint = Url::parse("https://effective.example/v1").unwrap();
    let options = SimpleGenerationOptions {
        max_output_tokens: Some(2_000),
        ..SimpleGenerationOptions::default()
    };
    let lowered = PatchApi::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compat,
            effective_base_url: &endpoint,
            estimated_input_tokens: 0,
            available_context_tokens: 12_288,
        },
        &options,
        &Patch {
            max_output_tokens: Some(333),
        },
    )
    .unwrap();
    assert_eq!(lowered, 333);
}

#[test]
fn simple_typed_and_erased_patch_conflict() {
    pi_basis!(SIMPLE_BASIS);
    let erased = ErasedApiOptionsPatch {
        api: ApiId::new(PatchApi::API_ID),
        schema_version: 1,
        value: RawValue::from_string("{}".to_owned()).unwrap(),
    };
    assert!(matches!(
        ApiOptionsInput::<PatchApi>::from_sources(Some(Patch::default()), Some(erased)),
        Err(LoweringError::ConflictingApiOptions { .. })
    ));
}

#[test]
fn simple_unknown_api_patch_rejected() {
    pi_basis!(SIMPLE_BASIS);
    let erased = ErasedApiOptionsPatch {
        api: ApiId::new("different-api"),
        schema_version: 1,
        value: RawValue::from_string("{}".to_owned()).unwrap(),
    };
    assert!(matches!(
        ApiOptionsInput::<PatchApi>::from_sources(None, Some(erased)),
        Err(LoweringError::UnknownApiOptions { .. })
    ));

    let runtime_patch = ErasedApiOptionsPatch {
        api: ApiId::new("different-api"),
        schema_version: 1,
        value: RawValue::from_string("{}".to_owned()).unwrap(),
    };
    let send_api = HttpChatApi::new(
        Arc::new(PatchValidationSendHandler {
            api: ApiId::new(PatchApi::API_ID),
        }),
        Arc::new(UnreachableSendTransport),
    );
    let send_result = block_on(ChatApi::stream(
        &send_api,
        send_patch_request(runtime_patch.clone()),
        CancellationToken::new(),
    ));
    assert!(matches!(
        send_result,
        Err(AiError {
            kind: AiErrorKind::InvalidRequest,
            ..
        })
    ));

    let local_api = LocalHttpChatApi::new(
        Rc::new(PatchValidationLocalHandler {
            api: ApiId::new(PatchApi::API_ID),
        }),
        Rc::new(UnreachableLocalTransport),
    );
    let local_result = block_on(LocalChatApi::stream(
        &local_api,
        local_patch_request(runtime_patch),
        CancellationToken::new(),
    ));
    assert!(matches!(
        local_result,
        Err(AiError {
            kind: AiErrorKind::InvalidRequest,
            ..
        })
    ));
}

struct PatchValidationSendHandler {
    api: ApiId,
}

impl ErasedApiHandler for PatchValidationSendHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _simple: &SimpleGenerationOptions,
        _patch: Option<&ErasedApiOptionsPatch>,
        _execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        panic!("mismatched patch reached send erased handler")
    }

    fn decode_stream(
        &self,
        _response: ProviderResponseStream,
        _execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        AssistantStream::new(futures_util::stream::empty())
    }
}

struct PatchValidationLocalHandler {
    api: ApiId,
}

impl LocalErasedApiHandler for PatchValidationLocalHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        _model: &ModelDescriptor,
        _context: &Context,
        _simple: &SimpleGenerationOptions,
        _patch: Option<&ErasedApiOptionsPatch>,
        _execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        panic!("mismatched patch reached local erased handler")
    }

    fn decode_stream(
        &self,
        _response: LocalProviderResponseStream,
        _execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        LocalAssistantStream::new(futures_util::stream::empty())
    }
}

struct UnreachableSendTransport;

impl HttpTransport for UnreachableSendTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        Box::pin(async { panic!("mismatched patch reached send transport") })
    }
}

struct UnreachableLocalTransport;

impl LocalHttpTransport for UnreachableLocalTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, TransportError>> {
        Box::pin(async { panic!("mismatched patch reached local transport") })
    }
}

fn send_patch_request(patch: ErasedApiOptionsPatch) -> ResolvedApiRequest {
    ResolvedApiRequest {
        model: target_model("provider", PatchApi::API_ID, "model", true),
        context: Context::new(None),
        options: SimpleGenerationOptions {
            api_options: Some(patch),
            ..SimpleGenerationOptions::default()
        },
        full_options: None,
        request_options: agentprism_ai::ApiRequestOptions::default(),
        endpoint: Url::parse("https://effective.example/v1").unwrap(),
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        api_key: None,
        api: ApiId::new(PatchApi::API_ID),
        payload_transforms: Arc::from(Vec::<Arc<dyn ErasedPayloadTransform>>::new()),
        response_observers: Arc::from(Vec::<Arc<dyn ResponseObserver>>::new()),
        attempt_middleware: Arc::from(Vec::<Arc<dyn AttemptMiddleware>>::new()),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(DefaultRetryClassifier::default()),
    }
}

fn local_patch_request(patch: ErasedApiOptionsPatch) -> LocalResolvedApiRequest {
    LocalResolvedApiRequest {
        model: target_model("provider", PatchApi::API_ID, "model", true),
        context: Context::new(None),
        options: SimpleGenerationOptions {
            api_options: Some(patch),
            ..SimpleGenerationOptions::default()
        },
        full_options: None,
        request_options: agentprism_ai::ApiRequestOptions::default(),
        endpoint: Url::parse("https://effective.example/v1").unwrap(),
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        api_key: None,
        api: ApiId::new(PatchApi::API_ID),
        payload_transforms: Rc::from(Vec::<Rc<dyn LocalErasedPayloadTransform>>::new()),
        response_observers: Rc::from(Vec::<Rc<dyn LocalResponseObserver>>::new()),
        attempt_middleware: Rc::from(Vec::<Rc<dyn LocalAttemptMiddleware>>::new()),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Rc::new(LocalDefaultRetryClassifier::default()),
    }
}

fn reasoning_map() -> ThinkingLevelMap<&'static str> {
    ThinkingLevelMap {
        high: Some(LevelSupport::Value("high")),
        xhigh: Some(LevelSupport::Unsupported),
        ..ThinkingLevelMap::default()
    }
}

#[test]
fn reasoning_xhigh_clamps_in_pi_mode() {
    pi_basis!(REASONING_BASIS);
    let resolved = reasoning_map()
        .resolve(ReasoningLevel::Xhigh, ReasoningFallback::Clamp)
        .unwrap();
    assert_eq!(resolved.effective, ReasoningLevel::High);
    assert_eq!(resolved.support, Some(LevelSupport::Value("high")));
    assert!(resolved.clamped);

    let hole_with_max = ThinkingLevelMap {
        xhigh: Some(LevelSupport::Unsupported),
        max: Some(LevelSupport::Value("max")),
        ..ThinkingLevelMap::default()
    }
    .resolve(ReasoningLevel::Xhigh, ReasoningFallback::Clamp)
    .unwrap();
    assert_eq!(hole_with_max.effective, ReasoningLevel::Max);
    assert_eq!(hole_with_max.support, Some(LevelSupport::Value("max")));

    assert_eq!(
        ReasoningLevel::Xhigh
            .resolve_extended(false, true, ReasoningFallback::Clamp)
            .unwrap(),
        ReasoningLevel::Max
    );

    let no_supported_mapping = ThinkingLevelMap {
        off: Some(LevelSupport::<&str>::Unsupported),
        minimal: Some(LevelSupport::Unsupported),
        low: Some(LevelSupport::Unsupported),
        medium: Some(LevelSupport::Unsupported),
        high: Some(LevelSupport::Unsupported),
        xhigh: Some(LevelSupport::Unsupported),
        max: Some(LevelSupport::Unsupported),
    }
    .resolve(ReasoningLevel::Xhigh, ReasoningFallback::Clamp)
    .unwrap();
    assert_eq!(no_supported_mapping.effective, ReasoningLevel::Off);
    assert_eq!(no_supported_mapping.support, Some(LevelSupport::Disabled));
    assert!(no_supported_mapping.clamped);
}

#[test]
fn reasoning_xhigh_rejects_in_strict_mode() {
    pi_basis!(REASONING_BASIS);
    assert_eq!(
        reasoning_map().resolve(ReasoningLevel::Xhigh, ReasoningFallback::Strict),
        Err(LoweringError::UnsupportedReasoningLevel {
            requested: ReasoningLevel::Xhigh,
        })
    );
}

#[test]
fn reasoning_explicit_unsupported_is_not_treated_as_missing() {
    pi_basis!(REASONING_BASIS);
    let missing = ThinkingLevelMap::<&str>::default()
        .resolve(ReasoningLevel::Xhigh, ReasoningFallback::Strict)
        .unwrap();
    assert_eq!(missing.support, None);
    assert!(!missing.clamped);
    assert!(matches!(
        reasoning_map().resolve(ReasoningLevel::Xhigh, ReasoningFallback::Strict),
        Err(LoweringError::UnsupportedReasoningLevel { .. })
    ));
}

#[test]
fn thinking_budget_defaults_match_pi() {
    pi_basis!(SIMPLE_BASIS);
    let budgets = ThinkingBudgets::default();
    assert_eq!(budgets.budget_for(ReasoningLevel::Minimal), Some(1_024));
    assert_eq!(budgets.budget_for(ReasoningLevel::Low), Some(2_048));
    assert_eq!(budgets.budget_for(ReasoningLevel::Medium), Some(8_192));
    assert_eq!(budgets.budget_for(ReasoningLevel::High), Some(16_384));
    assert_eq!(budgets.budget_for(ReasoningLevel::Xhigh), Some(16_384));
}

#[test]
fn thinking_budget_reserves_1024_answer_tokens() {
    pi_basis!(SIMPLE_BASIS);
    let plan = plan_thinking_budget(
        None,
        1_500,
        ReasoningLevel::High,
        &ThinkingBudgets::default(),
    )
    .unwrap();
    assert_eq!(plan.max_output_tokens, 1_500);
    assert_eq!(plan.thinking_budget, 476);

    // Anthropic applies the final answer-room clamp unconditionally, even
    // when the response ceiling is greater than the original thinking budget.
    let boundary = plan_thinking_budget(
        None,
        9_000,
        ReasoningLevel::Medium,
        &ThinkingBudgets::default(),
    )
    .unwrap();
    assert_eq!(boundary.max_output_tokens, 9_000);
    assert_eq!(boundary.thinking_budget, 7_976);
}

#[test]
fn thinking_budget_expands_explicit_answer_cap() {
    pi_basis!(SIMPLE_BASIS);
    let plan = plan_thinking_budget(
        Some(2_000),
        10_000,
        ReasoningLevel::Low,
        &ThinkingBudgets::default(),
    )
    .unwrap();
    assert_eq!(plan.max_output_tokens, 4_048);
    assert_eq!(plan.thinking_budget, 2_048);
}

#[test]
fn thinking_budget_respects_model_max_output() {
    pi_basis!(SIMPLE_BASIS);
    let plan = plan_thinking_budget(
        Some(8_000),
        10_000,
        ReasoningLevel::Medium,
        &ThinkingBudgets::default(),
    )
    .unwrap();
    assert_eq!(plan.max_output_tokens, 10_000);
    assert_eq!(plan.thinking_budget, 8_192);
}

#[test]
fn handoff_null_content_normalized() {
    pi_basis!(HANDOFF_BASIS);
    let message: Message = serde_json::from_value(json!({
        "role": "user",
        "id": "user",
        "content": null,
        "timestamp": 1
    }))
    .unwrap();
    let result = project(
        &context(vec![message]),
        &target_model("target", "api", "model", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert!(matches!(&result.context.messages[0], Message::User(user) if user.content.is_empty()));
}

#[test]
fn handoff_message_local_ids_may_repeat_across_assistants() {
    pi_basis!(HANDOFF_BASIS);
    let make_message = |message_id: &str, text_value: &str, call_id: &str, tool_name: &str| {
        let mut message = assistant(
            message_id,
            "source",
            "api",
            "model",
            vec![
                text("scripted-block-0", text_value),
                tool_call("scripted-tool-block-0", call_id, tool_name),
            ],
            AssistantFinishReason::ToolUse,
        );
        message.replay.items.push(replay_item(
            "scripted-replay-0",
            0,
            ReplayTarget::content_block("scripted-block-0"),
            "api.text-identity",
            ReplayApplicability::ExactProviderApiModel,
        ));
        Message::Assistant(message)
    };
    let result = project(
        &context(vec![
            make_message("assistant-1", "first", "call|first", "first_tool"),
            tool_result("result-1", "call|first", "first_tool"),
            make_message("assistant-2", "second", "call|second", "second_tool"),
            tool_result("result-2", "call|second", "second_tool"),
        ]),
        &target_model("target", "api", "model", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );

    assert_eq!(result.context.messages.len(), 4);
    for (message_index, expected_call_id) in [(0, "call_first"), (2, "call_second")] {
        let Message::Assistant(message) = &result.context.messages[message_index] else {
            panic!("expected assistant");
        };
        assert_eq!(message.content[0].id().as_str(), "scripted-block-0");
        assert_eq!(message.content[1].id().as_str(), "scripted-tool-block-0");
        assert!(matches!(
            &message.content[1],
            ContentBlock::ToolCall { call, .. } if call.id.as_str() == expected_call_id
        ));
    }
    for (message_index, expected_call_id) in [(1, "call_first"), (3, "call_second")] {
        assert!(matches!(
            &result.context.messages[message_index],
            Message::ToolResult(result) if result.tool_call_id.as_str() == expected_call_id
        ));
    }
}

#[test]
fn handoff_nonvision_user_image_replaced() {
    pi_basis!(HANDOFF_BASIS);
    let result = project(
        &context(vec![user("user", vec![image("image")])]),
        &target_model("target", "api", "model", false),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert!(matches!(
        &result.context.messages[0],
        Message::User(UserMessage { content, .. })
            if matches!(&content[0], ContentBlock::Text { text, .. }
                if text == "(image omitted: model does not support images)")
    ));
}

#[test]
fn handoff_nonvision_tool_image_replaced() {
    pi_basis!(HANDOFF_BASIS);
    let message = Message::ToolResult(ToolResultMessage {
        id: MessageId::new("result"),
        tool_call_id: ToolCallId::new("call"),
        tool_name: "tool".to_owned(),
        content: vec![ToolResultContent::Image {
            id: ContentBlockId::new("image"),
            data: "aGVsbG8=".to_owned(),
            mime_type: "image/png".to_owned(),
        }],
        details: None,
        usage: None,
        added_tool_names: Vec::new(),
        is_error: false,
        timestamp: Timestamp::from_unix_millis(1),
    });
    let result = project(
        &context(vec![message]),
        &target_model("target", "api", "model", false),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert!(matches!(
        &result.context.messages[0],
        Message::ToolResult(ToolResultMessage { content, .. })
            if matches!(&content[0], ToolResultContent::Text { text, .. }
                if text == "(tool image omitted: model does not support images)")
    ));
}

#[test]
fn handoff_adjacent_image_placeholders_collapsed() {
    pi_basis!(HANDOFF_BASIS);
    let result = project(
        &context(vec![user(
            "user",
            vec![image("image-1"), image("image-2"), text("text", "after")],
        )]),
        &target_model("target", "api", "model", false),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let Message::User(user) = &result.context.messages[0] else {
        panic!("expected user message");
    };
    assert_eq!(user.content.len(), 2);
    assert_eq!(
        result
            .report
            .changes
            .iter()
            .filter(|change| matches!(change, HandoffChange::ImageReplaced { .. }))
            .count(),
        2
    );
}

#[test]
fn handoff_failed_assistant_omitted() {
    pi_basis!(HANDOFF_BASIS);
    let failed = assistant(
        "assistant",
        "source",
        "api",
        "model",
        vec![text("text", "partial")],
        AssistantFinishReason::Error,
    );
    let result = project(
        &context(vec![
            user("user", vec![text("user-text", "hello")]),
            Message::Assistant(failed),
        ]),
        &target_model("source", "api", "model", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert_eq!(result.context.messages.len(), 1);
    assert!(matches!(
        result.report.changes.as_slice(),
        [HandoffChange::FailedAssistantOmitted {
            reason: AssistantFinishReason::Error,
            ..
        }]
    ));

    // Pi's first pass still normalizes IDs in a failed assistant and rewrites
    // an immediately following result. Its second pass then omits only the
    // failed assistant, without making its call pending or synthesizing a
    // replacement result.
    let failed_with_call = assistant(
        "failed-with-call",
        "source",
        "api",
        "model",
        vec![tool_call("failed-tool", "call|item", "read")],
        AssistantFinishReason::Error,
    );
    let combined = project(
        &context(vec![
            Message::Assistant(failed_with_call),
            tool_result("following-result", "call|item", "read"),
        ]),
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    assert_eq!(combined.context.messages.len(), 1);
    assert!(matches!(
        &combined.context.messages[0],
        Message::ToolResult(result) if result.tool_call_id.as_str() == "call_item"
    ));
    assert!(
        !combined
            .report
            .changes
            .iter()
            .any(|change| matches!(change, HandoffChange::SyntheticToolResultInserted { .. }))
    );
}

#[test]
fn handoff_aborted_assistant_omitted() {
    pi_basis!(HANDOFF_BASIS);
    let aborted = assistant(
        "assistant",
        "source",
        "api",
        "model",
        Vec::new(),
        AssistantFinishReason::Aborted,
    );
    let result = project(
        &context(vec![Message::Assistant(aborted)]),
        &target_model("source", "api", "model", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert!(result.context.messages.is_empty());
}

fn signed_thinking_context(redacted: bool, text_value: &str) -> Context {
    let block_id = ContentBlockId::new("thinking");
    let mut message = assistant(
        "assistant",
        "source",
        "api",
        "model",
        vec![ContentBlock::Thinking {
            id: block_id.clone(),
            text: text_value.to_owned(),
            redacted,
            replay_item: Some(ReplayItemId::new("signature")),
        }],
        AssistantFinishReason::Stop,
    );
    message.replay.items.push(replay_item(
        "signature",
        0,
        ReplayTarget::ContentBlock(block_id),
        "api.thinking-signature",
        ReplayApplicability::ExactProviderApiModel,
    ));
    Context {
        messages: vec![Message::Assistant(message)],
        ..Context::new(None)
    }
}

#[test]
fn handoff_redacted_thinking_retained_exact_model() {
    pi_basis!(HANDOFF_BASIS);
    let result = project(
        &signed_thinking_context(true, "[Reasoning redacted]"),
        &target_model("source", "api", "model", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let Message::Assistant(message) = &result.context.messages[0] else {
        panic!("expected assistant");
    };
    assert!(matches!(
        &message.content[0],
        ContentBlock::Thinking {
            redacted: true,
            replay_item: Some(replay_item),
            ..
        } if replay_item.as_str() == "signature"
    ));
    assert_eq!(message.replay.items.len(), 1);
}

#[test]
fn handoff_redacted_thinking_dropped_cross_model() {
    pi_basis!(HANDOFF_BASIS);
    let result = project(
        &signed_thinking_context(true, "[Reasoning redacted]"),
        &target_model("source", "api", "different", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let Message::Assistant(message) = &result.context.messages[0] else {
        panic!("expected assistant");
    };
    assert!(message.content.is_empty());
    assert!(
        result
            .report
            .changes
            .iter()
            .any(|change| matches!(change, HandoffChange::RedactedThinkingDropped { .. }))
    );
}

#[test]
fn handoff_signed_empty_thinking_retained_exact_model() {
    pi_basis!(HANDOFF_BASIS);
    let message_id = MessageId::new("assistant");
    let block_id = ContentBlockId::new("thinking");
    let replay_item_id = ReplayItemId::new("reasoning-output-0");
    let mut assembler = AssistantAssembler::with_timestamp(Timestamp::from_unix_millis(2));
    for event in [
        AssistantEvent::MessageStarted {
            message_id,
            provider: ProviderId::new("source"),
            api: ApiId::new("openai-responses"),
            model: ModelId::new("model"),
        },
        AssistantEvent::ReplayItemStarted {
            item_id: replay_item_id.clone(),
            ordinal: 0,
            target: ReplayTarget::ProviderOutputItem { output_index: 0 },
            kind: ReplayKind::new("openai.responses.reasoning-item"),
            applicability: ReplayApplicability::ExactProviderApiModel,
        },
        AssistantEvent::ContentBlockStarted {
            block_id: block_id.clone(),
            content_index: 0,
            kind: ContentBlockKind::Thinking,
        },
        AssistantEvent::ReplayData {
            item_id: replay_item_id.clone(),
            operation: ReplayDataOperation::ReplaceJsonBytes(
                br#"{"id":"rs_0","type":"reasoning","encrypted_content":"opaque"}"#.to_vec(),
            ),
        },
        AssistantEvent::ReplayItemFinished {
            item_id: replay_item_id.clone(),
        },
        AssistantEvent::ContentBlockFinished { block_id },
    ] {
        assembler.apply(&event).unwrap();
    }
    let assembled = assembler
        .finish_completed(AssistantFinish {
            reason: AssistantFinishReason::Stop,
            raw_provider_reason: None,
            error: None,
        })
        .unwrap();
    let persisted = serde_json::to_vec(&assembled).unwrap();
    let restored: AssistantMessage = serde_json::from_slice(&persisted).unwrap();
    assert!(matches!(
        &restored.content[0],
        ContentBlock::Thinking {
            text,
            replay_item: Some(item_id),
            ..
        } if text.is_empty() && item_id == &replay_item_id
    ));

    let result = project(
        &context(vec![Message::Assistant(restored)]),
        &target_model("source", "openai-responses", "model", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let Message::Assistant(message) = &result.context.messages[0] else {
        panic!("expected assistant");
    };
    assert_eq!(message.content.len(), 1);
    assert_eq!(message.replay.items.len(), 1);
}

#[test]
fn handoff_visible_thinking_becomes_plain_text_in_pi_mode() {
    pi_basis!(HANDOFF_BASIS);
    let mut source_assistant = assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![ContentBlock::Thinking {
            id: ContentBlockId::new("thinking"),
            text: "reasoning".to_owned(),
            redacted: false,
            replay_item: Some(ReplayItemId::new("provider-output-replay")),
        }],
        AssistantFinishReason::Stop,
    );
    source_assistant.replay.items.push(replay_item(
        "provider-output-replay",
        0,
        ReplayTarget::ProviderOutputItem { output_index: 0 },
        "api.reasoning-item",
        ReplayApplicability::ApiFamily,
    ));
    let source = context(vec![Message::Assistant(source_assistant)]);
    let result = project(
        &source,
        &target_model("source", "api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let Message::Assistant(message) = &result.context.messages[0] else {
        panic!("expected assistant");
    };
    assert!(matches!(&message.content[0], ContentBlock::Text { text, .. } if text == "reasoning"));
    assert!(message.replay.items.is_empty());
    assert!(result.report.changes.iter().any(|change| matches!(
        change,
        HandoffChange::OpaqueReplayDropped { replay_item_id, .. }
            if replay_item_id.as_str() == "provider-output-replay"
    )));
}

#[test]
fn handoff_visible_thinking_becomes_tagged_text_when_configured() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![Message::Assistant(assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![thinking("thinking", "reasoning", false)],
        AssistantFinishReason::Stop,
    ))]);
    let policy = HandoffPolicy {
        thinking_fallback: ThinkingFallback::TaggedText {
            opening: "<thinking>".to_owned(),
            closing: "</thinking>".to_owned(),
        },
        ..HandoffPolicy::default()
    };
    let result = project(
        &source,
        &target_model("source", "api", "new", true),
        &policy,
        &ALL_REPLAY,
    );
    let Message::Assistant(message) = &result.context.messages[0] else {
        panic!("expected assistant");
    };
    assert!(matches!(
        &message.content[0],
        ContentBlock::Text { text, .. } if text == "<thinking>reasoning</thinking>"
    ));
}

#[test]
fn handoff_text_signature_dropped_cross_model() {
    pi_basis!(HANDOFF_BASIS);
    let mut source = assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![text("text", "answer")],
        AssistantFinishReason::Stop,
    );
    source.replay.items.push(replay_item(
        "text-signature",
        0,
        ReplayTarget::content_block("text"),
        "api.text-signature",
        ReplayApplicability::ExactProviderApiModel,
    ));
    let result = project(
        &context(vec![Message::Assistant(source)]),
        &target_model("source", "api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let Message::Assistant(message) = &result.context.messages[0] else {
        panic!("expected assistant");
    };
    assert!(message.replay.items.is_empty());
    assert!(result.report.changes.iter().any(|change| matches!(
        change,
        HandoffChange::OpaqueReplayDropped { replay_item_id, .. }
            if replay_item_id.as_str() == "text-signature"
    )));
}

#[test]
fn handoff_tool_signature_dropped_cross_model() {
    pi_basis!(HANDOFF_BASIS);
    let mut source = assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![tool_call("tool", "call", "read")],
        AssistantFinishReason::ToolUse,
    );
    source.replay.items.push(replay_item(
        "tool-signature",
        0,
        ReplayTarget::tool_call("call"),
        "api.tool-signature",
        ReplayApplicability::ExactProviderApiModel,
    ));
    let result = project(
        &context(vec![
            Message::Assistant(source),
            tool_result("result", "call", "read"),
        ]),
        &target_model("source", "api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert!(
        result
            .report
            .changes
            .iter()
            .any(|change| matches!(change, HandoffChange::ToolSignatureDropped { .. }))
    );
}

#[test]
fn handoff_tool_id_normalized() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![
        Message::Assistant(assistant(
            "assistant",
            "source",
            "api",
            "old",
            vec![tool_call("tool", "call|item", "read")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result", "call|item", "read"),
    ]);
    let result = project(
        &source,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    let Message::Assistant(message) = &result.context.messages[0] else {
        panic!("expected assistant");
    };
    assert!(
        matches!(&message.content[0], ContentBlock::ToolCall { call, .. } if call.id.as_str() == "call_item")
    );
}

#[test]
fn handoff_matching_tool_result_id_rewritten() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![
        Message::Assistant(assistant(
            "assistant",
            "source",
            "api",
            "old",
            vec![tool_call("tool", "call|item", "read")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result", "call|item", "read"),
    ]);
    let result = project(
        &source,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    assert!(matches!(
        &result.context.messages[1],
        Message::ToolResult(result) if result.tool_call_id.as_str() == "call_item"
    ));

    // Pinned Pi does not invoke the target normalizer for an exact-model
    // assistant, even when that normalizer would change the identifier.
    let same_model = context(vec![
        Message::Assistant(assistant(
            "same-model-assistant",
            "target",
            "other-api",
            "new",
            vec![tool_call("same-model-tool", "same|model", "read")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("same-model-result", "same|model", "read"),
    ]);
    let same_model = project(
        &same_model,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    let Message::Assistant(same_model_assistant) = &same_model.context.messages[0] else {
        panic!("expected assistant");
    };
    assert!(matches!(
        &same_model_assistant.content[0],
        ContentBlock::ToolCall { call, .. } if call.id.as_str() == "same|model"
    ));
    assert!(matches!(
        &same_model.context.messages[1],
        Message::ToolResult(result) if result.tool_call_id.as_str() == "same|model"
    ));
    assert!(
        !same_model
            .report
            .changes
            .iter()
            .any(|change| matches!(change, HandoffChange::ToolCallIdRewritten { .. }))
    );

    // Pinned Pi retains one ID map for the entire ordered first pass. A tool
    // result is therefore still rewritten after a user interruption, even
    // though orphan closure separately inserts a synthetic result at the user
    // boundary.
    let after_user = context(vec![
        Message::Assistant(assistant(
            "assistant-before-user",
            "source",
            "api",
            "old",
            vec![tool_call("tool-before-user", "call|after-user", "read")],
            AssistantFinishReason::ToolUse,
        )),
        user("intervening-user", vec![text("user-text", "continue")]),
        tool_result("result-after-user", "call|after-user", "read"),
    ]);
    let after_user = project(
        &after_user,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    assert!(after_user.context.messages.iter().any(|message| matches!(
        message,
        Message::ToolResult(result)
            if result.id.as_str() == "result-after-user"
                && result.tool_call_id.as_str() == "call_after-user"
    )));

    // An assistant boundary also leaves the pass-wide map intact when that
    // assistant does not introduce a newer occurrence of the original ID.
    let after_assistant = context(vec![
        Message::Assistant(assistant(
            "assistant-before-assistant",
            "source",
            "api",
            "old",
            vec![tool_call(
                "tool-before-assistant",
                "call|after-assistant",
                "read",
            )],
            AssistantFinishReason::ToolUse,
        )),
        Message::Assistant(assistant(
            "intervening-assistant",
            "source",
            "api",
            "old",
            vec![text("assistant-text", "continuing")],
            AssistantFinishReason::Stop,
        )),
        tool_result("result-after-assistant", "call|after-assistant", "read"),
    ]);
    let after_assistant = project(
        &after_assistant,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    assert!(
        after_assistant
            .context
            .messages
            .iter()
            .any(|message| matches!(
                message,
                Message::ToolResult(result)
                    if result.id.as_str() == "result-after-assistant"
                        && result.tool_call_id.as_str() == "call_after-assistant"
            ))
    );

    // The same original ID can appear under different source fingerprints.
    // Each result must retain the mapping active at its transcript position;
    // a later assistant must not retroactively rewrite the earlier result.
    let repeated = context(vec![
        Message::Assistant(assistant(
            "assistant-a",
            "source",
            "api-a",
            "model-a",
            vec![tool_call("tool-a", "shared", "read")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result-a", "shared", "read"),
        Message::Assistant(assistant(
            "assistant-b",
            "source",
            "api-b",
            "model-b",
            vec![tool_call("tool-b", "shared", "write")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result-b", "shared", "write"),
    ]);
    let repeated = project(
        &repeated,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SourceSensitiveIds,
    );
    assert!(matches!(
        &repeated.context.messages[1],
        Message::ToolResult(result) if result.tool_call_id.as_str() == "model-a__shared"
    ));
    assert!(matches!(
        &repeated.context.messages[3],
        Message::ToolResult(result) if result.tool_call_id.as_str() == "model-b__shared"
    ));

    // A newer occurrence overwrites the pass-wide mapping used by a delayed
    // result for the same original ID.
    let overwritten = context(vec![
        Message::Assistant(assistant(
            "assistant-old-mapping",
            "source",
            "api-a",
            "model-a",
            vec![tool_call("tool-old-mapping", "shared", "read")],
            AssistantFinishReason::ToolUse,
        )),
        Message::Assistant(assistant(
            "assistant-new-mapping",
            "source",
            "api-b",
            "model-b",
            vec![tool_call("tool-new-mapping", "shared", "write")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result-new-mapping", "shared", "write"),
    ]);
    let overwritten = project(
        &overwritten,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SourceSensitiveIds,
    );
    assert!(overwritten.context.messages.iter().any(|message| matches!(
        message,
        Message::ToolResult(result)
            if result.id.as_str() == "result-new-mapping"
                && result.tool_call_id.as_str() == "model-b__shared"
    )));

    // A later cross-model occurrence whose normalizer returns the original ID
    // does not erase a changed mapping established earlier in the pass.
    let after_noop = context(vec![
        Message::Assistant(assistant(
            "assistant-changed",
            "source",
            "api",
            "changed",
            vec![tool_call("tool-changed", "shared", "read")],
            AssistantFinishReason::ToolUse,
        )),
        Message::Assistant(assistant(
            "assistant-noop",
            "source",
            "api",
            "noop",
            vec![tool_call("tool-noop", "shared", "write")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result-after-noop", "shared", "write"),
    ]);
    let after_noop = project(
        &after_noop,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SelectiveIds,
    );
    assert!(after_noop.context.messages.iter().any(|message| matches!(
        message,
        Message::ToolResult(result)
            if result.id.as_str() == "result-after-noop"
                && result.tool_call_id.as_str() == "changed__shared"
    )));

    // A later exact-model occurrence likewise leaves the earlier changed
    // mapping intact and is not itself normalized.
    let after_same_model = context(vec![
        Message::Assistant(assistant(
            "assistant-before-same-model",
            "source",
            "api",
            "old",
            vec![tool_call("tool-before-same-model", "shared", "read")],
            AssistantFinishReason::ToolUse,
        )),
        Message::Assistant(assistant(
            "assistant-same-model",
            "target",
            "other-api",
            "new",
            vec![tool_call("tool-same-model", "shared", "write")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result-after-same-model", "shared", "write"),
    ]);
    let after_same_model = project(
        &after_same_model,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SourceSensitiveIds,
    );
    let same_model_call = after_same_model
        .context
        .messages
        .iter()
        .find_map(|message| match message {
            Message::Assistant(assistant) if assistant.id.as_str() == "assistant-same-model" => {
                assistant.content.iter().find_map(|block| match block {
                    ContentBlock::ToolCall { call, .. } => Some(&call.id),
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("same-model tool call");
    assert_eq!(same_model_call.as_str(), "shared");
    assert!(
        after_same_model
            .context
            .messages
            .iter()
            .any(|message| matches!(
                message,
                Message::ToolResult(result)
                    if result.id.as_str() == "result-after-same-model"
                        && result.tool_call_id.as_str() == "old__shared"
            ))
    );

    // Repeating the same original ID with the same normalization output is an
    // overwrite, not a collision requiring a hash suffix.
    let repeated_same_mapping = context(vec![
        Message::Assistant(assistant(
            "assistant-same-mapping-a",
            "source",
            "api",
            "old",
            vec![tool_call("tool-same-mapping-a", "same|id", "read")],
            AssistantFinishReason::ToolUse,
        )),
        Message::Assistant(assistant(
            "assistant-same-mapping-b",
            "source",
            "api",
            "old",
            vec![tool_call("tool-same-mapping-b", "same|id", "write")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result-same-mapping", "same|id", "write"),
    ]);
    let repeated_same_mapping = project(
        &repeated_same_mapping,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    assert!(
        repeated_same_mapping
            .context
            .messages
            .iter()
            .any(|message| matches!(
                message,
                Message::ToolResult(result)
                    if result.id.as_str() == "result-same-mapping"
                        && result.tool_call_id.as_str() == "same_id"
            ))
    );
}

#[test]
fn handoff_tool_id_collision_gets_stable_hash() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![
        Message::Assistant(assistant(
            "assistant",
            "source",
            "api",
            "old",
            vec![
                tool_call("tool-1", "same|id", "read"),
                tool_call("tool-2", "same/id", "write"),
            ],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result-1", "same|id", "read"),
        tool_result("result-2", "same/id", "write"),
    ]);
    let target = target_model("target", "other-api", "new", true);
    let first = project(&source, &target, &HandoffPolicy::default(), &SANITIZE_IDS);
    let second = project(&source, &target, &HandoffPolicy::default(), &SANITIZE_IDS);
    let calls = |result: &HandoffResult| {
        let Message::Assistant(message) = &result.context.messages[0] else {
            panic!("expected assistant");
        };
        message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { call, .. } => Some(call.id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(calls(&first), calls(&second));
    assert_ne!(calls(&first)[0], calls(&first)[1]);

    // Every retained final ID reserves collision space, even when it did not
    // produce a Pi-style old-to-new result mapping. Cover both kinds skipped
    // by normalization: a cross-model no-op and a same-model call.
    let retained_id_collisions = context(vec![
        Message::Assistant(assistant(
            "cross-model-noop",
            "source",
            "api",
            "old",
            vec![tool_call(
                "cross-model-noop-tool",
                "cross_model_noop",
                "read",
            )],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("cross-model-noop-result", "cross_model_noop", "read"),
        Message::Assistant(assistant(
            "same-model",
            "target",
            "other-api",
            "new",
            vec![tool_call("same-model-tool", "same_model_id", "write")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("same-model-result", "same_model_id", "write"),
        Message::Assistant(assistant(
            "later-cross-model",
            "source",
            "api",
            "old",
            vec![
                tool_call(
                    "later-cross-model-noop-collision",
                    "cross|model|noop",
                    "read",
                ),
                tool_call("later-same-model-collision", "same|model|id", "write"),
            ],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("later-cross-model-noop-result", "cross|model|noop", "read"),
        tool_result("later-same-model-result", "same|model|id", "write"),
    ]);
    let retained_first = project(
        &retained_id_collisions,
        &target,
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    let retained_second = project(
        &retained_id_collisions,
        &target,
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    let retained_calls = |result: &HandoffResult| {
        result
            .context
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Assistant(assistant) => Some(&assistant.content),
                Message::User(_) | Message::ToolResult(_) => None,
            })
            .flatten()
            .filter_map(|block| match block {
                ContentBlock::ToolCall { call, .. } => Some(call.id.clone()),
                ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Thinking { .. } => None,
            })
            .collect::<Vec<_>>()
    };
    let retained_first_calls = retained_calls(&retained_first);
    assert_eq!(retained_first_calls, retained_calls(&retained_second));
    assert_eq!(retained_first_calls[0].as_str(), "cross_model_noop");
    assert_eq!(retained_first_calls[1].as_str(), "same_model_id");
    assert_ne!(retained_first_calls[2], retained_first_calls[0]);
    assert_ne!(retained_first_calls[3], retained_first_calls[1]);
    assert_eq!(
        retained_first_calls.iter().collect::<BTreeSet<_>>().len(),
        retained_first_calls.len()
    );
    let result_call_id = |message_id: &str| {
        retained_first
            .context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.id.as_str() == message_id => {
                    Some(result.tool_call_id.clone())
                }
                Message::User(_) | Message::Assistant(_) | Message::ToolResult(_) => None,
            })
            .expect("named tool result")
    };
    assert_eq!(
        result_call_id("cross-model-noop-result").as_str(),
        "cross_model_noop"
    );
    assert_eq!(
        result_call_id("same-model-result").as_str(),
        "same_model_id"
    );
    assert_eq!(
        result_call_id("later-cross-model-noop-result"),
        retained_first_calls[2]
    );
    assert_eq!(
        result_call_id("later-same-model-result"),
        retained_first_calls[3]
    );

    // Reservation must be independent of transcript order. A changed call
    // normalized before a later retained no-op or same-model call cannot
    // claim that later call's final ID.
    let reverse_order_collisions = context(vec![
        Message::Assistant(assistant(
            "earlier-cross-model",
            "source",
            "api",
            "old",
            vec![
                tool_call(
                    "earlier-cross-model-noop-collision",
                    "cross|model|noop",
                    "read",
                ),
                tool_call("earlier-same-model-collision", "same|model|id", "write"),
            ],
            AssistantFinishReason::ToolUse,
        )),
        tool_result(
            "earlier-cross-model-noop-result",
            "cross|model|noop",
            "read",
        ),
        tool_result("earlier-same-model-result", "same|model|id", "write"),
        Message::Assistant(assistant(
            "later-cross-model-noop",
            "source",
            "api",
            "old",
            vec![tool_call(
                "later-cross-model-noop-tool",
                "cross_model_noop",
                "read",
            )],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("later-cross-model-noop-result", "cross_model_noop", "read"),
        Message::Assistant(assistant(
            "later-same-model",
            "target",
            "other-api",
            "new",
            vec![tool_call("later-same-model-tool", "same_model_id", "write")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("later-same-model-result", "same_model_id", "write"),
    ]);
    let reverse_first = project(
        &reverse_order_collisions,
        &target,
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    let reverse_second = project(
        &reverse_order_collisions,
        &target,
        &HandoffPolicy::default(),
        &SANITIZE_IDS,
    );
    let reverse_first_calls = retained_calls(&reverse_first);
    assert_eq!(reverse_first_calls, retained_calls(&reverse_second));
    assert_ne!(reverse_first_calls[0].as_str(), "cross_model_noop");
    assert_ne!(reverse_first_calls[1].as_str(), "same_model_id");
    assert_eq!(reverse_first_calls[2].as_str(), "cross_model_noop");
    assert_eq!(reverse_first_calls[3].as_str(), "same_model_id");
    assert_eq!(
        reverse_first_calls.iter().collect::<BTreeSet<_>>().len(),
        reverse_first_calls.len()
    );
    let reverse_result_call_id = |message_id: &str| {
        reverse_first
            .context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::ToolResult(result) if result.id.as_str() == message_id => {
                    Some(result.tool_call_id.clone())
                }
                Message::User(_) | Message::Assistant(_) | Message::ToolResult(_) => None,
            })
            .expect("named reverse-order tool result")
    };
    assert_eq!(
        reverse_result_call_id("earlier-cross-model-noop-result"),
        reverse_first_calls[0]
    );
    assert_eq!(
        reverse_result_call_id("earlier-same-model-result"),
        reverse_first_calls[1]
    );
    assert_eq!(
        reverse_result_call_id("later-cross-model-noop-result"),
        reverse_first_calls[2]
    );
    assert_eq!(
        reverse_result_call_id("later-same-model-result"),
        reverse_first_calls[3]
    );
}

#[test]
fn handoff_missing_tool_result_synthesized() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![Message::Assistant(assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![tool_call("tool", "call", "read")],
        AssistantFinishReason::ToolUse,
    ))]);
    let result = project(
        &source,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert!(matches!(
        &result.context.messages[1],
        Message::ToolResult(result)
            if result.tool_call_id.as_str() == "call"
                && result.is_error
                && matches!(&result.content[0], ToolResultContent::Text { text, .. }
                    if text == "No result provided")
    ));

    // Pinned Pi closes a prior successful assistant's pending call before it
    // skips a later failed assistant that interrupts the tool-result sequence.
    let interrupted = context(vec![
        Message::Assistant(assistant(
            "prior-assistant",
            "source",
            "api",
            "old",
            vec![tool_call("prior-tool", "prior-call", "read")],
            AssistantFinishReason::ToolUse,
        )),
        Message::Assistant(assistant(
            "failed-assistant",
            "source",
            "api",
            "old",
            vec![text("failed-text", "partial")],
            AssistantFinishReason::Error,
        )),
    ]);
    let interrupted_result = project(
        &interrupted,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert_eq!(interrupted_result.context.messages.len(), 2);
    assert!(matches!(
        &interrupted_result.context.messages[1],
        Message::ToolResult(result)
            if result.tool_call_id.as_str() == "prior-call"
                && result.is_error
                && matches!(&result.content[0], ToolResultContent::Text { text, .. }
                    if text == "No result provided")
    ));
    assert!(
        interrupted_result
            .report
            .changes
            .iter()
            .any(|change| matches!(
                change,
                HandoffChange::FailedAssistantOmitted { message_id, .. }
                    if message_id.as_str() == "failed-assistant"
            ))
    );
}

#[test]
fn handoff_existing_tool_result_not_duplicated() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![
        Message::Assistant(assistant(
            "assistant",
            "source",
            "api",
            "old",
            vec![tool_call("tool", "call", "read")],
            AssistantFinishReason::ToolUse,
        )),
        tool_result("result", "call", "read"),
    ]);
    let result = project(
        &source,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    assert_eq!(result.context.messages.len(), 2);
    assert!(
        !result
            .report
            .changes
            .iter()
            .any(|change| matches!(change, HandoffChange::SyntheticToolResultInserted { .. }))
    );
}

#[test]
fn handoff_multiple_missing_results_preserve_source_order() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![Message::Assistant(assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![
            tool_call("tool-1", "call-1", "first"),
            tool_call("tool-2", "call-2", "second"),
        ],
        AssistantFinishReason::ToolUse,
    ))]);
    let result = project(
        &source,
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let ids = result.context.messages[1..]
        .iter()
        .map(|message| match message {
            Message::ToolResult(result) => result.tool_call_id.as_str(),
            _ => panic!("expected tool result"),
        })
        .collect::<Vec<_>>();
    assert_eq!(ids, ["call-1", "call-2"]);
}

#[test]
fn handoff_loss_report_contains_every_drop() {
    pi_basis!(HANDOFF_BASIS);
    let mut source = assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![text("text", "answer")],
        AssistantFinishReason::Stop,
    );
    source.replay.items.extend([
        replay_item(
            "replay-1",
            0,
            ReplayTarget::content_block("text"),
            "api.first",
            ReplayApplicability::ExactProviderApiModel,
        ),
        replay_item(
            "replay-2",
            1,
            ReplayTarget::Message,
            "api.second",
            ReplayApplicability::ExactProviderApiModel,
        ),
    ]);
    let result = project(
        &context(vec![Message::Assistant(source)]),
        &target_model("target", "other-api", "new", true),
        &HandoffPolicy::default(),
        &ALL_REPLAY,
    );
    let dropped = result
        .report
        .changes
        .iter()
        .filter_map(|change| match change {
            HandoffChange::OpaqueReplayDropped { replay_item_id, .. } => {
                Some(replay_item_id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(dropped, BTreeSet::from(["replay-1", "replay-2"]));
    assert!(result.report.lossy);

    let mut failed = assistant(
        "failed",
        "source",
        "api",
        "model",
        vec![text("failed-text", "partial")],
        AssistantFinishReason::Error,
    );
    failed.replay.items.extend([
        replay_item(
            "failed-replay-1",
            0,
            ReplayTarget::content_block("failed-text"),
            "api.failed-first",
            ReplayApplicability::ExactProviderApiModel,
        ),
        replay_item(
            "failed-replay-2",
            1,
            ReplayTarget::Message,
            "api.failed-second",
            ReplayApplicability::ExactProviderApiModel,
        ),
    ]);
    let display_policy = HandoffPolicy {
        failed_turn_policy: FailedTurnProjection::IncludeDisplayTextOnly,
        ..HandoffPolicy::default()
    };
    let display_result = project(
        &context(vec![Message::Assistant(failed)]),
        &target_model("source", "api", "model", true),
        &display_policy,
        &ALL_REPLAY,
    );
    let failed_drops = display_result
        .report
        .changes
        .iter()
        .filter_map(|change| match change {
            HandoffChange::OpaqueReplayDropped { replay_item_id, .. } => {
                Some(replay_item_id.as_str())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(
        failed_drops,
        BTreeSet::from(["failed-replay-1", "failed-replay-2"])
    );
    assert!(display_result.report.lossy);
}

#[test]
fn handoff_strict_mode_rejects_lossy_projection() {
    pi_basis!(HANDOFF_BASIS);
    let source = context(vec![Message::Assistant(assistant(
        "assistant",
        "source",
        "api",
        "old",
        vec![thinking("thinking", "reasoning", false)],
        AssistantFinishReason::Stop,
    ))]);
    let policy = HandoffPolicy {
        loss_policy: HandoffLossPolicy::RejectLossy,
        ..HandoffPolicy::default()
    };
    let error = transform_context_for_model(
        &source,
        &target_model("target", "other-api", "new", true),
        &policy,
        &ALL_REPLAY,
    )
    .unwrap_err();
    assert!(matches!(error, HandoffError::LossyProjection { .. }));

    let mut failed = assistant(
        "failed",
        "source",
        "api",
        "model",
        vec![text("failed-text", "partial")],
        AssistantFinishReason::Error,
    );
    failed.replay.items.push(replay_item(
        "failed-replay",
        0,
        ReplayTarget::Message,
        "api.failed",
        ReplayApplicability::ExactProviderApiModel,
    ));
    let failed_display_policy = HandoffPolicy {
        loss_policy: HandoffLossPolicy::RejectLossy,
        failed_turn_policy: FailedTurnProjection::IncludeDisplayTextOnly,
        ..HandoffPolicy::default()
    };
    let failed_error = transform_context_for_model(
        &context(vec![Message::Assistant(failed)]),
        &target_model("source", "api", "model", true),
        &failed_display_policy,
        &ALL_REPLAY,
    )
    .unwrap_err();
    assert!(matches!(failed_error, HandoffError::LossyProjection { .. }));
}
