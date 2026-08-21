use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static MINIMAX_MODELS: LazyLock<ModelCatalog> =
    LazyLock::new(|| parse_embedded_model_catalog("minimax", include_str!("data/minimax.json")));
