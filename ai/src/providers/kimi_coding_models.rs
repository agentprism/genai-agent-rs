use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static KIMI_CODING_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog("kimi-coding", include_str!("data/kimi-coding.json"))
});
