use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static QWEN_TOKEN_PLAN_CN_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog(
        "qwen-token-plan-cn",
        include_str!("data/qwen-token-plan-cn.json"),
    )
});
