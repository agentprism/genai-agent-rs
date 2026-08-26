//! Erased API handlers, transport decorators, authentication, and registration.

#![allow(
    clippy::result_large_err,
    reason = "ErasedApiHandler requires the architecture-specified AiError by value"
)]

use crate::{AnthropicMessagesDecodeContext, AnthropicMessagesSseDecoder, anthropic_models};
use agentprism_ai::{
    AiError, AiErrorKind, AnthropicMessages, AnthropicMessagesHandoff, AnthropicOptions,
    AnthropicSimplePatch, ApiExecutionContext, ApiFamily, ApiId, ApiModelConfig, ApiRequestOptions,
    AssistantStream, AuthError, AuthInteraction, AuthResolver, CONTEXT_SAFETY_TOKENS,
    CancellationToken, ChatApi, ContentBlock, Context, EncodeContext, EnvironmentApiKeyAuth,
    ErasedApiFullOptions, ErasedApiHandler, ErasedApiOptionsPatch, HeaderMapSpec, HttpBody,
    HttpChatApi, HttpRequest, HttpResponse, HttpTransport, LocalApiExecutionContext,
    LocalAssistantStream, LocalAuthInteraction, LocalAuthResolver, LocalBoxFuture, LocalChatApi,
    LocalErasedApiHandler, LocalHttpBody, LocalHttpChatApi, LocalHttpResponse, LocalHttpTransport,
    LocalOAuthAuth, LocalProviderAuthResolver, LocalProviderRegistration,
    LocalProviderResponseStream, LocalResolveAuthRequest, LocalResolvedApiRequest, MessageId,
    MiddlewareError, ModelDescriptor, OAuthAuth, OrderedJsonValue, OrderedJsonWriter,
    ProviderAuthResolver, ProviderPayload, ProviderRegistration, ProviderRegistrationError,
    ProviderResponseStream, ResolveAuthRequest, ResolvedApiRequest, ResolvedAuth, SecretString,
    SendBoxFuture, SimpleGenerationOptions, SimpleLoweringContext, Timestamp, TypedModelDescriptor,
    apply_anthropic_messages_full_options_request_headers, estimate_context_tokens,
    parse_ordered_json, transform_context_for_model,
};
use futures_util::{FutureExt, StreamExt, stream};
use http::{HeaderMap, HeaderValue, Method, header};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

static NEXT_MESSAGE_ID: AtomicU64 = AtomicU64::new(1);
const CLAUDE_CODE_SYSTEM_PROMPT: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// Shared Anthropic Messages erased API-family implementation.
#[derive(Clone, Debug)]
pub struct AnthropicMessagesHandler {
    api: ApiId,
}

impl Default for AnthropicMessagesHandler {
    fn default() -> Self {
        Self {
            api: ApiId::new(AnthropicMessages::API_ID),
        }
    }
}

impl ErasedApiHandler for AnthropicMessagesHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(
            model,
            context,
            simple,
            patch,
            execution.endpoint,
            execution.headers,
            execution.api_key,
        )
    }

    fn encode_full(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        encode_full(
            model,
            context,
            options,
            execution.endpoint,
            execution.headers,
            execution.api_key,
        )
    }

    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        apply_full_options_headers(model, context, options, request_options, headers)
    }

    fn finalize_payload(
        &self,
        payload: &mut ProviderPayload,
        execution: &ApiExecutionContext<'_>,
    ) -> Result<(), AiError> {
        finalize_anthropic_payload(payload, execution.model)
    }

    fn decode_stream(
        &self,
        response: ProviderResponseStream,
        execution: &ApiExecutionContext<'_>,
    ) -> AssistantStream {
        let mut decoder = AnthropicMessagesSseDecoder::new(decode_context(
            execution.model,
            execution.context,
            execution.api_key,
        ));
        let pending = decoder.take_events().into();
        AssistantStream::new(stream::unfold(
            SendDecodeStreamState {
                body: response.body,
                decoder,
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_send_decoded_event,
        ))
    }
}

impl LocalErasedApiHandler for AnthropicMessagesHandler {
    fn api_id(&self) -> &ApiId {
        &self.api
    }

    fn lower_and_encode(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        simple: &SimpleGenerationOptions,
        patch: Option<&ErasedApiOptionsPatch>,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        lower_and_encode(
            model,
            context,
            simple,
            patch,
            execution.endpoint,
            execution.headers,
            execution.api_key,
        )
    }

    fn encode_full(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<ProviderPayload, AiError> {
        encode_full(
            model,
            context,
            options,
            execution.endpoint,
            execution.headers,
            execution.api_key,
        )
    }

    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        _effective_base_url: &Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        apply_full_options_headers(model, context, options, request_options, headers)
    }

    fn finalize_payload(
        &self,
        payload: &mut ProviderPayload,
        execution: &LocalApiExecutionContext<'_>,
    ) -> Result<(), AiError> {
        finalize_anthropic_payload(payload, execution.model)
    }

    fn decode_stream(
        &self,
        response: LocalProviderResponseStream,
        execution: &LocalApiExecutionContext<'_>,
    ) -> LocalAssistantStream {
        let mut decoder = AnthropicMessagesSseDecoder::new(decode_context(
            execution.model,
            execution.context,
            execution.api_key,
        ));
        let pending = decoder.take_events().into();
        LocalAssistantStream::new(stream::unfold(
            LocalDecodeStreamState {
                body: response.body,
                decoder,
                cancellation: execution.cancellation.clone(),
                pending,
                done: false,
            },
            next_local_decoded_event,
        ))
    }
}

struct SendDecodeStreamState {
    body: HttpBody,
    decoder: AnthropicMessagesSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

struct LocalDecodeStreamState {
    body: LocalHttpBody,
    decoder: AnthropicMessagesSseDecoder,
    cancellation: CancellationToken,
    pending: VecDeque<agentprism_ai::AssistantEvent>,
    done: bool,
}

enum BodyPoll {
    Cancelled,
    Body(Option<Result<Vec<u8>, agentprism_ai::TransportError>>),
}

async fn next_send_decoded_event(
    mut state: SendDecodeStreamState,
) -> Option<(agentprism_ai::AssistantEvent, SendDecodeStreamState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_send_body(&mut state.body, &state.cancellation).await {
            BodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            BodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            BodyPoll::Body(Some(Err(error))) => {
                state
                    .pending
                    .extend(state.decoder.fail_transport("transport", error.message));
                state.done = true;
            }
            BodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_local_decoded_event(
    mut state: LocalDecodeStreamState,
) -> Option<(agentprism_ai::AssistantEvent, LocalDecodeStreamState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((event, state));
        }
        if state.done {
            return None;
        }
        match next_local_body(&mut state.body, &state.cancellation).await {
            BodyPoll::Cancelled => {
                state
                    .pending
                    .extend(state.decoder.cancel("Request was aborted"));
                state.done = true;
            }
            BodyPoll::Body(Some(Ok(chunk))) => {
                state.pending.extend(state.decoder.push(&chunk));
                state.done = state.decoder.is_terminated();
            }
            BodyPoll::Body(Some(Err(error))) => {
                state
                    .pending
                    .extend(state.decoder.fail_transport("transport", error.message));
                state.done = true;
            }
            BodyPoll::Body(None) => {
                state.pending.extend(state.decoder.finish());
                state.done = true;
            }
        }
    }
}

async fn next_send_body(body: &mut HttpBody, cancellation: &CancellationToken) -> BodyPoll {
    if cancellation.is_cancelled() {
        return BodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => BodyPoll::Cancelled,
        item = next => BodyPoll::Body(item),
    }
}

async fn next_local_body(body: &mut LocalHttpBody, cancellation: &CancellationToken) -> BodyPoll {
    if cancellation.is_cancelled() {
        return BodyPoll::Cancelled;
    }
    let cancelled = cancellation.cancelled().fuse();
    let next = body.next().fuse();
    futures_util::pin_mut!(cancelled, next);
    futures_util::select_biased! {
        _ = cancelled => BodyPoll::Cancelled,
        item = next => BodyPoll::Body(item),
    }
}

fn lower_and_encode(
    model: &ModelDescriptor,
    context: &Context,
    simple: &SimpleGenerationOptions,
    patch: Option<&ErasedApiOptionsPatch>,
    endpoint: &Url,
    headers: &HeaderMap,
    api_key: Option<&SecretString>,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::AnthropicMessages(config) = &model.api else {
        return Err(invalid_request(
            model,
            format!(
                "model uses API {}, not anthropic-messages",
                model.api.api_id()
            ),
        ));
    };
    assert_request_auth(model, api_key, headers)?;
    let typed = TypedModelDescriptor::<AnthropicMessages> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compatibility = AnthropicMessages::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let (estimated_input_tokens, available_context_tokens) =
        if model.common.limits.context_window == 0 {
            (0, 0)
        } else {
            // Architecture v2 part 2 §2.6 and pinned Pi's streamSimple path
            // require simple planning to observe the durable canonical context.
            // Handoff projection (including failed-turn omission and orphan
            // repair) happens only later while buildParams shapes the wire body.
            let estimate = estimate_context_tokens(context)
                .map_err(|error| invalid_request(model, error.to_string()))?;
            let available = model
                .common
                .limits
                .context_window
                .saturating_sub(estimate.tokens)
                .saturating_sub(CONTEXT_SAFETY_TOKENS);
            (estimate.tokens, available)
        };
    let patch = parse_patch(model, patch)?;
    let options = AnthropicMessages::lower_simple(
        SimpleLoweringContext {
            model: &typed,
            compat: &compatibility,
            effective_base_url: endpoint,
            estimated_input_tokens,
            available_context_tokens,
        },
        simple,
        &patch,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    let mut projected = transform_context_for_model(
        context,
        model,
        &Default::default(),
        &AnthropicMessagesHandoff,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?
    .context;
    let oauth = api_key.is_some_and(is_oauth_secret);
    if oauth {
        normalize_claude_code_tool_names(&mut projected);
    }
    encode_options_payload(
        typed,
        projected,
        compatibility,
        &options,
        endpoint,
        oauth,
        model,
    )
}

fn encode_full(
    model: &ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    endpoint: &Url,
    headers: &HeaderMap,
    api_key: Option<&SecretString>,
) -> Result<ProviderPayload, AiError> {
    let ApiModelConfig::AnthropicMessages(config) = &model.api else {
        return Err(invalid_request(
            model,
            format!(
                "model uses API {}, not anthropic-messages",
                model.api.api_id()
            ),
        ));
    };
    let options = options
        .downcast_ref::<AnthropicMessages>()
        .ok_or_else(|| invalid_request(model, "invalid anthropic-messages full options type"))?;
    assert_request_auth(model, api_key, headers)?;
    let typed = TypedModelDescriptor::<AnthropicMessages> {
        common: model.common.clone(),
        config: config.clone(),
        extensions: model.extensions.clone(),
    };
    let compatibility = AnthropicMessages::resolve_compat(endpoint, &config.compat)
        .map_err(|error| invalid_request(model, error.to_string()))?;
    let mut projected = transform_context_for_model(
        context,
        model,
        &Default::default(),
        &AnthropicMessagesHandoff,
    )
    .map_err(|error| invalid_request(model, error.to_string()))?
    .context;
    let oauth = api_key.is_some_and(is_oauth_secret);
    if oauth {
        normalize_claude_code_tool_names(&mut projected);
    }

    encode_options_payload(
        typed,
        projected,
        compatibility,
        options,
        endpoint,
        oauth,
        model,
    )
}

fn encode_options_payload(
    typed: TypedModelDescriptor<AnthropicMessages>,
    projected: Context,
    compatibility: agentprism_ai::AnthropicMessagesCompat,
    options: &AnthropicOptions,
    endpoint: &Url,
    oauth: bool,
    model: &ModelDescriptor,
) -> Result<ProviderPayload, AiError> {
    let wire = agentprism_ai::encode_anthropic_messages_with_system_prefix(
        EncodeContext {
            model: &typed,
            context: &projected,
            compat: &compatibility,
            effective_base_url: endpoint,
        },
        options,
        oauth.then_some(CLAUDE_CODE_SYSTEM_PROMPT),
    )
    .map_err(|error| invalid_request(model, error.to_string()))?;
    Ok(ProviderPayload::typed::<AnthropicMessages, _>(
        Method::POST,
        typed,
        wire,
        |request| {
            OrderedJsonWriter::to_vec(&request.clone().into()).map_err(|error| {
                MiddlewareError::new(
                    "provider_payload_encode",
                    format!("failed to encode Anthropic payload: {error}"),
                )
            })
        },
    ))
}

fn apply_full_options_headers(
    model: &ModelDescriptor,
    context: &Context,
    options: &ErasedApiFullOptions,
    request_options: &ApiRequestOptions,
    headers: &mut HeaderMap,
) -> Result<(), AiError> {
    let ApiModelConfig::AnthropicMessages(config) = &model.api else {
        return Err(invalid_request(
            model,
            format!(
                "model uses API {}, not anthropic-messages",
                model.api.api_id()
            ),
        ));
    };
    let options = options
        .downcast_ref::<AnthropicMessages>()
        .ok_or_else(|| invalid_request(model, "invalid anthropic-messages full options type"))?;
    apply_anthropic_messages_full_options_request_headers(
        config,
        context,
        options,
        request_options.session_id.as_deref(),
        headers,
    )
    .map_err(|error| invalid_request(model, error.message))
}

fn finalize_anthropic_payload(
    payload: &mut ProviderPayload,
    model: &ModelDescriptor,
) -> Result<(), AiError> {
    let body = payload
        .encode_body()
        .map_err(|error| invalid_request(model, error.message))?;
    let value = parse_ordered_json(&body)
        .map_err(|error| invalid_request(model, format!("invalid Anthropic payload: {error}")))?;
    let OrderedJsonValue::Object(mut request) = value else {
        return Err(invalid_request(
            model,
            "Anthropic payload transform must return a JSON object",
        ));
    };
    // Pinned Pi executes `onPayload` first, then spreads the resulting object
    // and reasserts `stream: true`. This must also cover an erased transform
    // that replaced the typed payload with raw JSON bytes.
    request.insert("stream", true);
    let body = OrderedJsonWriter::to_vec(&request.into()).map_err(|error| {
        invalid_request(
            model,
            format!("failed to encode finalized Anthropic payload: {error}"),
        )
    })?;
    let method = payload.method.clone();
    *payload = ProviderPayload::json(body);
    payload.method = method;
    Ok(())
}

fn assert_request_auth(
    model: &ModelDescriptor,
    api_key: Option<&SecretString>,
    headers: &HeaderMap,
) -> Result<(), AiError> {
    if api_key.is_some_and(|key| !key.expose_secret().is_empty())
        || [
            header::AUTHORIZATION.as_str(),
            "x-api-key",
            "cf-aig-authorization",
        ]
        .into_iter()
        .any(|name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| !value.trim().is_empty())
        })
    {
        return Ok(());
    }

    Err(AiError::new(
        AiErrorKind::Authentication,
        format!(
            "No API key for provider: {}",
            model.common.model_ref.provider
        ),
    )
    .with_model(model.common.model_ref.clone()))
}

fn parse_patch(
    model: &ModelDescriptor,
    patch: Option<&ErasedApiOptionsPatch>,
) -> Result<AnthropicSimplePatch, AiError> {
    let Some(patch) = patch else {
        return Ok(AnthropicSimplePatch::default());
    };
    if patch.schema_version != 1 {
        return Err(invalid_request(
            model,
            format!(
                "unsupported anthropic-messages options schema version {}",
                patch.schema_version
            ),
        ));
    }
    serde_json::from_str(patch.value.get())
        .map_err(|error| invalid_request(model, format!("invalid API options patch: {error}")))
}

fn invalid_request(model: &ModelDescriptor, message: impl Into<String>) -> AiError {
    AiError::new(AiErrorKind::InvalidRequest, message).with_model(model.common.model_ref.clone())
}

fn decode_context(
    model: &ModelDescriptor,
    context: &Context,
    api_key: Option<&SecretString>,
) -> AnthropicMessagesDecodeContext {
    let tool_name_aliases = if api_key.is_some_and(is_oauth_secret) {
        context
            .tools
            .iter()
            .map(|tool| {
                (
                    claude_code_tool_name(&tool.name).to_ascii_lowercase(),
                    tool.name.clone(),
                )
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    AnthropicMessagesDecodeContext {
        message_id: MessageId::new(format!(
            "anthropic-message-{}",
            NEXT_MESSAGE_ID.fetch_add(1, Ordering::Relaxed)
        )),
        provider: model.common.model_ref.provider.clone(),
        requested_model: model.common.model_ref.model.clone(),
        timestamp: now_timestamp(),
        tool_name_aliases,
    }
}

fn is_oauth_secret(secret: &SecretString) -> bool {
    secret.expose_secret().contains("sk-ant-oat")
}

fn normalize_claude_code_tool_names(context: &mut Context) {
    for tool in &mut context.tools {
        tool.name = claude_code_tool_name(&tool.name);
    }
    for message in &mut context.messages {
        match message {
            agentprism_ai::Message::Assistant(message) => {
                for block in &mut message.content {
                    if let ContentBlock::ToolCall { call, .. } = block {
                        call.name = claude_code_tool_name(&call.name);
                    }
                }
            }
            agentprism_ai::Message::ToolResult(message) => {
                message.tool_name = claude_code_tool_name(&message.tool_name);
                for name in &mut message.added_tool_names {
                    *name = claude_code_tool_name(name);
                }
            }
            agentprism_ai::Message::User(_) => {}
        }
    }
}

fn claude_code_tool_name(name: &str) -> String {
    const TOOLS: [&str; 17] = [
        "Read",
        "Write",
        "Edit",
        "Bash",
        "Grep",
        "Glob",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "KillShell",
        "NotebookEdit",
        "Skill",
        "Task",
        "TaskOutput",
        "TodoWrite",
        "WebFetch",
        "WebSearch",
    ];
    TOOLS
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_owned(), |candidate| (*candidate).to_owned())
}

fn now_timestamp() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

/// Creates a shared Send Anthropic Messages API object.
pub fn anthropic_messages_api(transport: Arc<dyn HttpTransport>) -> Arc<dyn ChatApi> {
    Arc::new(AnthropicMessagesApi {
        inner: HttpChatApi::new(
            Arc::new(AnthropicMessagesHandler::default()),
            Arc::new(AnthropicMessagesTransport::new(transport)),
        ),
    })
}

/// Creates a local-executor Anthropic Messages API object.
pub fn local_anthropic_messages_api(transport: Rc<dyn LocalHttpTransport>) -> Rc<dyn LocalChatApi> {
    Rc::new(LocalAnthropicMessagesApi {
        inner: LocalHttpChatApi::new(
            Rc::new(AnthropicMessagesHandler::default()),
            Rc::new(LocalAnthropicMessagesTransport::new(transport)),
        ),
    })
}

struct AnthropicMessagesApi {
    inner: HttpChatApi,
}

impl ChatApi for AnthropicMessagesApi {
    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        effective_base_url: &Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.inner.apply_full_options_headers(
            model,
            context,
            options,
            effective_base_url,
            request_options,
            headers,
        )
    }

    fn stream(
        &self,
        request: ResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<AssistantStream, AiError>> {
        self.inner.stream(request, cancellation)
    }
}

struct LocalAnthropicMessagesApi {
    inner: LocalHttpChatApi,
}

impl LocalChatApi for LocalAnthropicMessagesApi {
    fn apply_full_options_headers(
        &self,
        model: &ModelDescriptor,
        context: &Context,
        options: &ErasedApiFullOptions,
        effective_base_url: &Url,
        request_options: &ApiRequestOptions,
        headers: &mut HeaderMap,
    ) -> Result<(), AiError> {
        self.inner.apply_full_options_headers(
            model,
            context,
            options,
            effective_base_url,
            request_options,
            headers,
        )
    }

    fn stream(
        &self,
        request: LocalResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalAssistantStream, AiError>> {
        self.inner.stream(request, cancellation)
    }
}

const OAUTH_BETAS: &str = "claude-code-20250219,oauth-2025-04-20";
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.75";

/// Returns the API-family defaults used by an Anthropic provider
/// registration. Models applies these before auth, model headers, explicit
/// request headers, and the final header transform.
pub fn anthropic_default_headers() -> HeaderMapSpec {
    let mut headers = HeaderMapSpec::new();
    headers.insert("accept".to_owned(), Some("application/json".to_owned()));
    headers.insert(
        "content-type".to_owned(),
        Some("application/json".to_owned()),
    );
    headers.insert(
        "anthropic-version".to_owned(),
        Some("2023-06-01".to_owned()),
    );
    headers.insert(
        "anthropic-dangerous-direct-browser-access".to_owned(),
        Some("true".to_owned()),
    );
    headers.insert("user-agent".to_owned(), Some(anthropic_user_agent()));
    headers
}

/// Returns the default Pi-compatible Anthropic Messages user agent.
pub fn anthropic_user_agent() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        "pi (browser)".to_owned()
    }
    #[cfg(all(not(target_arch = "wasm32"), unix))]
    {
        let system = rustix::system::uname();
        let platform = match system.sysname().to_string_lossy().as_ref() {
            "Darwin" => "darwin".to_owned(),
            value => value.to_ascii_lowercase(),
        };
        let release = system.release().to_string_lossy();
        let architecture = match system.machine().to_string_lossy().as_ref() {
            "x86_64" => "x64".to_owned(),
            "aarch64" | "arm64" => "arm64".to_owned(),
            value => value.to_owned(),
        };
        format!("pi ({platform} {release}; {architecture})")
    }
    #[cfg(all(not(target_arch = "wasm32"), windows))]
    {
        use windows_sys::Wdk::System::SystemServices::RtlGetVersion;
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

        let mut version = OSVERSIONINFOW {
            dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>())
                .expect("OSVERSIONINFOW size fits u32"),
            ..OSVERSIONINFOW::default()
        };
        // SAFETY: `version` is initialized with the structure size required by
        // RtlGetVersion and remains valid and exclusively borrowed for the call.
        let status = unsafe { RtlGetVersion(&mut version) };
        let release = if status >= 0 {
            format!(
                "{}.{}.{}",
                version.dwMajorVersion, version.dwMinorVersion, version.dwBuildNumber
            )
        } else {
            "unknown".to_owned()
        };
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            value => value,
        };
        format!("pi (win32 {release}; {architecture})")
    }
    #[cfg(all(not(target_arch = "wasm32"), not(unix), not(windows)))]
    {
        let platform = std::env::consts::OS;
        let architecture = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            value => value,
        };
        format!("pi ({platform} unknown; {architecture})")
    }
}

/// Transport decorator resolving a provider base URL to `/v1/messages`.
#[derive(Clone)]
pub struct AnthropicMessagesTransport {
    inner: Arc<dyn HttpTransport>,
}

impl AnthropicMessagesTransport {
    /// Wraps one injected provider transport.
    pub fn new(inner: Arc<dyn HttpTransport>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for AnthropicMessagesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AnthropicMessagesTransport")
            .finish_non_exhaustive()
    }
}

impl HttpTransport for AnthropicMessagesTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<HttpResponse, agentprism_ai::TransportError>> {
        request.url = messages_url(&request.url);
        self.inner.execute(request, cancellation)
    }
}

/// Local transport decorator resolving `/v1/messages`.
#[derive(Clone)]
pub struct LocalAnthropicMessagesTransport {
    inner: Rc<dyn LocalHttpTransport>,
}

impl LocalAnthropicMessagesTransport {
    /// Wraps one injected local provider transport.
    pub fn new(inner: Rc<dyn LocalHttpTransport>) -> Self {
        Self { inner }
    }
}

impl fmt::Debug for LocalAnthropicMessagesTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalAnthropicMessagesTransport")
            .finish_non_exhaustive()
    }
}

impl LocalHttpTransport for LocalAnthropicMessagesTransport {
    fn execute(
        &self,
        mut request: HttpRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<LocalHttpResponse, agentprism_ai::TransportError>> {
        request.url = messages_url(&request.url);
        self.inner.execute(request, cancellation)
    }
}

fn messages_url(base: &Url) -> Url {
    let mut url = base.clone();
    let path = base.path().trim_end_matches('/');
    url.set_path(&format!("{path}/v1/messages"));
    url
}

/// Builds the built-in Anthropic provider registration.
pub fn anthropic_provider(
    transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, AnthropicProviderError> {
    let api = anthropic_messages_api(Arc::clone(&transport));
    anthropic_provider_with_api(api, transport)
}

/// Builds the built-in Anthropic provider around a caller-shared API object.
pub fn anthropic_provider_with_api(
    api: Arc<dyn ChatApi>,
    oauth_transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, AnthropicProviderError> {
    ProviderRegistration::builder("anthropic")
        .display_name("Anthropic")
        .base_url(Url::parse("https://api.anthropic.com").map_err(AnthropicProviderError::Url)?)
        .headers(anthropic_default_headers())
        .auth(Arc::new(AnthropicAuthResolver::new(Some(Arc::new(
            crate::AnthropicOAuth::new(oauth_transport),
        )))))
        .models(anthropic_models().map_err(AnthropicProviderError::Catalog)?)
        .api(AnthropicMessages::API_ID, api)
        .build()
        .map_err(AnthropicProviderError::Registration)
}

/// Builds the local-executor Anthropic provider registration.
pub fn local_anthropic_provider(
    transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, AnthropicProviderError> {
    let api = local_anthropic_messages_api(Rc::clone(&transport));
    local_anthropic_provider_with_api(api, transport)
}

/// Builds the local Anthropic registration around a shared API and OAuth transport.
pub fn local_anthropic_provider_with_api(
    api: Rc<dyn LocalChatApi>,
    oauth_transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, AnthropicProviderError> {
    LocalProviderRegistration::builder("anthropic")
        .display_name("Anthropic")
        .base_url(Url::parse("https://api.anthropic.com").map_err(AnthropicProviderError::Url)?)
        .headers(anthropic_default_headers())
        .auth(Rc::new(LocalAnthropicAuthResolver::new(Some(Rc::new(
            crate::LocalAnthropicOAuth::new(oauth_transport),
        )))))
        .models(anthropic_models().map_err(AnthropicProviderError::Catalog)?)
        .api(AnthropicMessages::API_ID, api)
        .build()
        .map_err(AnthropicProviderError::Registration)
}

/// Error while building the built-in Anthropic registration.
#[derive(Debug)]
pub enum AnthropicProviderError {
    /// Pinned catalog data was invalid.
    Catalog(crate::AnthropicCatalogError),
    /// The built-in endpoint URL was invalid.
    Url(url::ParseError),
    /// The assembled provider registration violated a core invariant.
    Registration(ProviderRegistrationError),
}

impl fmt::Display for AnthropicProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Catalog(error) => write!(formatter, "catalog error: {error}"),
            Self::Url(error) => write!(formatter, "URL error: {error}"),
            Self::Registration(error) => write!(formatter, "registration error: {error}"),
        }
    }
}

impl std::error::Error for AnthropicProviderError {}

struct AnthropicAuthResolver {
    inner: ProviderAuthResolver,
}

impl AnthropicAuthResolver {
    fn new(oauth: Option<Arc<dyn OAuthAuth>>) -> Self {
        Self {
            inner: ProviderAuthResolver::new(
                Some(Arc::new(EnvironmentApiKeyAuth::new(
                    "Anthropic API key",
                    [
                        "ANTHROPIC_AUTH_TOKEN",
                        "ANTHROPIC_OAUTH_TOKEN",
                        "ANTHROPIC_API_KEY",
                    ],
                ))),
                oauth,
            ),
        }
    }
}

impl AuthResolver for AnthropicAuthResolver {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_anthropic_auth_header(&mut resolved)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<agentprism_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> SendBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}

struct LocalAnthropicAuthResolver {
    inner: LocalProviderAuthResolver,
}

impl LocalAnthropicAuthResolver {
    fn new(oauth: Option<Rc<dyn LocalOAuthAuth>>) -> Self {
        Self {
            inner: LocalProviderAuthResolver::new(
                Some(Rc::new(EnvironmentApiKeyAuth::new(
                    "Anthropic API key",
                    [
                        "ANTHROPIC_AUTH_TOKEN",
                        "ANTHROPIC_OAUTH_TOKEN",
                        "ANTHROPIC_API_KEY",
                    ],
                ))),
                oauth,
            ),
        }
    }
}

impl LocalAuthResolver for LocalAnthropicAuthResolver {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let Some(mut resolved) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            insert_anthropic_auth_header(&mut resolved)?;
            Ok(Some(resolved))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<agentprism_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }

    fn logout(&self, cancellation: CancellationToken) -> LocalBoxFuture<'_, Result<(), AuthError>> {
        self.inner.logout(cancellation)
    }
}

fn insert_anthropic_auth_header(resolved: &mut ResolvedAuth) -> Result<(), AuthError> {
    let Some(secret) = resolved.api_key.as_ref() else {
        return Ok(());
    };
    let oauth_token_environment = resolved.source.0 == "ANTHROPIC_OAUTH_TOKEN";
    let oauth = resolved.source.0 == "OAuth" || secret.expose_secret().contains("sk-ant-oat");
    let (name, value) = if resolved.source.0 == "ANTHROPIC_AUTH_TOKEN" || oauth {
        (
            header::AUTHORIZATION,
            format!("Bearer {}", secret.expose_secret()),
        )
    } else {
        (
            http::HeaderName::from_static("x-api-key"),
            secret.expose_secret().to_owned(),
        )
    };
    let value = HeaderValue::from_str(&value).map_err(|_| {
        AuthError::new(
            "invalid_api_key",
            "credential cannot be encoded as a header",
        )
    })?;
    if !oauth && !oauth_token_environment {
        resolved.api_key = None;
    }
    resolved.headers.insert(name, value);
    if oauth {
        resolved
            .headers
            .insert("anthropic-beta", HeaderValue::from_static(OAUTH_BETAS));
        resolved.headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static(CLAUDE_CODE_USER_AGENT),
        );
        resolved
            .headers
            .insert("x-app", HeaderValue::from_static("cli"));
    }
    Ok(())
}
