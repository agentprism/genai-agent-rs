use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static XIAOMI_TOKEN_PLAN_AMS_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog(
        "xiaomi-token-plan-ams",
        include_str!("data/xiaomi-token-plan-ams.json"),
    )
});
