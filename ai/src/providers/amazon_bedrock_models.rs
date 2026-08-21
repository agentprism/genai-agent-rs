use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static AMAZON_BEDROCK_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog("amazon-bedrock", include_str!("data/amazon-bedrock.json"))
});
