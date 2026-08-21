use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static GOOGLE_MODELS: LazyLock<ModelCatalog> =
    LazyLock::new(|| parse_embedded_model_catalog("google", include_str!("data/google.json")));
