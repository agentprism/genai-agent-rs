//! Radius dynamic catalog and pi-messages provider leaf crate.

#![deny(missing_docs)]

mod oauth;

use futures_util::StreamExt;
use http::{HeaderMap, HeaderValue, Method, header};
use pi_ai::{
    ApiFamily, ApiId, ApiModelConfig, AuthError, AuthInteraction, AuthResolver,
    CacheWriteRetentionPricing, CancellationToken, CatalogCandidate, CatalogError,
    CatalogFetchContext, CommonModelDescriptor, CustomApiModelConfig, EnvironmentApiKeyAuth,
    ExtensionMap, HeaderMapSpec, HttpRequest, HttpTransport, LocalAuthInteraction,
    LocalAuthResolver, LocalBoxFuture, LocalHttpTransport, LocalModelCatalogSource, LocalOAuthAuth,
    LocalProviderAuthResolver, LocalProviderRegistration, LocalResolveAuthRequest, Modality,
    ModalityCapabilities, ModelCatalogSource, ModelDescriptor, ModelLimits, ModelPricing, ModelRef,
    MoneyRate, OAuthAuth, ProviderAuthResolver, ProviderRegistration, ProviderRegistrationError,
    ResolveAuthRequest, ResolvedAuth, SendBoxFuture, Timestamp, TokenPriceRates, trim_ecmascript,
};
use serde::Deserialize;
use serde_json::{Value, value::RawValue};
use std::collections::BTreeSet;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

pub use oauth::{LocalRadiusOAuth, RadiusOAuth};

/// Pinned default Radius gateway.
pub const DEFAULT_RADIUS_GATEWAY: &str = "https://radius.pi.dev";

/// One model entry returned by `/v1/config`.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayModel {
    /// Gateway model identifier.
    pub id: String,
    /// Human-readable model name.
    pub name: String,
    /// Whether reasoning is supported.
    pub reasoning: bool,
    /// Optional per-level mapping.
    #[serde(default)]
    pub thinking_level_map: serde_json::Value,
    /// Accepted input modalities.
    pub input: Vec<String>,
    /// Published per-million-token rates.
    pub cost: RadiusCost,
    /// Context window.
    pub context_window: u64,
    /// Maximum output tokens.
    pub max_tokens: u32,
}

/// Radius published token rates, represented as JSON numbers until converted
/// losslessly to integer micro-rates.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusCost {
    /// Input rate.
    pub input: serde_json::Number,
    /// Output rate.
    pub output: serde_json::Number,
    /// Cache-read rate.
    pub cache_read: serde_json::Number,
    /// Cache-write rate.
    pub cache_write: serde_json::Number,
}

/// Complete Radius gateway configuration.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RadiusGatewayConfig {
    /// Base URL used for pi-messages requests.
    pub base_url: Url,
    /// Complete gateway-owned model set.
    pub models: Vec<RadiusGatewayModel>,
}

/// Converts a sanitized Radius config into canonical model descriptors.
pub fn models_from_config(
    provider: &str,
    config: &RadiusGatewayConfig,
) -> Result<Vec<ModelDescriptor>, CatalogError> {
    config
        .models
        .iter()
        .map(|model| {
            let input = model
                .input
                .iter()
                .filter_map(|modality| match modality.as_str() {
                    "text" => Some(Modality::Text),
                    "image" => Some(Modality::Image),
                    "audio" => Some(Modality::Audio),
                    _ => None,
                })
                .collect::<BTreeSet<_>>();
            let custom = serde_json::json!({
                "thinking_levels": model.thinking_level_map,
            });
            Ok(ModelDescriptor {
                common: CommonModelDescriptor {
                    model_ref: ModelRef::new(provider, model.id.clone()),
                    display_name: model.name.clone(),
                    base_url: config.base_url.clone(),
                    modalities: ModalityCapabilities {
                        input,
                        output: BTreeSet::from([Modality::Text]),
                    },
                    limits: ModelLimits {
                        context_window: model.context_window,
                        max_output_tokens: model.max_tokens,
                    },
                    pricing: ModelPricing {
                        default: TokenPriceRates {
                            input: rate(&model.cost.input)?,
                            output: rate(&model.cost.output)?,
                            cache_read: rate(&model.cost.cache_read)?,
                            cache_write: rate(&model.cost.cache_write)?,
                        },
                        request_wide_tiers: Vec::new(),
                        cache_write_retention: CacheWriteRetentionPricing::default(),
                    },
                    reasoning: model.reasoning,
                    headers: HeaderMapSpec::new(),
                },
                api: ApiModelConfig::Custom(CustomApiModelConfig {
                    api: ApiId::new(pi_ai_pi_messages::PiMessages::API_ID),
                    schema_version: 1,
                    value: RawValue::from_string(custom.to_string()).map_err(|error| {
                        CatalogError::validation(format!("invalid Radius custom config: {error}"))
                    })?,
                }),
                extensions: ExtensionMap::new(),
            })
        })
        .collect()
}

fn rate(value: &serde_json::Number) -> Result<MoneyRate, CatalogError> {
    decimal_micros(&value.to_string())
        .map(MoneyRate::new)
        .ok_or_else(|| CatalogError::validation(format!("invalid Radius price {value}")))
}

fn decimal_micros(value: &str) -> Option<i128> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole = whole.parse::<i128>().ok()?;
    let mut fraction = fraction.chars().take(6).collect::<String>();
    while fraction.len() < 6 {
        fraction.push('0');
    }
    whole
        .checked_mul(1_000_000)?
        .checked_add(fraction.parse::<i128>().unwrap_or(0))
}

/// Send-capable Radius dynamic catalog source.
#[derive(Clone)]
pub struct RadiusCatalogSource {
    provider: String,
    gateway: Url,
    transport: Arc<dyn HttpTransport>,
}

impl RadiusCatalogSource {
    /// Creates a source for one provider/gateway pair.
    pub fn new(
        provider: impl Into<String>,
        gateway: Url,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            provider: provider.into(),
            gateway,
            transport,
        }
    }
}

impl ModelCatalogSource for RadiusCatalogSource {
    fn baseline(&self) -> Arc<[ModelDescriptor]> {
        Arc::from(Vec::new())
    }

    fn fetch(
        &self,
        context: CatalogFetchContext,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<CatalogCandidate, CatalogError>> {
        Box::pin(async move {
            let gateway = context.auth.base_url.as_ref().unwrap_or(&self.gateway);
            let request = config_request(gateway, &context.auth);
            let response = self
                .transport
                .execute(request, cancellation.clone())
                .await
                .map_err(|error| CatalogError::source(error.message))?;
            let status = response.status;
            let body = collect_send(response.body, &cancellation).await?;
            candidate(&self.provider, gateway, status, &body)
        })
    }
}

/// Local-executor Radius dynamic catalog source.
#[derive(Clone)]
pub struct LocalRadiusCatalogSource {
    provider: String,
    gateway: Url,
    transport: Rc<dyn LocalHttpTransport>,
}

impl LocalRadiusCatalogSource {
    /// Creates a local source for one provider/gateway pair.
    pub fn new(
        provider: impl Into<String>,
        gateway: Url,
        transport: Rc<dyn LocalHttpTransport>,
    ) -> Self {
        Self {
            provider: provider.into(),
            gateway,
            transport,
        }
    }
}

impl LocalModelCatalogSource for LocalRadiusCatalogSource {
    fn baseline(&self) -> Rc<[ModelDescriptor]> {
        Rc::from(Vec::new())
    }

    fn fetch(
        &self,
        context: CatalogFetchContext,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<CatalogCandidate, CatalogError>> {
        Box::pin(async move {
            let gateway = context.auth.base_url.as_ref().unwrap_or(&self.gateway);
            let request = config_request(gateway, &context.auth);
            let response = self
                .transport
                .execute(request, cancellation.clone())
                .await
                .map_err(|error| CatalogError::source(error.message))?;
            let status = response.status;
            let body = collect_local(response.body, &cancellation).await?;
            candidate(&self.provider, gateway, status, &body)
        })
    }
}

fn config_request(gateway: &Url, auth: &ResolvedAuth) -> HttpRequest {
    let mut url = gateway.clone();
    url.set_path("/v1/config");
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
    if let Some(value) = auth.headers.get(header::AUTHORIZATION) {
        headers.insert(header::AUTHORIZATION, value.clone());
    }
    HttpRequest {
        method: Method::GET,
        url,
        auth_headers: headers.clone(),
        headers,
        session_id: None,
        body: Vec::new(),
        timeout: None,
        transport: None,
        websocket_connect_timeout: None,
        attempt: 0,
    }
}

async fn collect_send(
    mut body: pi_ai::HttpBody,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, CatalogError> {
    let mut result = Vec::new();
    while let Some(chunk) = body.next().await {
        cancellation
            .check()
            .map_err(|_| CatalogError::source("Radius catalog refresh cancelled"))?;
        result.extend(chunk.map_err(|error| CatalogError::source(error.message))?);
    }
    Ok(result)
}

async fn collect_local(
    mut body: pi_ai::LocalHttpBody,
    cancellation: &CancellationToken,
) -> Result<Vec<u8>, CatalogError> {
    let mut result = Vec::new();
    while let Some(chunk) = body.next().await {
        cancellation
            .check()
            .map_err(|_| CatalogError::source("Radius catalog refresh cancelled"))?;
        result.extend(chunk.map_err(|error| CatalogError::source(error.message))?);
    }
    Ok(result)
}

fn candidate(
    provider: &str,
    gateway: &Url,
    status: u16,
    body: &[u8],
) -> Result<CatalogCandidate, CatalogError> {
    if !(200..300).contains(&status) {
        let detail = truncate_http_body(&String::from_utf8_lossy(body));
        return Err(CatalogError::source(format!(
            "Could not load Radius config from {}: {status}: {detail}",
            gateway.as_str().trim_end_matches('/')
        )));
    }
    let config = sanitize_radius_gateway_config(body).ok_or_else(|| {
        CatalogError::validation(format!(
            "Invalid Radius config from {}",
            gateway.as_str().trim_end_matches('/')
        ))
    })?;
    Ok(CatalogCandidate {
        models: models_from_config(provider, &config)?,
        checked_at: now(),
        revision: None,
        etag: None,
        source_metadata: ExtensionMap::new(),
    })
}

fn sanitize_radius_gateway_config(body: &[u8]) -> Option<RadiusGatewayConfig> {
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    let base_url = Url::parse(object.get("baseUrl")?.as_str()?).ok()?;
    let models = object
        .get("models")?
        .as_array()?
        .iter()
        .filter(|value| is_radius_gateway_model(value))
        .filter_map(|value| serde_json::from_value(value.clone()).ok())
        .collect();
    Some(RadiusGatewayConfig { base_url, models })
}

fn is_radius_gateway_model(value: &Value) -> bool {
    let Some(model) = value.as_object() else {
        return false;
    };
    model.get("id").is_some_and(Value::is_string)
        && model.get("name").is_some_and(Value::is_string)
        && model.get("reasoning").is_some_and(Value::is_boolean)
        && model.get("input").is_some_and(Value::is_array)
        && model
            .get("cost")
            .is_some_and(|cost| cost.is_object() && !cost.is_array())
        && model.get("contextWindow").is_some_and(Value::is_number)
        && model.get("maxTokens").is_some_and(Value::is_number)
}

fn truncate_http_body(body: &str) -> String {
    let trimmed = trim_ecmascript(body);
    if trimmed.encode_utf16().count() <= 512 {
        return trimmed.to_owned();
    }
    let mut result = String::new();
    let mut utf16_units = 0;
    for character in trimmed.chars() {
        let units = character.len_utf16();
        if utf16_units + units > 512 {
            break;
        }
        result.push(character);
        utf16_units += units;
    }
    result.push('…');
    result
}

fn now() -> Timestamp {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Timestamp::from_unix_millis(i64::try_from(millis).unwrap_or(i64::MAX))
}

struct RadiusAuth {
    inner: ProviderAuthResolver,
}

impl AuthResolver for RadiusAuth {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let configuration_check =
                request.purpose == pi_ai::AuthResolutionPurpose::ConfigurationCheck;
            let retention_cancellation = cancellation.clone();
            let explicit_retention = request
                .overrides
                .environment
                .get("PI_CACHE_RETENTION")
                .cloned();
            let auth_context = Arc::clone(&request.auth_context);
            let Some(mut auth) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            if configuration_check {
                return Ok(Some(auth));
            }
            insert_bearer(&mut auth)?;
            let retention = match explicit_retention {
                Some(value) => Some(value),
                None => {
                    auth_context
                        .env("PI_CACHE_RETENTION".into(), retention_cancellation)
                        .await?
                }
            };
            if retention.as_deref() == Some("long") {
                auth.headers.insert(
                    pi_ai_pi_messages::PI_CACHE_RETENTION_AUTH_HEADER,
                    HeaderValue::from_static("long"),
                );
            }
            Ok(Some(auth))
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

struct LocalRadiusAuth {
    inner: LocalProviderAuthResolver,
}

impl LocalAuthResolver for LocalRadiusAuth {
    fn resolve(
        &self,
        request: LocalResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            let configuration_check =
                request.purpose == pi_ai::AuthResolutionPurpose::ConfigurationCheck;
            let retention_cancellation = cancellation.clone();
            let explicit_retention = request
                .overrides
                .environment
                .get("PI_CACHE_RETENTION")
                .cloned();
            let auth_context = Rc::clone(&request.auth_context);
            let Some(mut auth) = self.inner.resolve(request, cancellation).await? else {
                return Ok(None);
            };
            if configuration_check {
                return Ok(Some(auth));
            }
            insert_bearer(&mut auth)?;
            let retention = match explicit_retention {
                Some(value) => Some(value),
                None => {
                    auth_context
                        .env("PI_CACHE_RETENTION".into(), retention_cancellation)
                        .await?
                }
            };
            if retention.as_deref() == Some("long") {
                auth.headers.insert(
                    pi_ai_pi_messages::PI_CACHE_RETENTION_AUTH_HEADER,
                    HeaderValue::from_static("long"),
                );
            }
            Ok(Some(auth))
        })
    }

    fn login(
        &self,
        interaction: Rc<dyn LocalAuthInteraction>,
        cancellation: CancellationToken,
    ) -> LocalBoxFuture<'_, Result<pi_ai::Credential, AuthError>> {
        self.inner.login(interaction, cancellation)
    }
}

fn insert_bearer(auth: &mut ResolvedAuth) -> Result<(), AuthError> {
    let Some(key) = auth.api_key.take() else {
        return Ok(());
    };
    auth.headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.expose_secret()))
            .map_err(|_| AuthError::new("invalid_api_key", "invalid Radius API key"))?,
    );
    Ok(())
}

/// Radius provider construction failure.
#[derive(Debug)]
pub enum RadiusProviderError {
    /// Invalid gateway URL.
    Url(url::ParseError),
    /// Invalid registration composition.
    Registration(ProviderRegistrationError),
}

impl fmt::Display for RadiusProviderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Url(error) => write!(formatter, "URL error: {error}"),
            Self::Registration(error) => write!(formatter, "registration error: {error}"),
        }
    }
}

impl std::error::Error for RadiusProviderError {}

/// Builds the dynamic Send Radius provider.
pub fn radius_provider(
    transport: Arc<dyn HttpTransport>,
) -> Result<ProviderRegistration, RadiusProviderError> {
    let gateway = Url::parse(DEFAULT_RADIUS_GATEWAY).map_err(RadiusProviderError::Url)?;
    let oauth: Arc<dyn OAuthAuth> =
        Arc::new(RadiusOAuth::new(Arc::clone(&transport), gateway.clone()));
    ProviderRegistration::builder("radius")
        .display_name("Radius")
        .base_url(gateway.clone())
        .auth(Arc::new(RadiusAuth {
            inner: ProviderAuthResolver::new(
                Some(Arc::new(EnvironmentApiKeyAuth::new(
                    "Radius API key",
                    ["RADIUS_API_KEY"],
                ))),
                Some(oauth),
            ),
        }))
        .catalog_source(Arc::new(RadiusCatalogSource::new(
            "radius",
            gateway,
            Arc::clone(&transport),
        )))
        .api(
            pi_ai_pi_messages::PiMessages::API_ID,
            pi_ai_pi_messages::pi_messages_api(transport),
        )
        .build()
        .map_err(RadiusProviderError::Registration)
}

/// Builds the dynamic local Radius provider.
pub fn local_radius_provider(
    transport: Rc<dyn LocalHttpTransport>,
) -> Result<LocalProviderRegistration, RadiusProviderError> {
    let gateway = Url::parse(DEFAULT_RADIUS_GATEWAY).map_err(RadiusProviderError::Url)?;
    let oauth: Rc<dyn LocalOAuthAuth> = Rc::new(LocalRadiusOAuth::new(
        Rc::clone(&transport),
        gateway.clone(),
    ));
    LocalProviderRegistration::builder("radius")
        .display_name("Radius")
        .base_url(gateway.clone())
        .auth(Rc::new(LocalRadiusAuth {
            inner: LocalProviderAuthResolver::new(
                Some(Rc::new(EnvironmentApiKeyAuth::new(
                    "Radius API key",
                    ["RADIUS_API_KEY"],
                ))),
                Some(oauth),
            ),
        }))
        .catalog_source(Rc::new(LocalRadiusCatalogSource::new(
            "radius",
            gateway,
            Rc::clone(&transport),
        )))
        .api(
            pi_ai_pi_messages::PiMessages::API_ID,
            pi_ai_pi_messages::local_pi_messages_api(transport),
        )
        .build()
        .map_err(RadiusProviderError::Registration)
}
