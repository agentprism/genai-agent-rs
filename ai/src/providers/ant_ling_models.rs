use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static ANT_LING_MODELS: LazyLock<ModelCatalog> =
    LazyLock::new(|| parse_embedded_model_catalog("ant-ling", include_str!("data/ant-ling.json")));
