//! Pinned Google catalog conversion into typed `pi-ai` descriptors.

use pi_ai::{
    ApiModelConfig, CommonModelDescriptor, GoogleModelConfig, LevelSupport, Modality,
    ModalityCapabilities, ModelDescriptor, ModelId, ModelLimits, ModelPricing, ModelRef, MoneyRate,
    ThinkingLevelMap, TokenPriceRates,
};
use std::collections::BTreeSet;
use std::fmt;
use url::Url;

#[derive(Clone, Copy)]
struct CatalogModel {
    id: &'static str,
    name: &'static str,
    input: i128,
    output: i128,
    cache_read: i128,
    context: u64,
    maximum: u32,
    levels: LevelProfile,
}

#[derive(Clone, Copy)]
enum LevelProfile {
    Default,
    NoOff,
    Pro,
    Gemma,
}

macro_rules! model {
    ($id:expr, $name:expr, $input:expr, $output:expr, $cache_read:expr, $context:expr, $maximum:expr, $levels:expr $(,)?) => {
        CatalogModel {
            id: $id,
            name: $name,
            input: $input,
            output: $output,
            cache_read: $cache_read,
            context: $context,
            maximum: $maximum,
            levels: $levels,
        }
    };
}

const GOOGLE_MODELS: &[CatalogModel] = &[
    model!(
        "deep-research-max-preview-04-2026",
        "Deep Research Max Preview (Apr-21-2026)",
        2_000_000,
        12_000_000,
        200_000,
        131_072,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "deep-research-preview-04-2026",
        "Deep Research Preview (Apr-21-2026)",
        2_000_000,
        12_000_000,
        200_000,
        131_072,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-2.5-computer-use-preview-10-2025",
        "Gemini 2.5 Computer Use Preview 10-2025",
        1_250_000,
        10_000_000,
        0,
        131_072,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-2.5-flash",
        "Gemini 2.5 Flash",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-2.5-flash-lite",
        "Gemini 2.5 Flash-Lite",
        100_000,
        400_000,
        10_000,
        1_048_576,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-2.5-pro",
        "Gemini 2.5 Pro",
        1_250_000,
        10_000_000,
        125_000,
        1_048_576,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-3-flash-preview",
        "Gemini 3 Flash Preview",
        500_000,
        3_000_000,
        50_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.1-flash-lite",
        "Gemini 3.1 Flash Lite",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.1-flash-lite-image",
        "Nano Banana 2 Lite",
        250_000,
        30_000_000,
        0,
        65_536,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash Lite Preview",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.1-flash-live-preview",
        "Gemini 3.1 Flash Live Preview",
        750_000,
        4_500_000,
        0,
        131_072,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.1-pro-preview",
        "Gemini 3.1 Pro Preview",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        LevelProfile::Pro,
    ),
    model!(
        "gemini-3.1-pro-preview-customtools",
        "Gemini 3.1 Pro Preview Custom Tools",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        LevelProfile::Pro,
    ),
    model!(
        "gemini-3.5-flash",
        "Gemini 3.5 Flash",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.5-flash-lite",
        "Gemini 3.5 Flash Lite",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.6-flash",
        "Gemini 3.6 Flash",
        1_500_000,
        7_500_000,
        150_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.7-flash",
        "Gemini 3.7 Flash",
        750_000,
        3_750_000,
        75_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-flash-latest",
        "Gemini Flash Latest",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-flash-lite-latest",
        "Gemini Flash-Lite Latest",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-robotics-er-1.6-preview",
        "Gemini Robotics-ER 1.6 Preview",
        1_000_000,
        5_000_000,
        0,
        131_072,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemma-4-26b-a4b-it",
        "Gemma 4 26B A4B IT",
        0,
        0,
        0,
        262_144,
        32_768,
        LevelProfile::Gemma,
    ),
    model!(
        "gemma-4-31b-it",
        "Gemma 4 31B IT",
        0,
        0,
        0,
        262_144,
        32_768,
        LevelProfile::Gemma,
    ),
];

const VERTEX_MODELS: &[CatalogModel] = &[
    model!(
        "gemini-2.5-flash",
        "Gemini 2.5 Flash",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-2.5-flash-lite",
        "Gemini 2.5 Flash-Lite",
        100_000,
        400_000,
        10_000,
        1_048_576,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-2.5-pro",
        "Gemini 2.5 Pro",
        1_250_000,
        10_000_000,
        125_000,
        1_048_576,
        65_536,
        LevelProfile::Default,
    ),
    model!(
        "gemini-3-flash-preview",
        "Gemini 3 Flash Preview",
        500_000,
        3_000_000,
        50_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.1-flash-lite",
        "Gemini 3.1 Flash Lite",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.1-pro-preview",
        "Gemini 3.1 Pro Preview",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        LevelProfile::Pro,
    ),
    model!(
        "gemini-3.1-pro-preview-customtools",
        "Gemini 3.1 Pro Preview Custom Tools",
        2_000_000,
        12_000_000,
        200_000,
        1_048_576,
        65_536,
        LevelProfile::Pro,
    ),
    model!(
        "gemini-3.5-flash",
        "Gemini 3.5 Flash",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.5-flash-lite",
        "Gemini 3.5 Flash Lite",
        300_000,
        2_500_000,
        30_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.6-flash",
        "Gemini 3.6 Flash",
        1_500_000,
        7_500_000,
        150_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-3.7-flash",
        "Gemini 3.7 Flash",
        750_000,
        3_750_000,
        75_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-flash-latest",
        "Gemini Flash Latest",
        1_500_000,
        9_000_000,
        150_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
    model!(
        "gemini-flash-lite-latest",
        "Gemini Flash-Lite Latest",
        250_000,
        1_500_000,
        25_000,
        1_048_576,
        65_536,
        LevelProfile::NoOff,
    ),
];

/// Returns the pinned Gemini Developer API catalog.
pub fn google_models() -> Result<Vec<ModelDescriptor>, GoogleCatalogError> {
    parse_catalog(
        GOOGLE_MODELS,
        "google",
        "google-generative-ai",
        "https://generativelanguage.googleapis.com/v1beta",
    )
}

/// Returns the pinned Vertex Gemini catalog.
pub fn google_vertex_models() -> Result<Vec<ModelDescriptor>, GoogleCatalogError> {
    // Rust's typed URL cannot retain Pi's `{location}` hostname template. The
    // catalog stores Pi's default location as a valid URL; Vertex auth replaces
    // it with the resolved project/location endpoint before every request.
    parse_catalog(
        VERTEX_MODELS,
        "google-vertex",
        "google-vertex",
        "https://us-central1-aiplatform.googleapis.com",
    )
}

fn parse_catalog(
    source: &[CatalogModel],
    provider: &str,
    api: &str,
    base_url: &str,
) -> Result<Vec<ModelDescriptor>, GoogleCatalogError> {
    let base_url = Url::parse(base_url)
        .map_err(|error| GoogleCatalogError::new(format!("invalid catalog URL: {error}")))?;
    Ok(source
        .iter()
        .map(|source| descriptor(*source, provider, api, base_url.clone()))
        .collect())
}

fn descriptor(source: CatalogModel, provider: &str, api: &str, base_url: Url) -> ModelDescriptor {
    let modalities = [Modality::Text, Modality::Image]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let config = GoogleModelConfig {
        thinking_levels: thinking_levels(source.levels),
    };
    ModelDescriptor {
        common: CommonModelDescriptor {
            model_ref: ModelRef::new(provider, ModelId::new(source.id)),
            display_name: source.name.to_owned(),
            base_url,
            modalities: ModalityCapabilities {
                input: modalities,
                output: [Modality::Text].into_iter().collect(),
            },
            limits: ModelLimits {
                context_window: source.context,
                max_output_tokens: source.maximum,
            },
            pricing: ModelPricing {
                default: TokenPriceRates {
                    input: MoneyRate::new(source.input),
                    output: MoneyRate::new(source.output),
                    cache_read: MoneyRate::new(source.cache_read),
                    cache_write: MoneyRate::new(0),
                },
                request_wide_tiers: Vec::new(),
                cache_write_retention: Default::default(),
            },
            reasoning: true,
            headers: Default::default(),
        },
        api: if api == "google-generative-ai" {
            ApiModelConfig::GoogleGenerativeAi(config)
        } else {
            ApiModelConfig::GoogleVertex(config)
        },
        extensions: Default::default(),
    }
}

fn thinking_levels(profile: LevelProfile) -> ThinkingLevelMap<String> {
    let unsupported = || Some(LevelSupport::Unsupported);
    let value = |value: &str| Some(LevelSupport::Value(value.to_owned()));
    match profile {
        LevelProfile::Default => ThinkingLevelMap::default(),
        LevelProfile::NoOff => ThinkingLevelMap {
            off: unsupported(),
            ..Default::default()
        },
        LevelProfile::Pro => ThinkingLevelMap {
            off: unsupported(),
            minimal: unsupported(),
            low: value("LOW"),
            medium: unsupported(),
            high: value("HIGH"),
            ..Default::default()
        },
        LevelProfile::Gemma => ThinkingLevelMap {
            off: unsupported(),
            minimal: value("MINIMAL"),
            low: unsupported(),
            medium: unsupported(),
            high: value("HIGH"),
            ..Default::default()
        },
    }
}

/// Invalid built-in Google catalog data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleCatalogError {
    message: String,
}

impl GoogleCatalogError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for GoogleCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GoogleCatalogError {}
