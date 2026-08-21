use crate::api::google_generative_ai::{
    GoogleRequestTarget, GoogleThinkingOptions, GoogleToolChoice, consume_google_stream,
    create_google_backend, google_budget, google_wire_request_from_params, is_gemini_3_flash_model,
    is_gemini_3_pro_model, start_google_stream, thinking_level,
};
use crate::api::google_shared::{
    ResolvedGoogleThinkingLevel, resolve_google_thinking_level, retry_google_request,
};
use crate::api::simple_options::build_base_options;
use crate::api::{ApiStreamOptions, ProviderStreams};
use crate::event_stream::{AssistantMessageEvent, AssistantMessageEventStream};
use crate::models::clamp_thinking_level;
use crate::types::{
    Context, ErrorStopReason, Model, ModelThinkingLevel, ProviderEnv, SimpleStreamOptions,
    StopReason, StreamOptions, ThinkingBudgets, ToolChoice, is_default_fetch,
};
use crate::utils::provider_env::get_provider_env_value;
use crate::utils::provider_retry::ProviderRetryOptions;
use google_cloud_auth::credentials::Credentials;
use reqwest_012::header::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};

const API_VERSION: &str = "v1";
pub const GCP_VERTEX_CREDENTIALS_MARKER: &str = "gcp-vertex-credentials";
static TOOL_CALL_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoogleVertexOptions {
    #[serde(flatten)]
    pub stream: StreamOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<GoogleToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<GoogleThinkingOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct GoogleVertexApi;

pub fn google_vertex_api() -> GoogleVertexApi {
    GoogleVertexApi
}

impl ProviderStreams for GoogleVertexApi {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: ApiStreamOptions,
    ) -> AssistantMessageEventStream {
        let options = match options {
            ApiStreamOptions::Base(stream) => GoogleVertexOptions {
                stream,
                ..GoogleVertexOptions::default()
            },
            ApiStreamOptions::GoogleVertex(options) => options,
            ApiStreamOptions::Custom { base, extra } => {
                let mut value = serde_json::to_value(base).unwrap_or(Value::Object(Map::new()));
                if let Some(object) = value.as_object_mut() {
                    object.extend(extra);
                }
                match serde_json::from_value(value) {
                    Ok(options) => options,
                    Err(error) => return setup_error_stream(model, error.to_string()),
                }
            }
            _ => {
                return setup_error_stream(
                    model,
                    "Google Vertex received options for a different API".to_owned(),
                );
            }
        };
        stream(model, context, options)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        stream_simple(model, context, options)
    }
}

pub fn stream(
    model: &Model,
    context: &Context,
    options: GoogleVertexOptions,
) -> AssistantMessageEventStream {
    let (sender, stream) = AssistantMessageEventStream::channel();
    let model = model.clone();
    let context = context.clone();
    tokio::spawn(async move {
        let mut output = crate::types::AssistantMessage::pending(
            "google-vertex",
            model.provider.clone(),
            model.id.clone(),
            now_millis(),
        );
        if let Err(error) = run_stream(&sender, &model, &context, &options, &mut output).await {
            output.stop_reason = if options
                .stream
                .request
                .signal
                .as_ref()
                .is_some_and(|signal| signal.is_aborted())
            {
                StopReason::Aborted
            } else {
                StopReason::Error
            };
            output.error_message = Some(error);
            let _ = sender.send(AssistantMessageEvent::Error {
                reason: if output.stop_reason == StopReason::Aborted {
                    ErrorStopReason::Aborted
                } else {
                    ErrorStopReason::Error
                },
                error: output,
            });
        }
    });
    stream
}

async fn run_stream(
    sender: &crate::event_stream::AssistantStreamSender,
    model: &Model,
    context: &Context,
    options: &GoogleVertexOptions,
    output: &mut crate::types::AssistantMessage,
) -> Result<(), String> {
    if options
        .stream
        .request
        .fetch
        .as_ref()
        .is_some_and(|fetch| !is_default_fetch(fetch))
    {
        return Err("Custom fetch is not supported by the Google Vertex adapter".to_owned());
    }
    let api_key = resolve_api_key(Some(options));
    let mut needs_adc_credentials = false;
    let mut backend = if let Some(api_key) = api_key {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("x-goog-api-key"),
            HeaderValue::from_str(&api_key).map_err(|error| error.to_string())?,
        );
        let (base_url, api_version, collection_scope) =
            vertex_backend_defaults(model, "https://aiplatform.googleapis.com", true);
        create_google_backend(
            model,
            headers,
            &options.stream,
            base_url,
            api_version,
            collection_scope,
            GoogleRequestTarget::vertex(None, None),
        )?
    } else {
        let project = resolve_project(Some(options))?;
        let location = resolve_location(Some(options))?;
        let host = if location == "global" {
            "https://aiplatform.googleapis.com".to_owned()
        } else if matches!(location.as_str(), "us" | "eu") {
            format!("https://aiplatform.{location}.rep.googleapis.com")
        } else {
            format!("https://{location}-aiplatform.googleapis.com")
        };
        let (base_url, api_version, collection_scope) =
            vertex_backend_defaults(model, &host, false);
        needs_adc_credentials = true;
        create_google_backend(
            model,
            HeaderMap::new(),
            &options.stream,
            base_url,
            api_version,
            collection_scope,
            GoogleRequestTarget::vertex(Some(project), Some(location)),
        )?
    };
    let mut params = build_params(model, context, options)?;
    if let Some(on_payload) = &options.stream.request.on_payload
        && let Some(replacement) = on_payload(params.clone(), model).await?
    {
        params = replacement;
    }
    let request = google_wire_request_from_params(&params, &backend.target)?;
    if needs_adc_credentials {
        backend = backend.with_credentials(create_adc_credentials(options)?);
    }
    let retry = ProviderRetryOptions {
        max_retries: options.stream.request.max_retries,
        max_retry_delay_ms: options.stream.request.max_retry_delay_ms,
        signal: options.stream.request.signal.clone(),
    };
    let google_stream = retry_google_request(
        || {
            let request = request.clone();
            start_google_stream(&backend, request, options.stream.request.signal.as_ref())
        },
        retry,
    )
    .await
    .map_err(|error| error.to_string())?;
    consume_google_stream(
        sender,
        model,
        &options.stream,
        output,
        google_stream,
        &TOOL_CALL_COUNTER,
        "Google Vertex stream ended without a finish reason",
    )
    .await
}

fn create_adc_credentials(options: &GoogleVertexOptions) -> Result<Credentials, String> {
    if let Some(filename) = build_google_auth_key_filename(options.stream.request.env.as_ref()) {
        let credential = std::fs::read_to_string(filename).map_err(|error| error.to_string())?;
        let value =
            serde_json::from_str::<Value>(&credential).map_err(|error| error.to_string())?;
        return match value.get("type").and_then(Value::as_str) {
            Some("service_account") => {
                google_cloud_auth::credentials::service_account::Builder::new(value)
                    .build()
                    .map_err(|error| error.to_string())
            }
            Some("external_account") => {
                google_cloud_auth::credentials::external_account::Builder::new(value)
                    .build()
                    .map_err(|error| error.to_string())
            }
            Some("authorized_user") => {
                google_cloud_auth::credentials::user_account::Builder::new(value)
                    .build()
                    .map_err(|error| error.to_string())
            }
            Some("impersonated_service_account") => {
                google_cloud_auth::credentials::impersonated::Builder::new(value)
                    .build()
                    .map_err(|error| error.to_string())
            }
            Some(kind) => Err(format!("unsupported credential type: {kind}")),
            None => Err("credential JSON is missing type".to_owned()),
        };
    }
    google_cloud_auth::credentials::Builder::default()
        .build()
        .map_err(|error| error.to_string())
}

fn build_params(
    model: &Model,
    context: &Context,
    options: &GoogleVertexOptions,
) -> Result<Value, String> {
    let google = crate::api::google_generative_ai::GoogleOptions {
        stream: options.stream.clone(),
        tool_choice: options.tool_choice,
        thinking: options.thinking.clone(),
    };
    let mut params = crate::api::google_generative_ai::build_params(model, context, &google)?;
    if model.reasoning
        && options
            .thinking
            .as_ref()
            .is_some_and(|thinking| !thinking.enabled)
        && !is_gemini_3_pro_model(model)
        && !is_gemini_3_flash_model(model)
    {
        params["config"]["thinkingConfig"] = json!({ "thinkingBudget": 0 });
    }
    Ok(params)
}

pub fn stream_simple(
    model: &Model,
    context: &Context,
    options: SimpleStreamOptions,
) -> AssistantMessageEventStream {
    let mut base = GoogleVertexOptions {
        stream: build_base_options(model, context, Some(&options), None),
        tool_choice: options.tool_choice.map(|choice| match choice {
            ToolChoice::Auto => GoogleToolChoice::Auto,
            ToolChoice::None => GoogleToolChoice::None,
        }),
        thinking: None,
        project: None,
        location: None,
    };
    let Some(reasoning) = options.reasoning else {
        base.thinking = Some(GoogleThinkingOptions {
            enabled: false,
            budget_tokens: None,
            level: None,
        });
        return stream(model, context, base);
    };
    let clamped = clamp_thinking_level(model, model_thinking_level(reasoning));
    let level = match resolve_google_thinking_level(model, clamped) {
        Ok(level) => level,
        Err(error) => return setup_error_stream(model, error),
    };
    base.thinking = if is_gemini_3_pro_model(model) || is_gemini_3_flash_model(model) {
        Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: None,
            level: Some(thinking_level(level, model)),
        })
    } else {
        Some(GoogleThinkingOptions {
            enabled: true,
            budget_tokens: Some(vertex_google_budget(
                model,
                level,
                options.thinking_budgets.as_ref(),
            )),
            level: None,
        })
    };
    stream(model, context, base)
}

fn vertex_google_budget(
    model: &Model,
    level: ResolvedGoogleThinkingLevel,
    custom: Option<&ThinkingBudgets>,
) -> f64 {
    if let Some(custom) = custom.and_then(|budgets| match level {
        ResolvedGoogleThinkingLevel::Minimal => budgets.minimal,
        ResolvedGoogleThinkingLevel::Low => budgets.low,
        ResolvedGoogleThinkingLevel::Medium => budgets.medium,
        ResolvedGoogleThinkingLevel::High => budgets.high,
    }) {
        return custom;
    }
    if model.id.contains("2.5-pro") || model.id.contains("2.5-flash") {
        return match level {
            ResolvedGoogleThinkingLevel::Minimal => 128.0,
            ResolvedGoogleThinkingLevel::Low => 2_048.0,
            ResolvedGoogleThinkingLevel::Medium => 8_192.0,
            ResolvedGoogleThinkingLevel::High => 24_576.0,
        };
    }
    google_budget(model, level, None)
}

fn model_thinking_level(level: crate::types::ThinkingLevel) -> ModelThinkingLevel {
    match level {
        crate::types::ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
        crate::types::ThinkingLevel::Low => ModelThinkingLevel::Low,
        crate::types::ThinkingLevel::Medium => ModelThinkingLevel::Medium,
        crate::types::ThinkingLevel::High => ModelThinkingLevel::High,
        crate::types::ThinkingLevel::Xhigh => ModelThinkingLevel::Xhigh,
        crate::types::ThinkingLevel::Max => ModelThinkingLevel::Max,
    }
}

pub fn resolve_api_key(options: Option<&GoogleVertexOptions>) -> Option<String> {
    options
        .and_then(|options| options.stream.request.api_key.as_deref())
        .map(str::trim)
        .filter(|key| {
            !key.is_empty()
                && *key != GCP_VERTEX_CREDENTIALS_MARKER
                && !regex::Regex::new(r"^<[^>]+>$")
                    .expect("static regular expression")
                    .is_match(key)
        })
        .map(str::to_owned)
}

pub fn resolve_project(options: Option<&GoogleVertexOptions>) -> Result<String, String> {
    options
        .and_then(|options| options.project.clone())
        .filter(|project| !project.is_empty())
        .or_else(|| {
            get_provider_env_value(
                "GOOGLE_CLOUD_PROJECT",
                options.and_then(|options| options.stream.request.env.as_ref()),
            )
        })
        .or_else(|| {
            get_provider_env_value(
                "GCLOUD_PROJECT",
                options.and_then(|options| options.stream.request.env.as_ref()),
            )
        })
        .ok_or_else(|| {
            "Vertex AI requires a project ID. Set GOOGLE_CLOUD_PROJECT/GCLOUD_PROJECT or pass project in options."
                .to_owned()
        })
}

pub fn resolve_location(options: Option<&GoogleVertexOptions>) -> Result<String, String> {
    options
        .and_then(|options| options.location.clone())
        .filter(|location| !location.is_empty())
        .or_else(|| {
            get_provider_env_value(
                "GOOGLE_CLOUD_LOCATION",
                options.and_then(|options| options.stream.request.env.as_ref()),
            )
        })
        .ok_or_else(|| {
            "Vertex AI requires a location. Set GOOGLE_CLOUD_LOCATION or pass location in options."
                .to_owned()
        })
}

pub fn resolve_custom_base_url(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    (!trimmed.is_empty() && !trimmed.contains("{location}")).then(|| trimmed.to_owned())
}

pub fn base_url_includes_api_version(base_url: &str) -> bool {
    let pattern = regex::Regex::new(r"^v\d+(?:beta\d*)?$").expect("static regular expression");
    url::Url::parse(base_url)
        .ok()
        .is_some_and(|url| url.path().split('/').any(|part| pattern.is_match(part)))
        || regex::Regex::new(r"(?:^|/)v\d+(?:beta\d*)?(?:/|$)")
            .expect("static regular expression")
            .is_match(base_url)
}

fn build_google_auth_key_filename(env: Option<&ProviderEnv>) -> Option<String> {
    get_provider_env_value("GOOGLE_APPLICATION_CREDENTIALS", env)
}

#[cfg(test)]
fn vertex_api_key_base_url(model: &Model) -> String {
    let (base_url, api_version, _) =
        vertex_backend_defaults(model, "https://aiplatform.googleapis.com", true);
    join_base_and_version(&base_url, &api_version)
}

#[cfg(test)]
fn vertex_adc_base_url(model: &Model, project: &str, location: &str) -> String {
    let host = if location == "global" {
        "https://aiplatform.googleapis.com".to_owned()
    } else if matches!(location, "us" | "eu") {
        format!("https://aiplatform.{location}.rep.googleapis.com")
    } else {
        format!("https://{location}-aiplatform.googleapis.com")
    };
    let (base_url, api_version, collection_scope) = vertex_backend_defaults(model, &host, false);
    let mut base_url = join_base_and_version(&base_url, &api_version);
    if !collection_scope {
        base_url.push_str(&format!("projects/{project}/locations/{location}/"));
    }
    base_url
}

fn vertex_backend_defaults(
    model: &Model,
    default_base_url: &str,
    api_key: bool,
) -> (String, String, bool) {
    if let Some(base_url) = resolve_custom_base_url(&model.base_url) {
        let api_version = if base_url_includes_api_version(&base_url) {
            String::new()
        } else {
            API_VERSION.to_owned()
        };
        return (base_url, api_version, true);
    }
    (default_base_url.to_owned(), API_VERSION.to_owned(), api_key)
}

#[cfg(test)]
fn join_base_and_version(base_url: &str, api_version: &str) -> String {
    let mut value = format!("{}/", base_url.trim_end_matches('/'));
    if !api_version.is_empty() {
        value.push_str(api_version.trim_matches('/'));
        value.push('/');
    }
    value
}

fn setup_error_stream(model: &Model, message: String) -> AssistantMessageEventStream {
    let mut output = crate::types::AssistantMessage::pending(
        model.api.clone(),
        model.provider.clone(),
        model.id.clone(),
        now_millis(),
    );
    output.stop_reason = StopReason::Error;
    output.error_message = Some(message);
    AssistantMessageEventStream::from_events(vec![AssistantMessageEvent::Error {
        reason: ErrorStopReason::Error,
        error: output,
    }])
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests;
