//! Pinned Amazon Bedrock model catalog.

use agentprism_ai::{
    ApiModelConfig, BedrockCompat, BedrockModelConfig, CacheWriteRetentionPricing,
    CommonModelDescriptor, LevelSupport, Modality, ModalityCapabilities, ModelDescriptor, ModelId,
    ModelLimits, ModelPricing, ModelRef, MoneyRate, ProviderId, ThinkingLevelMap, TokenPriceRates,
};
use serde::Deserialize;
use serde_json::Number;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use url::Url;

const PINNED_CATALOG: &str = include_str!("data/amazon-bedrock.json");

/// Returns every model in pinned Pi's generated `AMAZON_BEDROCK_MODELS`
/// catalog, including region-specific inference profiles.
pub fn bedrock_models() -> Vec<ModelDescriptor> {
    static MODELS: OnceLock<Vec<ModelDescriptor>> = OnceLock::new();
    MODELS.get_or_init(parse_pinned_catalog).clone()
}

#[derive(Deserialize)]
struct CatalogFile {
    #[serde(rename = "bedrock-converse-stream")]
    models: BTreeMap<String, CatalogModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogModel {
    id: String,
    name: String,
    api: String,
    provider: String,
    base_url: String,
    reasoning: bool,
    input: BTreeSet<Modality>,
    cost: CatalogCost,
    context_window: u64,
    max_tokens: u32,
    #[serde(default)]
    compat: CatalogCompat,
    #[serde(default)]
    thinking_level_map: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogCost {
    input: Number,
    output: Number,
    cache_read: Number,
    cache_write: Number,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogCompat {
    supports_strict_mode: Option<bool>,
}

fn parse_pinned_catalog() -> Vec<ModelDescriptor> {
    let catalog: CatalogFile =
        serde_json::from_str(PINNED_CATALOG).expect("vendored pinned Bedrock catalog is valid");
    catalog
        .models
        .into_iter()
        .map(|(key, model)| {
            assert_eq!(key, model.id, "Bedrock catalog key must equal model id");
            assert_eq!(model.api, "bedrock-converse-stream");
            assert_eq!(model.provider, "amazon-bedrock");
            ModelDescriptor {
                common: CommonModelDescriptor {
                    model_ref: ModelRef::new(
                        ProviderId::new("amazon-bedrock"),
                        ModelId::new(model.id),
                    ),
                    display_name: model.name,
                    base_url: Url::parse(&model.base_url)
                        .expect("generated Bedrock base URL is valid"),
                    modalities: ModalityCapabilities {
                        input: model.input,
                        output: [Modality::Text].into_iter().collect(),
                    },
                    limits: ModelLimits {
                        context_window: model.context_window,
                        max_output_tokens: model.max_tokens,
                    },
                    pricing: ModelPricing {
                        default: TokenPriceRates {
                            input: money_rate(&model.cost.input),
                            output: money_rate(&model.cost.output),
                            cache_read: money_rate(&model.cost.cache_read),
                            cache_write: money_rate(&model.cost.cache_write),
                        },
                        request_wide_tiers: Vec::new(),
                        cache_write_retention: CacheWriteRetentionPricing::default(),
                    },
                    reasoning: model.reasoning,
                    headers: Default::default(),
                },
                api: ApiModelConfig::BedrockConverse(BedrockModelConfig {
                    compat: BedrockCompat {
                        supports_strict_mode: model.compat.supports_strict_mode,
                        extensions: Default::default(),
                    },
                    thinking_levels: thinking_levels(model.thinking_level_map),
                }),
                extensions: Default::default(),
            }
        })
        .collect()
}

fn money_rate(value: &Number) -> MoneyRate {
    let text = value.to_string();
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    assert!(
        fraction.len() <= 6,
        "catalog price exceeds micro-dollar precision"
    );
    let whole = whole
        .parse::<i128>()
        .expect("catalog price integer is valid");
    let fraction = format!("{fraction:0<6}")
        .parse::<i128>()
        .expect("catalog price fraction is valid");
    MoneyRate::new(whole * 1_000_000 + fraction)
}

fn thinking_levels(values: BTreeMap<String, Option<String>>) -> ThinkingLevelMap<String> {
    fn level(
        values: &BTreeMap<String, Option<String>>,
        name: &str,
    ) -> Option<LevelSupport<String>> {
        values.get(name).map(|value| match value {
            Some(value) => LevelSupport::Value(value.clone()),
            None => LevelSupport::Unsupported,
        })
    }

    ThinkingLevelMap {
        off: level(&values, "off"),
        minimal: level(&values, "minimal"),
        low: level(&values, "low"),
        medium: level(&values, "medium"),
        high: level(&values, "high"),
        xhigh: level(&values, "xhigh"),
        max: level(&values, "max"),
    }
}
