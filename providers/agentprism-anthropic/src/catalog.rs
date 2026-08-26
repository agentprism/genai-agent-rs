//! Pinned Anthropic model catalog conversion into typed `pi-ai` descriptors.

use agentprism_ai::{
    AnthropicEffort, AnthropicFallbackModel, AnthropicMessagesCompat, AnthropicMessagesModelConfig,
    AnthropicThinkingValue, ApiModelConfig, CacheWriteRetentionPricing, CommonModelDescriptor,
    HeaderMapSpec, LevelSupport, Modality, ModalityCapabilities, ModelDescriptor, ModelId,
    ModelLimits, ModelPricing, ModelRef, MoneyRate, ProviderId, ThinkingLevelMap, TokenPriceRates,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use url::Url;

const ANTHROPIC_CATALOG: &str = include_str!("../data/anthropic.json");

/// Parses the pinned generated Anthropic catalog.
pub fn anthropic_models() -> Result<Vec<ModelDescriptor>, AnthropicCatalogError> {
    parse_anthropic_published_catalog(ANTHROPIC_CATALOG)
}

/// Parses the Anthropic Messages family from Pi's generated provider data.
pub fn parse_anthropic_published_catalog(
    source: &str,
) -> Result<Vec<ModelDescriptor>, AnthropicCatalogError> {
    let root: Value = serde_json::from_str(source)
        .map_err(|error| AnthropicCatalogError::new(format!("invalid catalog JSON: {error}")))?;
    let families = object(&root, "catalog root")?;
    let models = families
        .get("anthropic-messages")
        .ok_or_else(|| AnthropicCatalogError::new("catalog omits anthropic-messages"))?;
    object(models, "anthropic-messages catalog")?
        .values()
        .map(parse_model)
        .collect()
}

fn parse_model(value: &Value) -> Result<ModelDescriptor, AnthropicCatalogError> {
    let model = object(value, "model")?;
    let id = string(model, "id")?;
    let provider = string(model, "provider")?;
    if string(model, "api")? != "anthropic-messages" {
        return Err(AnthropicCatalogError::new(format!(
            "model {id} uses the wrong API family"
        )));
    }
    let mut input = BTreeSet::new();
    for modality in array(model, "input")? {
        input.insert(match modality.as_str() {
            Some("text") => Modality::Text,
            Some("image") => Modality::Image,
            Some("audio") => Modality::Audio,
            _ => {
                return Err(AnthropicCatalogError::new(format!(
                    "model {id} has an invalid input modality"
                )));
            }
        });
    }
    let mut output = BTreeSet::new();
    output.insert(Modality::Text);
    let cost = object(
        model
            .get("cost")
            .ok_or_else(|| AnthropicCatalogError::new(format!("model {id} omits cost")))?,
        "cost",
    )?;
    let pricing = parse_pricing(cost)?;
    let compatibility = parse_compat(model.get("compat"), id)?;
    Ok(ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, id),
            display_name: string(model, "name")?.to_owned(),
            base_url: Url::parse(string(model, "baseUrl")?).map_err(|error| {
                AnthropicCatalogError::new(format!("invalid URL for {id}: {error}"))
            })?,
            modalities: ModalityCapabilities { input, output },
            limits: ModelLimits {
                context_window: unsigned(model, "contextWindow")?,
                max_output_tokens: u32::try_from(unsigned(model, "maxTokens")?).map_err(|_| {
                    AnthropicCatalogError::new(format!("maxTokens overflows u32 for {id}"))
                })?,
            },
            pricing,
            reasoning: boolean(model, "reasoning")?,
            headers: parse_headers(model.get("headers"), id)?,
        },
        api: ApiModelConfig::AnthropicMessages(AnthropicMessagesModelConfig {
            compat: compatibility,
            thinking_levels: parse_thinking_levels(model.get("thinkingLevelMap"), id)?,
        }),
        extensions: Default::default(),
    })
}

fn parse_headers(
    value: Option<&Value>,
    model_id: &str,
) -> Result<HeaderMapSpec, AnthropicCatalogError> {
    let Some(value) = value else {
        return Ok(HeaderMapSpec::new());
    };
    object(value, "model headers")?
        .iter()
        .map(|(name, value)| {
            let value = value.as_str().ok_or_else(|| {
                AnthropicCatalogError::new(format!(
                    "model {model_id} header {name} is not a string"
                ))
            })?;
            Ok((name.clone(), Some(value.to_owned())))
        })
        .collect()
}

fn parse_pricing(cost: &Map<String, Value>) -> Result<ModelPricing, AnthropicCatalogError> {
    let cache_write = money_rate(cost, "cacheWrite")?;
    let input = money_rate(cost, "input")?;
    let one_hour = input
        .0
        .checked_mul(2)
        .map(MoneyRate::new)
        .ok_or_else(|| AnthropicCatalogError::new("one-hour cache-write price overflow"))?;
    Ok(ModelPricing {
        default: TokenPriceRates {
            input,
            output: money_rate(cost, "output")?,
            cache_read: money_rate(cost, "cacheRead")?,
            cache_write,
        },
        request_wide_tiers: Vec::new(),
        cache_write_retention: CacheWriteRetentionPricing {
            short: None,
            one_hour: Some(one_hour),
        },
    })
}

fn parse_compat(
    value: Option<&Value>,
    model_id: &str,
) -> Result<AnthropicMessagesCompat, AnthropicCatalogError> {
    let Some(value) = value else {
        return Ok(AnthropicMessagesCompat::default());
    };
    let compatibility = object(value, "compat")?;
    Ok(AnthropicMessagesCompat {
        supports_eager_tool_input_streaming: optional_bool(
            compatibility,
            "supportsEagerToolInputStreaming",
            model_id,
        )?,
        supports_long_cache_retention: optional_bool(
            compatibility,
            "supportsLongCacheRetention",
            model_id,
        )?,
        send_session_affinity_headers: optional_bool(
            compatibility,
            "sendSessionAffinityHeaders",
            model_id,
        )?,
        supports_cache_control_on_tools: optional_bool(
            compatibility,
            "supportsCacheControlOnTools",
            model_id,
        )?,
        supports_temperature: optional_bool(compatibility, "supportsTemperature", model_id)?,
        force_adaptive_thinking: optional_bool(compatibility, "forceAdaptiveThinking", model_id)?,
        allow_empty_signature: optional_bool(compatibility, "allowEmptySignature", model_id)?,
        supports_strict_tools: optional_bool(compatibility, "supportsStrictTools", model_id)?,
        supports_tool_references: optional_bool(compatibility, "supportsToolReferences", model_id)?,
        allowed_fallback_models: compatibility
            .get("allowedFallbackModels")
            .map(parse_fallbacks)
            .transpose()?
            .unwrap_or_default(),
        extensions: Default::default(),
    })
}

fn parse_fallbacks(value: &Value) -> Result<Vec<AnthropicFallbackModel>, AnthropicCatalogError> {
    value
        .as_array()
        .ok_or_else(|| AnthropicCatalogError::new("allowedFallbackModels is not an array"))?
        .iter()
        .map(|value| {
            let fallback = object(value, "fallback")?;
            let cost = object(
                fallback
                    .get("cost")
                    .ok_or_else(|| AnthropicCatalogError::new("fallback omits cost"))?,
                "fallback cost",
            )?;
            Ok(AnthropicFallbackModel {
                provider: ProviderId::new(string(fallback, "provider")?),
                model: ModelId::new(string(fallback, "model")?),
                cost: parse_pricing(cost)?,
            })
        })
        .collect()
}

fn parse_thinking_levels(
    value: Option<&Value>,
    model_id: &str,
) -> Result<ThinkingLevelMap<AnthropicThinkingValue>, AnthropicCatalogError> {
    let Some(value) = value else {
        return Ok(ThinkingLevelMap::default());
    };
    let levels = object(value, "thinkingLevelMap")?;
    let parse = |name| parse_thinking_level(levels.get(name), model_id, name);
    Ok(ThinkingLevelMap {
        off: parse("off")?,
        minimal: parse("minimal")?,
        low: parse("low")?,
        medium: parse("medium")?,
        high: parse("high")?,
        xhigh: parse("xhigh")?,
        max: parse("max")?,
    })
}

fn parse_thinking_level(
    value: Option<&Value>,
    model_id: &str,
    level: &str,
) -> Result<Option<LevelSupport<AnthropicThinkingValue>>, AnthropicCatalogError> {
    match value {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(LevelSupport::Unsupported)),
        Some(Value::String(value)) => Ok(Some(LevelSupport::Value(if value == "off" {
            AnthropicThinkingValue::Off
        } else {
            AnthropicThinkingValue::Effort(parse_effort(value).ok_or_else(|| {
                AnthropicCatalogError::new(format!(
                    "invalid Anthropic effort {value} for {model_id} level {level}"
                ))
            })?)
        }))),
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(|value| Some(LevelSupport::Value(AnthropicThinkingValue::Budget(value))))
            .ok_or_else(|| {
                AnthropicCatalogError::new(format!(
                    "invalid thinking budget for {model_id} level {level}"
                ))
            }),
        Some(_) => Err(AnthropicCatalogError::new(format!(
            "invalid thinking mapping for {model_id} level {level}"
        ))),
    }
}

fn parse_effort(value: &str) -> Option<AnthropicEffort> {
    match value {
        "minimal" => Some(AnthropicEffort::Minimal),
        "low" => Some(AnthropicEffort::Low),
        "medium" => Some(AnthropicEffort::Medium),
        "high" => Some(AnthropicEffort::High),
        "xhigh" => Some(AnthropicEffort::Xhigh),
        "max" => Some(AnthropicEffort::Max),
        _ => None,
    }
}

fn money_rate(cost: &Map<String, Value>, name: &str) -> Result<MoneyRate, AnthropicCatalogError> {
    let number = cost
        .get(name)
        .and_then(Value::as_number)
        .ok_or_else(|| AnthropicCatalogError::new(format!("catalog cost omits {name}")))?;
    decimal_dollars_per_million_to_rate(&number.to_string())
}

fn decimal_dollars_per_million_to_rate(value: &str) -> Result<MoneyRate, AnthropicCatalogError> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned));
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty() || fraction.len() > 6 || value.contains(['e', 'E']) {
        return Err(AnthropicCatalogError::new(format!(
            "catalog price is not a signed six-place decimal: {value}"
        )));
    }
    let whole = whole
        .parse::<u128>()
        .map_err(|_| AnthropicCatalogError::new(format!("invalid catalog price: {value}")))?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u128>()
            .map_err(|_| AnthropicCatalogError::new(format!("invalid catalog price: {value}")))?
            * 10_u128.pow(u32::try_from(6 - fraction.len()).unwrap_or_default())
    };
    let magnitude = whole
        .checked_mul(1_000_000)
        .and_then(|whole| whole.checked_add(fraction))
        .ok_or_else(|| AnthropicCatalogError::new(format!("catalog price overflow: {value}")))?;
    let micros = i128::try_from(magnitude)
        .map_err(|_| AnthropicCatalogError::new(format!("catalog price overflow: {value}")))?;
    Ok(MoneyRate::new(if negative { -micros } else { micros }))
}

fn object<'a>(
    value: &'a Value,
    description: &str,
) -> Result<&'a Map<String, Value>, AnthropicCatalogError> {
    value
        .as_object()
        .ok_or_else(|| AnthropicCatalogError::new(format!("{description} is not an object")))
}

fn string<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a str, AnthropicCatalogError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| AnthropicCatalogError::new(format!("catalog field {name} is not a string")))
}

fn array<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Vec<Value>, AnthropicCatalogError> {
    object
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| AnthropicCatalogError::new(format!("catalog field {name} is not an array")))
}

fn unsigned(object: &Map<String, Value>, name: &str) -> Result<u64, AnthropicCatalogError> {
    object
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| AnthropicCatalogError::new(format!("catalog field {name} is not unsigned")))
}

fn boolean(object: &Map<String, Value>, name: &str) -> Result<bool, AnthropicCatalogError> {
    object
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| AnthropicCatalogError::new(format!("catalog field {name} is not boolean")))
}

fn optional_bool(
    object: &Map<String, Value>,
    name: &str,
    model_id: &str,
) -> Result<Option<bool>, AnthropicCatalogError> {
    object
        .get(name)
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                AnthropicCatalogError::new(format!("compat {name} is not boolean for {model_id}"))
            })
        })
        .transpose()
}

/// Pinned Anthropic catalog conversion error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AnthropicCatalogError {
    message: String,
}

impl AnthropicCatalogError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AnthropicCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AnthropicCatalogError {}
