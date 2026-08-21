use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static ANTHROPIC_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog("anthropic", include_str!("data/anthropic.json"))
});
