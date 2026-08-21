use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static GROQ_MODELS: LazyLock<ModelCatalog> =
    LazyLock::new(|| parse_embedded_model_catalog("groq", include_str!("data/groq.json")));
