use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static AZURE_OPENAI_RESPONSES_MODELS: LazyLock<ModelCatalog> = LazyLock::new(|| {
    parse_embedded_model_catalog(
        "azure-openai-responses",
        include_str!("data/azure-openai-responses.json"),
    )
});
