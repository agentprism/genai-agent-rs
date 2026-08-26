use agentprism_ai::{
    ApiRequestOptions, AssistantEvent, AssistantFinishReason, AssistantMessage, CancellationToken,
    ChatApi, ContentBlock, ContentBlockId, Context, DefaultRetryClassifier, HttpRequest,
    HttpResponse, HttpTransport, Message, MessageId, ModelDescriptor, ResolvedApiRequest,
    RetryPolicy, SecretString, SendBoxFuture, SimpleGenerationOptions, Timestamp, TransportError,
    UserMessage,
};
use agentprism_bedrock::{
    BedrockSigner, BedrockSignerError, BedrockSignerResponse, BedrockSigningConfig,
    bedrock_converse_stream_api, bedrock_models,
};
use agentprism_providers_all::remaining_provider_models;
use futures_executor::block_on;
use futures_util::{StreamExt, stream};
use http::HeaderMap;
use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../fixtures");

#[derive(Clone)]
enum ResponseScript {
    Complete(Vec<u8>),
    Pending(Vec<u8>),
}

#[derive(Default)]
struct AbortTransport {
    calls: AtomicUsize,
    responses: Mutex<VecDeque<ResponseScript>>,
}

impl AbortTransport {
    fn new(complete: Vec<u8>, partial: Vec<u8>) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            responses: Mutex::new(VecDeque::from([
                ResponseScript::Complete(complete.clone()),
                ResponseScript::Pending(partial),
                ResponseScript::Complete(complete),
            ])),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn response(&self) -> HttpResponse {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let script = self
            .responses
            .lock()
            .expect("response queue lock")
            .pop_front()
            .expect("fixture response queue");
        match script {
            ResponseScript::Complete(bytes) => {
                HttpResponse::from_bytes(200, HeaderMap::new(), bytes)
            }
            ResponseScript::Pending(bytes) => HttpResponse {
                status: 200,
                headers: HeaderMap::new(),
                diagnostics: Vec::new(),
                notify_observers: true,
                decode_non_success: false,
                body: Box::pin(
                    stream::once(async move { Ok(bytes) })
                        .chain(stream::pending::<Result<Vec<u8>, TransportError>>()),
                ),
            },
        }
    }
}

impl HttpTransport for AbortTransport {
    fn execute(
        &self,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, TransportError>> {
        let response = self.response();
        Box::pin(async move { Ok(response) })
    }
}

impl BedrockSigner for AbortTransport {
    fn execute(
        &self,
        _config: BedrockSigningConfig,
        _request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<BedrockSignerResponse, BedrockSignerError>> {
        let response = BedrockSignerResponse::from(self.response());
        Box::pin(async move { Ok(response) })
    }
}

fn fixture(family: &str) -> Vec<u8> {
    fs::read(format!(
        "{FIXTURE_ROOT}/{family}/text-only/response-turn-1.sse"
    ))
    .expect("captured provider response fixture")
}

fn partial_sse(family: &str, complete: &[u8]) -> Vec<u8> {
    let source = String::from_utf8(complete.to_vec()).expect("SSE fixture is UTF-8");
    match family {
        "google-generative-ai" | "google-vertex" => source
            .replace(",\"finishReason\":\"STOP\"", "")
            .into_bytes(),
        "mistral-conversations" => {
            let partial = source.replace("\"finish_reason\":\"stop\"", "\"finish_reason\":null");
            format!(
                "{}\n\n",
                partial
                    .split("\n\n")
                    .next()
                    .expect("first Mistral SSE record")
            )
            .into_bytes()
        }
        _ => {
            let marker = match family {
                "anthropic-messages" => "\"type\":\"text_delta\"",
                "openai-completions" => "\"content\":\"fixture response turn 1\"",
                "openai-responses" | "azure-openai-responses" | "openai-codex-responses" => {
                    "\"type\":\"response.output_text.delta\""
                }
                other => panic!("missing partial SSE marker for {other}"),
            };
            let marker_index = source.find(marker).expect("fixture content marker");
            let record_end = source[marker_index..]
                .find("\n\n")
                .map(|offset| marker_index + offset + 2)
                .unwrap_or(source.len());
            source.as_bytes()[..record_end].to_vec()
        }
    }
}

fn partial_bedrock(complete: &[u8]) -> Vec<u8> {
    let first = u32::from_be_bytes(complete[..4].try_into().expect("first frame length")) as usize;
    let second = u32::from_be_bytes(
        complete[first..first + 4]
            .try_into()
            .expect("second frame length"),
    ) as usize;
    complete[..first + second].to_vec()
}

fn first_model(models: Vec<ModelDescriptor>, api: &str) -> ModelDescriptor {
    models
        .into_iter()
        .find(|model| model.api.api_id().as_str() == api)
        .unwrap_or_else(|| panic!("catalog has no model for {api}"))
}

fn user(id: &str, text: &str) -> Message {
    Message::User(UserMessage {
        id: MessageId::new(id),
        content: vec![ContentBlock::Text {
            id: ContentBlockId::new(format!("{id}-text")),
            text: text.to_owned(),
        }],
        timestamp: Timestamp::from_unix_millis(1_700_000_000_000),
    })
}

fn context() -> Context {
    let mut context = Context::new(None);
    context.messages.push(user("user-1", "hello"));
    context
}

fn request(model: ModelDescriptor, context: Context) -> ResolvedApiRequest {
    let api = model.api.api_id();
    let mut endpoint = model.common.base_url.clone();
    if api.as_str() == "google-vertex" {
        endpoint.set_path("/v1/projects/fixture-project/locations/us-central1");
    }
    ResolvedApiRequest {
        model,
        context,
        options: SimpleGenerationOptions::default(),
        full_options: None,
        request_options: ApiRequestOptions::default(),
        endpoint,
        headers: HeaderMap::new(),
        auth_headers: HeaderMap::new(),
        api_key: Some(SecretString::new("fixture-key")),
        api,
        payload_transforms: Arc::from([]),
        response_observers: Arc::from([]),
        attempt_middleware: Arc::from([]),
        retry_policy: RetryPolicy::default(),
        timeout: None,
        retry_classifier: Arc::new(DefaultRetryClassifier::default()),
    }
}

fn terminal(events: &[AssistantEvent]) -> &AssistantMessage {
    events
        .last()
        .and_then(AssistantEvent::terminal_message)
        .expect("terminal assistant message")
}

fn visible_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } | ContentBlock::Thinking { text, .. } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}

fn collect(
    api: &dyn ChatApi,
    request: ResolvedApiRequest,
    token: CancellationToken,
) -> Vec<AssistantEvent> {
    block_on(async {
        api.stream(request, token)
            .await
            .expect("adapter establishes stream")
            .collect()
            .await
    })
}

fn assert_abort_matrix(
    label: &str,
    api: Arc<dyn ChatApi>,
    model: ModelDescriptor,
    transport: &AbortTransport,
) {
    let immediate = CancellationToken::new();
    immediate.cancel();
    let events = collect(api.as_ref(), request(model.clone(), context()), immediate);
    let aborted = terminal(&events).clone();
    assert_eq!(
        aborted.finish.reason,
        AssistantFinishReason::Aborted,
        "{label}: immediate cancellation"
    );
    assert!(
        aborted.content.is_empty(),
        "{label}: immediate abort remains empty"
    );
    assert_eq!(
        transport.calls(),
        0,
        "{label}: pre-cancelled request must not touch transport"
    );

    let mut followup_context = context();
    followup_context.messages.push(Message::Assistant(aborted));
    followup_context
        .messages
        .push(user("user-after-immediate", "continue"));
    let events = collect(
        api.as_ref(),
        request(model.clone(), followup_context),
        CancellationToken::new(),
    );
    assert_eq!(
        terminal(&events).finish.reason,
        AssistantFinishReason::Stop,
        "{label}: follow-up after immediate cancellation"
    );
    assert!(!visible_text(terminal(&events)).is_empty());

    let midstream = CancellationToken::new();
    let (events, partial) = block_on(async {
        let mut stream = api
            .stream(request(model.clone(), context()), midstream.clone())
            .await
            .expect("adapter establishes midstream fixture");
        let mut events = Vec::new();
        loop {
            let event = stream.next().await.expect("content before pending body");
            let has_content = matches!(
                event,
                AssistantEvent::TextDelta { .. } | AssistantEvent::ThinkingDelta { .. }
            );
            events.push(event);
            if has_content {
                midstream.cancel();
                break;
            }
            assert!(
                !events.last().is_some_and(AssistantEvent::is_terminal),
                "{label}: fixture terminated before cancellation"
            );
        }
        events.extend(stream.collect::<Vec<_>>().await);
        let partial = terminal(&events).clone();
        (events, partial)
    });
    assert_eq!(
        terminal(&events).finish.reason,
        AssistantFinishReason::Aborted,
        "{label}: midstream cancellation"
    );
    assert!(
        !visible_text(&partial).is_empty(),
        "{label}: partial content survives cancellation"
    );

    let mut followup_context = context();
    followup_context.messages.push(Message::Assistant(partial));
    followup_context
        .messages
        .push(user("user-after-midstream", "continue again"));
    let events = collect(
        api.as_ref(),
        request(model, followup_context),
        CancellationToken::new(),
    );
    assert_eq!(
        terminal(&events).finish.reason,
        AssistantFinishReason::Stop,
        "{label}: follow-up after midstream cancellation"
    );
    assert!(!visible_text(terminal(&events)).is_empty());
    assert_eq!(transport.calls(), 3, "{label}: exact transport call count");
}

fn run_http_case(
    family: &str,
    model: ModelDescriptor,
    build_api: impl FnOnce(Arc<dyn HttpTransport>) -> Arc<dyn ChatApi>,
) {
    let complete = fixture(family);
    let partial = partial_sse(family, &complete);
    let transport = Arc::new(AbortTransport::new(complete, partial));
    let api = build_api(transport.clone());
    assert_abort_matrix(family, api, model, transport.as_ref());
}

/// Architecture v2 part 2 §1.3, §2.1, §9.5, and §10.1; pinned Pi basis:
/// `packages/ai/test/abort.test.ts`. Each case executes the real family
/// handler, transport decorator, incremental decoder, and terminal assembler.
#[test]
fn provider_api_family_abort_immediate_midstream_and_followup_pi_exact() {
    run_http_case(
        "anthropic-messages",
        first_model(
            agentprism_anthropic::anthropic_models().expect("Anthropic catalog"),
            "anthropic-messages",
        ),
        agentprism_anthropic::anthropic_messages_api,
    );
    run_http_case(
        "openai-completions",
        first_model(
            remaining_provider_models("together").expect("Together catalog"),
            "openai-completions",
        ),
        agentprism_openai::openai_completions_api,
    );
    run_http_case(
        "openai-responses",
        first_model(
            agentprism_openai::openai_models().expect("OpenAI catalog"),
            "openai-responses",
        ),
        agentprism_openai::openai_responses_api,
    );
    run_http_case(
        "azure-openai-responses",
        first_model(
            agentprism_azure_openai_responses::models().expect("Azure OpenAI catalog"),
            "azure-openai-responses",
        ),
        agentprism_openai::azure_openai_responses_api,
    );
    run_http_case(
        "openai-codex-responses",
        first_model(
            agentprism_openai_codex::models().expect("OpenAI Codex catalog"),
            "openai-codex-responses",
        ),
        agentprism_openai::openai_codex_responses_api,
    );
    run_http_case(
        "google-generative-ai",
        first_model(
            agentprism_google::google_models().expect("Google catalog"),
            "google-generative-ai",
        ),
        agentprism_google::google_generative_ai_api,
    );
    run_http_case(
        "google-vertex",
        first_model(
            agentprism_google_vertex::models().expect("Google Vertex catalog"),
            "google-vertex",
        ),
        agentprism_google::google_vertex_api,
    );
    run_http_case(
        "mistral-conversations",
        first_model(
            agentprism_mistral::mistral_models().expect("Mistral catalog"),
            "mistral-conversations",
        ),
        agentprism_mistral::mistral_conversations_api,
    );
}

/// Architecture v2 part 2 §3.1 and §10.1; pinned Pi basis:
/// `packages/ai/test/abort.test.ts` provider matrix. Every provider-owned
/// descriptor traverses the concrete API family selected by its catalog.
#[test]
fn provider_catalog_abort_matrix_pi_exact() {
    for provider in [
        "baseten",
        "together",
        "xiaomi",
        "xiaomi-token-plan-ams",
        "xiaomi-token-plan-cn",
        "xiaomi-token-plan-sgp",
        "qwen-token-plan",
        "qwen-token-plan-cn",
        "qwen-token-plan-individual",
    ] {
        let model = first_model(
            remaining_provider_models(provider).expect("provider catalog"),
            "openai-completions",
        );
        run_http_case(
            "openai-completions",
            model,
            agentprism_openai::openai_completions_api,
        );
    }

    for provider in ["kimi-coding", "minimax", "vercel-ai-gateway"] {
        let model = first_model(
            remaining_provider_models(provider).expect("provider catalog"),
            "anthropic-messages",
        );
        run_http_case(
            "anthropic-messages",
            model,
            agentprism_anthropic::anthropic_messages_api,
        );
    }
}

/// Architecture v2 part 2 §1.3, §2.1, §9.5, and §10.1; pinned Pi basis:
/// `packages/ai/test/abort.test.ts`, including Bedrock's explicit immediate
/// abort followed by a new message.
#[test]
fn bedrock_family_adapter_abort_immediate_midstream_and_followup_pi_exact() {
    let complete = fixture("bedrock-converse-stream");
    let partial = partial_bedrock(&complete);
    let transport = Arc::new(AbortTransport::new(complete, partial));
    let api = bedrock_converse_stream_api(transport.clone());
    let model = first_model(bedrock_models(), "bedrock-converse-stream");
    assert_abort_matrix("bedrock-converse-stream", api, model, transport.as_ref());
}
