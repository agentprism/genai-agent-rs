//! Pinned Mistral published model catalog.

use pi_ai::{
    ApiModelConfig, CacheWriteRetentionPricing, CommonModelDescriptor, HeaderMapSpec, LevelSupport,
    MistralModelConfig, Modality, ModalityCapabilities, ModelDescriptor, ModelLimits, ModelPricing,
    ModelRef, MoneyRate, ThinkingLevelMap, TokenPriceRates,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use url::Url;

/// Pinned Mistral catalog bytes published by `@earendil-works/pi-ai@0.84.2`.
pub const MISTRAL_CATALOG_JSON: &str = include_str!("../data/mistral.json");

/// Invalid pinned Mistral catalog data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MistralCatalogError(String);

impl fmt::Display for MistralCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for MistralCatalogError {}

fn invalid(message: impl Into<String>) -> MistralCatalogError {
    MistralCatalogError(message.into())
}

/// Loads the exact Mistral model set published by pinned Pi.
pub fn mistral_models() -> Result<Vec<ModelDescriptor>, MistralCatalogError> {
    let root: Value = serde_json::from_str(MISTRAL_CATALOG_JSON)
        .map_err(|error| invalid(format!("invalid published catalog: {error}")))?;
    object(&root, "catalog root")?
        .get("mistral-conversations")
        .ok_or_else(|| invalid("catalog omits mistral-conversations"))
        .and_then(|family| object(family, "Mistral family"))?
        .values()
        .map(parse_model)
        .collect()
}

fn parse_model(value: &Value) -> Result<ModelDescriptor, MistralCatalogError> {
    let model = object(value, "model")?;
    let id = string(model, "id")?;
    if string(model, "provider")? != "mistral" || string(model, "api")? != "mistral-conversations" {
        return Err(invalid(format!("invalid Mistral identity for {id}")));
    }
    let input = array(model, "input")?
        .iter()
        .map(|value| match value.as_str() {
            Some("text") => Ok(Modality::Text),
            Some("image") => Ok(Modality::Image),
            Some("audio") => Ok(Modality::Audio),
            _ => Err(invalid(format!("invalid input modality for {id}"))),
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let cost = object(
        model
            .get("cost")
            .ok_or_else(|| invalid(format!("model {id} omits cost")))?,
        "cost",
    )?;
    Ok(ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new("mistral", id),
            display_name: string(model, "name")?.into(),
            base_url: Url::parse(string(model, "baseUrl")?)
                .map_err(|error| invalid(format!("invalid base URL for {id}: {error}")))?,
            modalities: ModalityCapabilities {
                input,
                output: BTreeSet::from([Modality::Text]),
            },
            limits: ModelLimits {
                context_window: unsigned(model, "contextWindow")?,
                max_output_tokens: u32::try_from(unsigned(model, "maxTokens")?)
                    .map_err(|_| invalid(format!("maxTokens does not fit u32 for {id}")))?,
            },
            pricing: ModelPricing {
                default: TokenPriceRates {
                    input: money_rate(cost, "input")?,
                    output: money_rate(cost, "output")?,
                    cache_read: money_rate(cost, "cacheRead")?,
                    cache_write: money_rate(cost, "cacheWrite")?,
                },
                request_wide_tiers: Vec::new(),
                cache_write_retention: CacheWriteRetentionPricing::default(),
            },
            reasoning: boolean(model, "reasoning")?,
            headers: HeaderMapSpec::new(),
        },
        api: ApiModelConfig::MistralConversations(MistralModelConfig {
            thinking_levels: parse_thinking_levels(model.get("thinkingLevelMap"), id)?,
        }),
        extensions: Default::default(),
    })
}

fn parse_thinking_levels(
    value: Option<&Value>,
    model_id: &str,
) -> Result<ThinkingLevelMap<String>, MistralCatalogError> {
    let Some(value) = value else {
        return Ok(ThinkingLevelMap::default());
    };
    let values = object(value, "thinkingLevelMap")?;
    let parse = |name| match values.get(name) {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(LevelSupport::Unsupported)),
        Some(Value::String(value)) => Ok(Some(LevelSupport::Value(value.clone()))),
        Some(_) => Err(invalid(format!(
            "invalid thinking mapping for {model_id} level {name}"
        ))),
    };
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

fn money_rate(cost: &Map<String, Value>, name: &str) -> Result<MoneyRate, MistralCatalogError> {
    let source = cost
        .get(name)
        .and_then(Value::as_number)
        .ok_or_else(|| invalid(format!("catalog cost omits {name}")))?
        .to_string();
    let (whole, fraction) = source.split_once('.').unwrap_or((&source, ""));
    if fraction.len() > 6 || source.contains(['e', 'E']) {
        return Err(invalid(format!("invalid catalog price: {source}")));
    }
    let whole = whole
        .parse::<i128>()
        .map_err(|_| invalid(format!("invalid catalog price: {source}")))?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<i128>()
            .map_err(|_| invalid(format!("invalid catalog price: {source}")))?
            * 10_i128.pow(u32::try_from(6 - fraction.len()).unwrap_or_default())
    };
    Ok(MoneyRate::new(whole.saturating_mul(1_000_000) + fraction))
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a Map<String, Value>, MistralCatalogError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{what} is not an object")))
}

fn string<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str, MistralCatalogError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(format!("catalog field {name} is not a string")))
}

fn array<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Vec<Value>, MistralCatalogError> {
    object
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| invalid(format!("catalog field {name} is not an array")))
}

fn unsigned(object: &Map<String, Value>, name: &str) -> Result<u64, MistralCatalogError> {
    object
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid(format!("catalog field {name} is not unsigned")))
}

fn boolean(object: &Map<String, Value>, name: &str) -> Result<bool, MistralCatalogError> {
    object
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid(format!("catalog field {name} is not boolean")))
}
