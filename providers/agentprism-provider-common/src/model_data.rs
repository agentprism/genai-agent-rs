//! Validation of native provider-owned generated model-data shards.
//!
//! This is the Rust counterpart of pinned Pi's `scripts/model-data.ts`.  The
//! port uses provider-owned JSON shards instead of a TypeScript aggregator,
//! but retains exact-set checks, generation stamps, content hashes, grouping
//! validation, and model-schema validation before publication.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::Path;

/// Current native generated-model-data manifest schema.
pub const MODEL_DATA_SCHEMA_VERSION: u32 = 3;

/// Name of the manifest stored beside provider data shards.
pub const MODEL_DATA_MANIFEST_FILE: &str = ".manifest.json";

/// Provider -> model -> API-family generation structure.
pub type ModelDataStructure = BTreeMap<String, BTreeMap<String, String>>;

/// Hash manifest for one generated model-data publication.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelDataManifest {
    /// Manifest schema version.
    pub schema_version: u32,
    /// Generation timestamp emitted by the generator.
    pub generated_at: String,
    /// SHA-256 of the normalized generation structure.
    pub structure_hash: String,
    /// Provider filename -> SHA-256 of its exact UTF-8 contents.
    pub files: BTreeMap<String, String>,
}

/// Failure while validating generated model data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelDataValidationError(String);

impl ModelDataValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ModelDataValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ModelDataValidationError {}

/// Requires two model-ID collections to contain exactly the same unique IDs.
pub fn assert_exact_model_ids(
    label: &str,
    expected: impl IntoIterator<Item = impl Into<String>>,
    actual: impl IntoIterator<Item = impl Into<String>>,
) -> Result<(), ModelDataValidationError> {
    let expected = expected
        .into_iter()
        .map(Into::into)
        .collect::<BTreeSet<_>>();
    let actual = actual.into_iter().map(Into::into).collect::<BTreeSet<_>>();
    if expected == actual {
        return Ok(());
    }
    Err(ModelDataValidationError::new(format!(
        "{label} model IDs do not match ({})",
        describe_set_difference(&expected, &actual)
    )))
}

/// Returns the Pi-compatible normalized SHA-256 generation-structure hash.
pub fn model_data_structure_hash(structure: &ModelDataStructure) -> String {
    let normalized = serde_json::to_string(structure)
        .expect("BTreeMap model-data structure always serializes as JSON");
    sha256(normalized.as_bytes())
}

/// Creates a sorted content-hash manifest for generated provider shards.
pub fn create_model_data_manifest(
    structure: &ModelDataStructure,
    file_contents: &BTreeMap<String, String>,
    generated_at: impl Into<String>,
) -> ModelDataManifest {
    ModelDataManifest {
        schema_version: MODEL_DATA_SCHEMA_VERSION,
        generated_at: generated_at.into(),
        structure_hash: model_data_structure_hash(structure),
        files: file_contents
            .iter()
            .map(|(file, content)| (file.clone(), sha256(content.as_bytes())))
            .collect(),
    }
}

/// Checks that provider-owned native shard filenames exactly match the
/// generated provider inventory.
pub fn validate_model_shard_inventory(
    provider_ids: impl IntoIterator<Item = impl Into<String>>,
    actual_shards: impl IntoIterator<Item = impl Into<String>>,
) -> Result<(), ModelDataValidationError> {
    let expected = provider_ids
        .into_iter()
        .map(Into::into)
        .map(|provider| format!("{provider}.models.rs"))
        .collect::<BTreeSet<_>>();
    let actual = actual_shards
        .into_iter()
        .map(Into::into)
        .collect::<BTreeSet<_>>();
    if expected == actual {
        return Ok(());
    }
    Err(ModelDataValidationError::new(format!(
        "generated model aggregator and provider shards do not match ({})",
        describe_set_difference(&expected, &actual)
    )))
}

/// Validates a generated-model-data directory before its shards are published.
pub fn validate_model_data_directory(
    structure: &ModelDataStructure,
    data_dir: &Path,
) -> Result<(), ModelDataValidationError> {
    if !data_dir.is_dir() {
        return Err(ModelDataValidationError::new(format!(
            "generated model data directory does not exist: {}",
            data_dir.display()
        )));
    }

    let mut errors = Vec::new();
    let expected_files = structure
        .keys()
        .map(|provider| format!("{provider}.json"))
        .collect::<BTreeSet<_>>();
    let actual_files = match fs::read_dir(data_dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.ends_with(".json") && name != MODEL_DATA_MANIFEST_FILE)
            .collect::<BTreeSet<_>>(),
        Err(error) => {
            return Err(ModelDataValidationError::new(format!(
                "cannot read generated model data directory {}: {error}",
                data_dir.display()
            )));
        }
    };
    if expected_files != actual_files {
        errors.push(format!(
            "provider data files do not match the generated catalog ({})",
            describe_set_difference(&expected_files, &actual_files)
        ));
    }

    let manifest_value = read_json_object(
        &data_dir.join(MODEL_DATA_MANIFEST_FILE),
        "model data manifest",
        &mut errors,
    );
    if manifest_value
        .as_ref()
        .and_then(|manifest| manifest.get("schemaVersion"))
        .and_then(Value::as_u64)
        != Some(u64::from(MODEL_DATA_SCHEMA_VERSION))
    {
        errors.push(format!(
            "model data schema is {}, expected {MODEL_DATA_SCHEMA_VERSION}",
            display_json_field(manifest_value.as_ref(), "schemaVersion")
        ));
    }
    if !manifest_value
        .as_ref()
        .and_then(|manifest| manifest.get("generatedAt"))
        .and_then(Value::as_str)
        .is_some_and(is_generated_timestamp)
    {
        errors.push("model data manifest has an invalid generation timestamp".into());
    }
    if manifest_value
        .as_ref()
        .and_then(|manifest| manifest.get("structureHash"))
        .and_then(Value::as_str)
        != Some(model_data_structure_hash(structure).as_str())
    {
        errors.push("model data generation stamp does not match the generated catalog".into());
    }
    let manifest_files = manifest_value
        .as_ref()
        .and_then(|manifest| manifest.get("files"))
        .and_then(Value::as_object);
    if let Some(manifest_files) = manifest_files {
        let names = manifest_files.keys().cloned().collect::<BTreeSet<_>>();
        if names != expected_files {
            errors.push(format!(
                "manifest file hashes do not match provider data files ({})",
                describe_set_difference(&expected_files, &names)
            ));
        }
    } else {
        errors.push("model data manifest has no file hashes".into());
    }

    for (provider, expected_models) in structure {
        validate_provider_file(
            provider,
            expected_models,
            data_dir,
            manifest_files,
            &mut errors,
        );
    }

    if errors.is_empty() {
        Ok(())
    } else {
        let visible = errors.iter().take(30).collect::<Vec<_>>();
        let mut message = format!(
            "invalid generated model data:\n{}",
            visible
                .iter()
                .map(|error| format!("  - {error}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        if errors.len() > visible.len() {
            message.push_str(&format!(
                "\n  ... and {} more",
                errors.len() - visible.len()
            ));
        }
        Err(ModelDataValidationError::new(message))
    }
}

fn validate_provider_file(
    provider: &str,
    expected_models: &BTreeMap<String, String>,
    data_dir: &Path,
    manifest_files: Option<&Map<String, Value>>,
    errors: &mut Vec<String>,
) {
    let filename = format!("{provider}.json");
    let path = data_dir.join(&filename);
    if !path.exists() {
        return;
    }
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) => {
            errors.push(format!("cannot read {filename}: {error}"));
            return;
        }
    };
    if manifest_files
        .and_then(|files| files.get(&filename))
        .and_then(Value::as_str)
        != Some(sha256(content.as_bytes()).as_str())
    {
        errors.push(format!("{filename} does not match its manifest hash"));
    }
    let Some(groups) = read_json_object(&path, &filename, errors) else {
        return;
    };
    let mut actual_models = BTreeMap::new();
    for (api, value) in groups {
        let Some(models) = value.as_object() else {
            errors.push(format!("{filename} API group {api:?} must be an object"));
            continue;
        };
        for (model_id, model) in models {
            if actual_models
                .insert(model_id.clone(), api.clone())
                .is_some()
            {
                errors.push(format!(
                    "{provider}/{model_id} appears in more than one API group"
                ));
                continue;
            }
            validate_model_value(model, provider, model_id, &api, errors);
        }
    }

    let expected_ids = expected_models.keys().cloned().collect::<BTreeSet<_>>();
    let actual_ids = actual_models.keys().cloned().collect::<BTreeSet<_>>();
    if expected_ids != actual_ids {
        errors.push(format!(
            "{filename} model IDs do not match the generated catalog ({})",
            describe_set_difference(&expected_ids, &actual_ids)
        ));
    }
    for (model_id, expected_api) in expected_models {
        if let Some(actual_api) = actual_models.get(model_id)
            && actual_api != expected_api
        {
            errors.push(format!(
                "{provider}/{model_id} is grouped under API {actual_api:?}, expected {expected_api:?}"
            ));
        }
    }
}

fn validate_model_value(
    value: &Value,
    provider: &str,
    model_id: &str,
    expected_api: &str,
    errors: &mut Vec<String>,
) {
    let label = format!("{provider}/{model_id}");
    let Some(model) = value.as_object() else {
        errors.push(format!("{label} must be an object"));
        return;
    };
    check_exact_string(model, "id", model_id, &label, errors);
    check_exact_string(model, "provider", provider, &label, errors);
    check_exact_string(model, "api", expected_api, &label, errors);
    if !model
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|name| !name.is_empty())
    {
        errors.push(format!("{label} has no model name"));
    }
    if !model.get("baseUrl").is_some_and(Value::is_string) {
        errors.push(format!("{label} has no baseUrl string"));
    }
    if !model.get("reasoning").is_some_and(Value::is_boolean) {
        errors.push(format!("{label} has no reasoning boolean"));
    }
    if !model
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|input| {
            !input.is_empty()
                && input
                    .iter()
                    .all(|entry| matches!(entry.as_str(), Some("text" | "image")))
        })
    {
        errors.push(format!("{label} has invalid input modalities"));
    }
    for field in ["contextWindow", "maxTokens"] {
        if !model
            .get(field)
            .and_then(Value::as_f64)
            .is_some_and(|value| value.is_finite() && value > 0.0)
        {
            errors.push(format!("{label} has invalid {field}"));
        }
    }
    let Some(cost) = model.get("cost").and_then(Value::as_object) else {
        errors.push(format!("{label} has invalid cost metadata"));
        return;
    };
    for field in ["input", "output", "cacheRead", "cacheWrite"] {
        if !cost.get(field).is_some_and(Value::is_number) {
            errors.push(format!("{label} has invalid cost.{field}"));
        }
    }
}

fn check_exact_string(
    model: &Map<String, Value>,
    field: &str,
    expected: &str,
    label: &str,
    errors: &mut Vec<String>,
) {
    if model.get(field).and_then(Value::as_str) != Some(expected) {
        errors.push(format!(
            "{label} has {field} {}, expected {expected:?}",
            model
                .get(field)
                .map_or_else(|| "null".into(), Value::to_string)
        ));
    }
}

fn read_json_object(
    path: &Path,
    description: &str,
    errors: &mut Vec<String>,
) -> Option<Map<String, Value>> {
    let source = match fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            errors.push(format!("{description} is not valid JSON: {error}"));
            return None;
        }
    };
    match serde_json::from_str::<Value>(&source) {
        Ok(Value::Object(object)) => Some(object),
        Ok(_) => {
            errors.push(format!("{description} must contain a JSON object"));
            None
        }
        Err(error) => {
            errors.push(format!("{description} is not valid JSON: {error}"));
            None
        }
    }
}

fn describe_set_difference(expected: &BTreeSet<String>, actual: &BTreeSet<String>) -> String {
    let mut parts = Vec::new();
    let missing = expected.difference(actual).cloned().collect::<Vec<_>>();
    let extra = actual.difference(expected).cloned().collect::<Vec<_>>();
    if !missing.is_empty() {
        parts.push(format!("missing: {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        parts.push(format!("extra: {}", extra.join(", ")));
    }
    parts.join("; ")
}

fn display_json_field(object: Option<&Map<String, Value>>, field: &str) -> String {
    object
        .and_then(|value| value.get(field))
        .map_or_else(|| "null".into(), Value::to_string)
}

fn sha256(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

fn is_generated_timestamp(value: &str) -> bool {
    // The generator emits Date.toISOString/RFC 3339 UTC stamps.  Validate the
    // stable generated shape without introducing a runtime/date dependency.
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return false;
    }
    let Some(time) = value.strip_suffix('Z') else {
        return false;
    };
    let Some((date, clock)) = time.split_once('T') else {
        return false;
    };
    let date = date.split('-').collect::<Vec<_>>();
    let clock = clock.split(':').collect::<Vec<_>>();
    date.len() == 3
        && clock.len() == 3
        && date.iter().all(|field| field.parse::<u32>().is_ok())
        && clock[0].parse::<u32>().is_ok_and(|hour| hour < 24)
        && clock[1].parse::<u32>().is_ok_and(|minute| minute < 60)
        && clock[2]
            .split_once('.')
            .map_or(clock[2], |(second, _)| second)
            .parse::<u32>()
            .is_ok_and(|second| second < 60)
}
