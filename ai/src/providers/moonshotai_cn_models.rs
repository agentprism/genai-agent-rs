use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static MOONSHOTAI_CN_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog("moonshotai-cn", include_str!("data/moonshotai-cn.json"))
});
