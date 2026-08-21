use indexmap::IndexMap;
use serde::Deserialize;
use std::sync::LazyLock;

pub const MODEL_DATA_MANIFEST_JSON: &str = include_str!(".manifest.json");

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDataManifest {
    pub schema_version: u64,
    pub generated_at: String,
    pub structure_hash: String,
    pub files: IndexMap<String, String>,
}

pub static MODEL_DATA_MANIFEST: LazyLock<ModelDataManifest> = LazyLock::new(|| {
    serde_json::from_str(MODEL_DATA_MANIFEST_JSON)
        .expect("embedded provider data manifest must be valid")
});

pub fn model_data_generated_at() -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(&MODEL_DATA_MANIFEST.generated_at)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}
