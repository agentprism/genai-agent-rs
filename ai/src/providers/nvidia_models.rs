use crate::model_catalog::{ModelCatalog, parse_embedded_model_catalog};
use std::sync::LazyLock;

pub static NVIDIA_MODELS: LazyLock<ModelCatalog> =
    LazyLock::new(|| parse_embedded_model_catalog("nvidia", include_str!("data/nvidia.json")));
