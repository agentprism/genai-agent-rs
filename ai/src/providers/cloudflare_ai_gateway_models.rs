use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static CLOUDFLARE_AI_GATEWAY_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog(
        "cloudflare-ai-gateway",
        include_str!("data/cloudflare-ai-gateway.json"),
    )
});
