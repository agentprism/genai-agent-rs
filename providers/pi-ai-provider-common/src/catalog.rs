//! Parser support for published Google-family entries in mixed leaf catalogs.

use pi_ai::{
    ApiModelConfig, CacheWriteRetentionPricing, CommonModelDescriptor, GoogleModelConfig,
    HeaderMapSpec, LevelSupport, Modality, ModalityCapabilities, ModelDescriptor, ModelLimits,
    ModelPricing, ModelRef, MoneyRate, ThinkingLevelMap, TokenPriceRates,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use url::Url;

/// Failure while parsing the Google family from provider-owned published data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GooglePublishedCatalogError(String);

impl fmt::Display for GooglePublishedCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for GooglePublishedCatalogError {}

/// Parses `google-generative-ai` entries from one leaf-owned published file.
pub fn parse_google_published_catalog(
    source: &str,
    provider: &str,
) -> Result<Vec<ModelDescriptor>, GooglePublishedCatalogError> {
    let root: Value = serde_json::from_str(source).map_err(|error| {
        GooglePublishedCatalogError(format!("invalid {provider} catalog: {error}"))
    })?;
    let root = root.as_object().ok_or_else(|| {
        GooglePublishedCatalogError(format!("{provider} catalog is not an object"))
    })?;
    let family = root
        .get("google-generative-ai")
        .and_then(Value::as_object)
        .ok_or_else(|| GooglePublishedCatalogError("catalog omits google-generative-ai".into()))?;
    family
        .values()
        .map(|value| parse_model(value, provider))
        .collect()
}

fn parse_model(
    value: &Value,
    provider: &str,
) -> Result<ModelDescriptor, GooglePublishedCatalogError> {
    let model = value
        .as_object()
        .ok_or_else(|| GooglePublishedCatalogError("model is not an object".into()))?;
    let id = string(model, "id")?;
    let modalities = model
        .get("input")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|value| match value {
            "text" => Some(Modality::Text),
            "image" => Some(Modality::Image),
            "audio" => Some(Modality::Audio),
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let cost = model
        .get("cost")
        .and_then(Value::as_object)
        .ok_or_else(|| GooglePublishedCatalogError(format!("{id} omits cost")))?;
    Ok(ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, id),
            display_name: string(model, "name")?.into(),
            base_url: Url::parse(string(model, "baseUrl")?)
                .map_err(|error| GooglePublishedCatalogError(error.to_string()))?,
            modalities: ModalityCapabilities {
                input: modalities,
                output: BTreeSet::from([Modality::Text]),
            },
            limits: ModelLimits {
                context_window: unsigned(model, "contextWindow")?,
                max_output_tokens: u32::try_from(unsigned(model, "maxTokens")?)
                    .map_err(|_| GooglePublishedCatalogError(format!("{id} maxTokens")))?,
            },
            pricing: ModelPricing {
                default: TokenPriceRates {
                    input: price(cost, "input")?,
                    output: price(cost, "output")?,
                    cache_read: price(cost, "cacheRead")?,
                    cache_write: price(cost, "cacheWrite")?,
                },
                request_wide_tiers: Vec::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            reasoning: model
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            headers: parse_headers(model.get("headers"), id)?,
        },
        api: ApiModelConfig::GoogleGenerativeAi(GoogleModelConfig {
            thinking_levels: parse_string_levels(model.get("thinkingLevelMap")),
        }),
        extensions: Default::default(),
    })
}

fn parse_string_levels(value: Option<&Value>) -> ThinkingLevelMap<String> {
    let mut levels = ThinkingLevelMap::default();
    let Some(value) = value.and_then(Value::as_object) else {
        return levels;
    };
    for (name, target) in [
        ("off", &mut levels.off),
        ("minimal", &mut levels.minimal),
        ("low", &mut levels.low),
        ("medium", &mut levels.medium),
        ("high", &mut levels.high),
        ("xhigh", &mut levels.xhigh),
        ("max", &mut levels.max),
    ] {
        if let Some(value) = value.get(name) {
            *target = Some(match value.as_str() {
                Some(value) => LevelSupport::Value(value.into()),
                None if value.is_null() => LevelSupport::Disabled,
                None => LevelSupport::Unsupported,
            });
        }
    }
    levels
}

fn parse_headers(
    value: Option<&Value>,
    model_id: &str,
) -> Result<HeaderMapSpec, GooglePublishedCatalogError> {
    let Some(value) = value else {
        return Ok(HeaderMapSpec::new());
    };
    value
        .as_object()
        .ok_or_else(|| GooglePublishedCatalogError(format!("{model_id} headers is not an object")))?
        .iter()
        .map(|(name, value)| {
            let value = value.as_str().ok_or_else(|| {
                GooglePublishedCatalogError(format!("{model_id} header {name} is not a string"))
            })?;
            Ok((name.clone(), Some(value.to_owned())))
        })
        .collect()
}

fn price(cost: &Map<String, Value>, field: &str) -> Result<MoneyRate, GooglePublishedCatalogError> {
    let source = cost
        .get(field)
        .map(Value::to_string)
        .unwrap_or_else(|| "0".into());
    decimal_micros(&source)
        .map(MoneyRate::new)
        .ok_or_else(|| GooglePublishedCatalogError(format!("invalid cost {field}: {source}")))
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

fn string<'a>(
    model: &'a Map<String, Value>,
    field: &str,
) -> Result<&'a str, GooglePublishedCatalogError> {
    model
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| GooglePublishedCatalogError(format!("model omits string {field}")))
}

fn unsigned(model: &Map<String, Value>, field: &str) -> Result<u64, GooglePublishedCatalogError> {
    model
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| GooglePublishedCatalogError(format!("model omits unsigned {field}")))
}
