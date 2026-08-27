//! OpenRouter provider leaf backed by the shared OpenAI Completions family.

#![deny(missing_docs)]

mod oauth;

use agentprism_ai::{
    ApiId, CacheWriteRetentionPricing, HeaderMapSpec, ImageModality, ImageModelDescriptor,
    ModelPricing, ModelRef, MoneyRate, RequestWidePriceTier, TokenPriceRates,
};
use serde_json::{Map, Value};
use std::rc::Rc;
use std::sync::Arc;

pub use agentprism_provider_common::{LocalProviderInputs, ProviderBuildError, ProviderInputs};
pub use oauth::{LocalOpenRouterOAuth, OpenRouterOAuth};

/// Returns the pinned OpenRouter catalog owned by this leaf.
pub fn models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    agentprism_openai::parse_openai_published_catalog(
        include_str!("../data/models.json"),
        "openrouter",
        "openai-completions",
    )
    .map_err(ProviderBuildError::catalog)
}

/// Compatibility name for the leaf-owned OpenRouter catalog.
pub fn openrouter_models() -> Result<Vec<agentprism_ai::ModelDescriptor>, ProviderBuildError> {
    models()
}

/// Returns the pinned image-model catalog published by OpenRouter through Pi.
pub fn image_models() -> Result<Vec<ImageModelDescriptor>, ProviderBuildError> {
    parse_image_models_response(include_str!("../data/image-models.json"))
}

/// Compatibility name for the OpenRouter image-generation catalog.
pub fn openrouter_image_models() -> Result<Vec<ImageModelDescriptor>, ProviderBuildError> {
    image_models()
}

/// Parses the strict OpenRouter image-model response used by Pi's generator.
pub fn parse_image_models_response(
    source: &str,
) -> Result<Vec<ImageModelDescriptor>, ProviderBuildError> {
    let root: Value = serde_json::from_str(source).map_err(ProviderBuildError::catalog)?;
    let data = root
        .as_object()
        .and_then(|root| root.get("data"))
        .and_then(Value::as_array)
        .filter(|data| !data.is_empty())
        .ok_or_else(|| ProviderBuildError::catalog("missing or empty image model list"))?;
    let models = data
        .iter()
        .filter_map(|value| parse_image_model(value).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    if models.is_empty() {
        return Err(ProviderBuildError::catalog("no usable image models"));
    }
    Ok(models)
}

fn parse_image_model(value: &Value) -> Result<Option<ImageModelDescriptor>, ProviderBuildError> {
    let model = value
        .as_object()
        .ok_or_else(|| ProviderBuildError::catalog("image model entry is not an object"))?;
    let architecture = model.get("architecture").and_then(Value::as_object);
    let input = parse_modalities(architecture.and_then(|value| value.get("input_modalities")));
    let output = parse_modalities(architecture.and_then(|value| value.get("output_modalities")));
    if !output.contains(&ImageModality::Image) {
        return Ok(None);
    }
    let id = image_string(model, "id")?;
    let pricing = model.get("pricing").and_then(Value::as_object);
    Ok(Some(ImageModelDescriptor {
        model_ref: ModelRef::new("openrouter", id),
        display_name: image_string(model, "name")?.to_owned(),
        api: ApiId::new(agentprism_ai::OPENROUTER_IMAGES_API_ID),
        base_url: url::Url::parse("https://openrouter.ai/api/v1")
            .map_err(ProviderBuildError::configuration)?,
        input: if input.is_empty() {
            vec![ImageModality::Text]
        } else {
            input
        },
        output,
        pricing: ModelPricing {
            default: TokenPriceRates {
                input: image_money_rate(pricing, "prompt")?,
                output: image_money_rate(pricing, "completion")?,
                cache_read: image_money_rate(pricing, "input_cache_read")?,
                cache_write: image_money_rate(pricing, "input_cache_write")?,
            },
            request_wide_tiers: Vec::<RequestWidePriceTier>::new(),
            cache_write_retention: CacheWriteRetentionPricing::default(),
        },
        headers: HeaderMapSpec::new(),
    }))
}

fn image_string<'a>(
    model: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, ProviderBuildError> {
    model
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderBuildError::catalog(format!("image model omits string {field}")))
}

fn parse_modalities(value: Option<&Value>) -> Vec<ImageModality> {
    let mut modalities = Vec::new();
    for modality in value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| match value.as_str() {
            Some("text") => Some(ImageModality::Text),
            Some("image") => Some(ImageModality::Image),
            _ => None,
        })
    {
        if !modalities.contains(&modality) {
            modalities.push(modality);
        }
    }
    modalities
}

fn image_money_rate(
    pricing: Option<&Map<String, Value>>,
    field: &str,
) -> Result<MoneyRate, ProviderBuildError> {
    let source = pricing
        .and_then(|pricing| pricing.get(field))
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_number().map(ToString::to_string))
        })
        .unwrap_or_else(|| "0".to_owned());
    decimal_dollars_per_token_to_micros_per_million(&source)
        .map(MoneyRate::new)
        .ok_or_else(|| ProviderBuildError::catalog(format!("invalid image cost {field}: {source}")))
}

fn decimal_dollars_per_token_to_micros_per_million(value: &str) -> Option<i128> {
    let value = value.trim();
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned));
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let (significand, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((significand, exponent)) => (significand, exponent.parse::<i32>().ok()?),
        None => (unsigned, 0_i32),
    };
    let (whole, fraction) = significand.split_once('.').unwrap_or((significand, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.chars().all(|character| character.is_ascii_digit())
        || !fraction.chars().all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let digits = format!("{whole}{fraction}");
    let magnitude = digits.parse::<i128>().ok()?;
    let shift = 12_i32
        .checked_add(exponent)?
        .checked_sub(i32::try_from(fraction.len()).ok()?)?;
    let scaled = if shift >= 0 {
        magnitude.checked_mul(10_i128.checked_pow(u32::try_from(shift).ok()?)?)?
    } else {
        magnitude / 10_i128.checked_pow(shift.unsigned_abs())?
    };
    Some(if negative { -scaled } else { scaled })
}

/// Builds a Send OpenRouter registration around a caller-shared family API.
pub fn provider_with_api(
    api: Arc<dyn agentprism_ai::ChatApi>,
    oauth_transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let oauth = Arc::new(OpenRouterOAuth::new(Arc::clone(&oauth_transport)));
    agentprism_ai::ProviderRegistration::builder("openrouter")
        .display_name("OpenRouter")
        .auth(agentprism_provider_common::bearer_auth(
            "OpenRouter API key",
            "OPENROUTER_API_KEY",
            Some(oauth),
        ))
        .models(models()?)
        .api(ApiId::new("openai-completions"), api)
        .image_models(image_models()?)
        .image_api(
            agentprism_ai::OPENROUTER_IMAGES_API_ID,
            Arc::new(agentprism_ai::OpenRouterImagesApi::new(oauth_transport)),
        )
        .build()
        .map_err(ProviderBuildError::Registration)
}

/// Compatibility name for a caller-shared OpenRouter family API.
pub fn openrouter_provider_with_api(
    api: Arc<dyn agentprism_ai::ChatApi>,
    oauth_transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider_with_api(api, oauth_transport)
}

/// Builds the Send OpenRouter registration.
pub fn provider(
    inputs: ProviderInputs,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::openai_completions_api(Arc::clone(&inputs.http));
    provider_with_api(api, inputs.http)
}

/// Builds OpenRouter directly from a raw Send transport.
pub fn openrouter_provider(
    transport: Arc<dyn agentprism_ai::HttpTransport>,
) -> Result<agentprism_ai::ProviderRegistration, ProviderBuildError> {
    provider(ProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}

/// Builds a local OpenRouter registration around a caller-shared family API.
pub fn local_provider_with_api(
    api: Rc<dyn agentprism_ai::LocalChatApi>,
    oauth_transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let oauth = Rc::new(LocalOpenRouterOAuth::new(Rc::clone(&oauth_transport)));
    agentprism_ai::LocalProviderRegistration::builder("openrouter")
        .display_name("OpenRouter")
        .auth(agentprism_provider_common::local_bearer_auth(
            "OpenRouter API key",
            "OPENROUTER_API_KEY",
            Some(oauth),
        ))
        .models(models()?)
        .api(ApiId::new("openai-completions"), api)
        .image_models(image_models()?)
        .image_api(
            agentprism_ai::OPENROUTER_IMAGES_API_ID,
            Rc::new(agentprism_ai::LocalOpenRouterImagesApi::new(
                oauth_transport,
            )),
        )
        .build()
        .map_err(ProviderBuildError::Registration)
}

/// Compatibility name for a caller-shared local OpenRouter family API.
pub fn local_openrouter_provider_with_api(
    api: Rc<dyn agentprism_ai::LocalChatApi>,
    oauth_transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider_with_api(api, oauth_transport)
}

/// Builds the local OpenRouter registration.
pub fn local_provider(
    inputs: LocalProviderInputs,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    let api = agentprism_openai::local_openai_completions_api(Rc::clone(&inputs.http));
    local_provider_with_api(api, inputs.http)
}

/// Builds OpenRouter directly from a raw local transport.
pub fn local_openrouter_provider(
    transport: Rc<dyn agentprism_ai::LocalHttpTransport>,
) -> Result<agentprism_ai::LocalProviderRegistration, ProviderBuildError> {
    local_provider(LocalProviderInputs {
        http: transport,
        environment: Default::default(),
    })
}
