use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static CLOUDFLARE_WORKERS_AI_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog(
        "cloudflare-workers-ai",
        include_str!("data/cloudflare-workers-ai.json"),
    )
});
