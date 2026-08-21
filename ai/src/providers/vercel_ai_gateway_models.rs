use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static VERCEL_AI_GATEWAY_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog(
        "vercel-ai-gateway",
        include_str!("data/vercel-ai-gateway.json"),
    )
});
