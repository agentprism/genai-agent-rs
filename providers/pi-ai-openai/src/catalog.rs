//! Pinned published model catalogs for the two shared-family providers.

use pi_ai::{
    ApiModelConfig, CacheControlFormat, CacheWriteRetentionPricing, CommonModelDescriptor,
    HeaderMapSpec, LevelSupport, MaxTokensField, Modality, ModalityCapabilities, ModelDescriptor,
    ModelLimits, ModelPricing, ModelRef, MoneyRate, OpenAiCompletionsCompat,
    OpenAiCompletionsModelConfig, OpenAiThinkingFormat, OpenAiThinkingValue, ThinkingLevelMap,
    TokenPriceRates,
};
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::fmt;
use url::Url;

/// Pinned DeepSeek catalog bytes published by `@earendil-works/pi-ai@0.84.2`.
pub const DEEPSEEK_CATALOG_JSON: &str = include_str!("../data/deepseek.json");

/// Pinned OpenRouter catalog bytes published by `@earendil-works/pi-ai@0.84.2`.
pub const OPENROUTER_CATALOG_JSON: &str = include_str!("../data/openrouter.json");

/// Failure while converting Pi's published catalog into typed Rust models.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAiCatalogError {
    message: String,
}

impl OpenAiCatalogError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for OpenAiCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OpenAiCatalogError {}

/// Loads the exact DeepSeek model set published by pinned Pi.
pub fn deepseek_models() -> Result<Vec<ModelDescriptor>, OpenAiCatalogError> {
    parse_published_catalog(DEEPSEEK_CATALOG_JSON, "deepseek")
}

/// Loads the exact OpenRouter model set published by pinned Pi.
pub fn openrouter_models() -> Result<Vec<ModelDescriptor>, OpenAiCatalogError> {
    parse_published_catalog(OPENROUTER_CATALOG_JSON, "openrouter")
}

fn parse_published_catalog(
    source: &str,
    expected_provider: &str,
) -> Result<Vec<ModelDescriptor>, OpenAiCatalogError> {
    let root: Value = serde_json::from_str(source)
        .map_err(|error| OpenAiCatalogError::new(format!("invalid published catalog: {error}")))?;
    let family = object(&root, "catalog root")?
        .get("openai-completions")
        .ok_or_else(|| OpenAiCatalogError::new("catalog omits openai-completions"))?;
    object(family, "openai-completions catalog")?
        .values()
        .map(|model| parse_model(model, expected_provider))
        .collect()
}

fn parse_model(
    value: &Value,
    expected_provider: &str,
) -> Result<ModelDescriptor, OpenAiCatalogError> {
    let model = object(value, "model")?;
    let id = string(model, "id")?;
    let provider = string(model, "provider")?;
    if provider != expected_provider {
        return Err(OpenAiCatalogError::new(format!(
            "catalog model {id} belongs to {provider}, expected {expected_provider}"
        )));
    }
    if string(model, "api")? != "openai-completions" {
        return Err(OpenAiCatalogError::new(format!(
            "catalog model {id} does not use openai-completions"
        )));
    }

    let input = array(model, "input")?
        .iter()
        .map(|modality| match modality.as_str() {
            Some("text") => Ok(Modality::Text),
            Some("image") => Ok(Modality::Image),
            Some("audio") => Ok(Modality::Audio),
            Some(other) => Err(OpenAiCatalogError::new(format!(
                "unknown input modality {other} for {id}"
            ))),
            None => Err(OpenAiCatalogError::new(format!(
                "non-string input modality for {id}"
            ))),
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    let output = BTreeSet::from([Modality::Text]);
    let cost = object(
        model
            .get("cost")
            .ok_or_else(|| OpenAiCatalogError::new(format!("model {id} omits cost")))?,
        "model cost",
    )?;

    Ok(ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, id),
            display_name: string(model, "name")?.to_owned(),
            base_url: Url::parse(string(model, "baseUrl")?).map_err(|error| {
                OpenAiCatalogError::new(format!("invalid base URL for {id}: {error}"))
            })?,
            modalities: ModalityCapabilities { input, output },
            limits: ModelLimits {
                context_window: unsigned(model, "contextWindow")?,
                max_output_tokens: u32::try_from(unsigned(model, "maxTokens")?).map_err(|_| {
                    OpenAiCatalogError::new(format!("maxTokens does not fit u32 for {id}"))
                })?,
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
        api: ApiModelConfig::OpenAiCompletions(OpenAiCompletionsModelConfig {
            compat: parse_compat(model.get("compat"), provider, id)?,
            thinking_levels: parse_thinking_levels(model.get("thinkingLevelMap"), id)?,
            sampling_defaults: Default::default(),
        }),
        extensions: Default::default(),
    })
}

fn parse_compat(
    value: Option<&Value>,
    provider: &str,
    model_id: &str,
) -> Result<OpenAiCompletionsCompat, OpenAiCatalogError> {
    let Some(value) = value else {
        return Ok(OpenAiCompletionsCompat::default());
    };
    let compat = object(value, "model compat")?;
    Ok(OpenAiCompletionsCompat {
        supports_store: optional_bool(compat, "supportsStore", model_id)?,
        supports_developer_role: optional_bool(compat, "supportsDeveloperRole", model_id)?.or_else(
            || {
                (provider == "openrouter"
                    && (model_id.starts_with("anthropic/") || model_id.starts_with("openai/")))
                .then_some(true)
            },
        ),
        requires_reasoning_content_on_assistant_messages: optional_bool(
            compat,
            "requiresReasoningContentOnAssistantMessages",
            model_id,
        )?,
        max_tokens_field: compat
            .get("maxTokensField")
            .map(|value| match value.as_str() {
                Some("max_tokens") => Ok(MaxTokensField::MaxTokens),
                Some("max_completion_tokens") => Ok(MaxTokensField::MaxCompletionTokens),
                _ => Err(OpenAiCatalogError::new(format!(
                    "invalid maxTokensField for {model_id}"
                ))),
            })
            .transpose()?,
        thinking_format: compat
            .get("thinkingFormat")
            .map(|value| {
                serde_json::from_value::<OpenAiThinkingFormat>(value.clone()).map_err(|error| {
                    OpenAiCatalogError::new(format!(
                        "invalid thinkingFormat for {model_id}: {error}"
                    ))
                })
            })
            .transpose()?,
        cache_control_format: compat
            .get("cacheControlFormat")
            .map(|value| {
                serde_json::from_value::<CacheControlFormat>(value.clone()).map_err(|error| {
                    OpenAiCatalogError::new(format!(
                        "invalid cacheControlFormat for {model_id}: {error}"
                    ))
                })
            })
            .transpose()?
            .or_else(|| {
                (provider == "openrouter" && model_id.starts_with("anthropic/"))
                    .then_some(CacheControlFormat::Anthropic)
            }),
        ..Default::default()
    })
}

fn parse_thinking_levels(
    value: Option<&Value>,
    model_id: &str,
) -> Result<ThinkingLevelMap<OpenAiThinkingValue>, OpenAiCatalogError> {
    let Some(value) = value else {
        return Ok(ThinkingLevelMap::default());
    };
    let values = object(value, "thinkingLevelMap")?;
    let parse = |name| parse_thinking_level(values.get(name), model_id, name);
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
) -> Result<Option<LevelSupport<OpenAiThinkingValue>>, OpenAiCatalogError> {
    match value {
        None => Ok(None),
        Some(Value::Null) => Ok(Some(LevelSupport::Unsupported)),
        Some(Value::String(value)) => Ok(Some(LevelSupport::Value(OpenAiThinkingValue::Effort(
            value.clone(),
        )))),
        Some(Value::Number(value)) => {
            let budget = value.as_u64().and_then(|value| u32::try_from(value).ok());
            budget
                .map(|value| Some(LevelSupport::Value(OpenAiThinkingValue::TokenBudget(value))))
                .ok_or_else(|| {
                    OpenAiCatalogError::new(format!(
                        "invalid thinking budget for {model_id} level {level}"
                    ))
                })
        }
        Some(_) => Err(OpenAiCatalogError::new(format!(
            "invalid thinking mapping for {model_id} level {level}"
        ))),
    }
}

fn money_rate(cost: &Map<String, Value>, name: &str) -> Result<MoneyRate, OpenAiCatalogError> {
    let number = cost
        .get(name)
        .and_then(Value::as_number)
        .ok_or_else(|| OpenAiCatalogError::new(format!("catalog cost omits {name}")))?;
    decimal_dollars_per_million_to_rate(&number.to_string())
}

fn decimal_dollars_per_million_to_rate(value: &str) -> Result<MoneyRate, OpenAiCatalogError> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned));
    let (whole, fraction) = unsigned.split_once('.').unwrap_or((unsigned, ""));
    if whole.is_empty() || fraction.len() > 6 || value.contains(['e', 'E']) {
        return Err(OpenAiCatalogError::new(format!(
            "catalog price is not a signed six-place decimal: {value}"
        )));
    }
    let whole = whole
        .parse::<u128>()
        .map_err(|_| OpenAiCatalogError::new(format!("invalid catalog price: {value}")))?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction
            .parse::<u128>()
            .map_err(|_| OpenAiCatalogError::new(format!("invalid catalog price: {value}")))?
            * 10_u128.pow(u32::try_from(6 - fraction.len()).unwrap_or_default())
    };
    let magnitude = whole
        .checked_mul(1_000_000)
        .and_then(|whole| whole.checked_add(fraction))
        .ok_or_else(|| OpenAiCatalogError::new(format!("catalog price overflow: {value}")))?;
    let micros = i128::try_from(magnitude)
        .map_err(|_| OpenAiCatalogError::new(format!("catalog price overflow: {value}")))?;
    Ok(MoneyRate::new(if negative { -micros } else { micros }))
}

fn object<'a>(
    value: &'a Value,
    description: &str,
) -> Result<&'a Map<String, Value>, OpenAiCatalogError> {
    value
        .as_object()
        .ok_or_else(|| OpenAiCatalogError::new(format!("{description} is not an object")))
}

fn string<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a str, OpenAiCatalogError> {
    object
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| OpenAiCatalogError::new(format!("catalog field {name} is not a string")))
}

fn array<'a>(
    object: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Vec<Value>, OpenAiCatalogError> {
    object
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| OpenAiCatalogError::new(format!("catalog field {name} is not an array")))
}

fn unsigned(object: &Map<String, Value>, name: &str) -> Result<u64, OpenAiCatalogError> {
    object
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| OpenAiCatalogError::new(format!("catalog field {name} is not unsigned")))
}

fn boolean(object: &Map<String, Value>, name: &str) -> Result<bool, OpenAiCatalogError> {
    object
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| OpenAiCatalogError::new(format!("catalog field {name} is not boolean")))
}

fn optional_bool(
    object: &Map<String, Value>,
    name: &str,
    model_id: &str,
) -> Result<Option<bool>, OpenAiCatalogError> {
    object
        .get(name)
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                OpenAiCatalogError::new(format!("compat {name} is not boolean for {model_id}"))
            })
        })
        .transpose()
}
