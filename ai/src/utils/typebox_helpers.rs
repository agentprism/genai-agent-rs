//! TypeBox-compatible schema helpers ⇐ pi `src/utils/typebox-helpers.ts`.

use serde_json::{Map, Value};

pub fn string_enum(
    values: impl IntoIterator<Item = impl Into<String>>,
    description: Option<&str>,
    default: Option<&str>,
) -> Value {
    let mut schema = Map::new();
    schema.insert("type".to_owned(), Value::String("string".to_owned()));
    schema.insert(
        "enum".to_owned(),
        Value::Array(
            values
                .into_iter()
                .map(|value| Value::String(value.into()))
                .collect(),
        ),
    );
    if let Some(description) = description.filter(|value| !value.is_empty()) {
        schema.insert(
            "description".to_owned(),
            Value::String(description.to_owned()),
        );
    }
    if let Some(default) = default.filter(|value| !value.is_empty()) {
        schema.insert("default".to_owned(), Value::String(default.to_owned()));
    }
    Value::Object(schema)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pins pi `src/utils/typebox-helpers.ts:14-24` truthy option spreading.
    #[test]
    fn string_enum_uses_google_compatible_shape() {
        assert_eq!(
            string_enum(["add", "subtract"], Some("Operation"), Some("add")),
            json!({
                "type":"string",
                "enum":["add","subtract"],
                "description":"Operation",
                "default":"add"
            })
        );
        assert_eq!(
            string_enum(["one"], Some(""), Some("")),
            json!({"type":"string","enum":["one"]})
        );
    }
}
