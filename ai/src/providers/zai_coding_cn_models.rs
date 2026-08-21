use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static ZAI_CODING_CN_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog("zai-coding-cn", include_str!("data/zai-coding-cn.json"))
});
