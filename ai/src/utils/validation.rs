//! Tool argument validation and coercion ⇐ pi `src/utils/validation.ts`.

use crate::types::{Tool, ToolCall};
use jsonschema::error::ValidationErrorKind;
use serde_json::{Map, Number, Value};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("{0}")]
pub struct ToolValidationError(pub String);

fn schema_types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches_type(value: &Value, kind: &str) -> bool {
    match kind {
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn js_number(value: &str) -> Option<f64> {
    let value = crate::utils::error_body::trim_javascript_whitespace(value);
    if value.is_empty() {
        return Some(0.0);
    }
    let radix = if let Some(value) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some((16, value))
    } else if let Some(value) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        Some((2, value))
    } else {
        value
            .strip_prefix("0o")
            .or_else(|| value.strip_prefix("0O"))
            .map(|value| (8, value))
    };
    if let Some((radix, digits)) = radix {
        return Some(u128::from_str_radix(digits, radix).ok()? as f64);
    }
    value.parse().ok()
}

fn json_number(value: f64) -> Option<Value> {
    Number::from_f64(value).map(Value::Number)
}

fn coerce_primitive(value: &Value, kind: &str) -> Value {
    match kind {
        "number" => match value {
            Value::Null => Value::from(0),
            Value::String(value)
                if !crate::utils::error_body::trim_javascript_whitespace(value).is_empty() =>
            {
                js_number(value)
                    .filter(|value| value.is_finite())
                    .and_then(json_number)
                    .unwrap_or_else(|| Value::String(value.clone()))
            }
            Value::Bool(value) => Value::from(u8::from(*value)),
            _ => value.clone(),
        },
        "integer" => match value {
            Value::Null => Value::from(0),
            Value::String(value)
                if !crate::utils::error_body::trim_javascript_whitespace(value).is_empty() =>
            {
                js_number(value)
                    .filter(|value| value.is_finite() && value.fract() == 0.0)
                    .and_then(json_number)
                    .unwrap_or_else(|| Value::String(value.clone()))
            }
            Value::Bool(value) => Value::from(u8::from(*value)),
            _ => value.clone(),
        },
        "boolean" => match value {
            Value::Null => Value::Bool(false),
            Value::String(value) if value == "true" => Value::Bool(true),
            Value::String(value) if value == "false" => Value::Bool(false),
            Value::Number(value) if value.as_f64() == Some(1.0) => Value::Bool(true),
            Value::Number(value) if value.as_f64() == Some(0.0) => Value::Bool(false),
            _ => value.clone(),
        },
        "string" => match value {
            Value::Null => Value::String(String::new()),
            Value::Bool(value) => Value::String(value.to_string()),
            Value::Number(value) => {
                Value::String(crate::utils::error_body::js_number_string(value))
            }
            _ => value.clone(),
        },
        "null"
            if matches!(value, Value::String(value) if value.is_empty())
                || value.as_f64() == Some(0.0)
                || value == &Value::Bool(false) =>
        {
            Value::Null
        }
        _ => value.clone(),
    }
}

fn schema_valid(schema: &Value, value: &Value) -> bool {
    jsonschema::validator_for(schema).is_ok_and(|validator| validator.is_valid(value))
}

fn coerce_union(value: &Value, schemas: &[Value]) -> Value {
    if schemas.iter().any(|schema| schema_valid(schema, value)) {
        return value.clone();
    }
    for schema in schemas {
        let candidate = coerce(value.clone(), schema);
        if schema_valid(schema, &candidate) {
            return candidate;
        }
    }
    value.clone()
}

fn coerce(mut value: Value, schema: &Value) -> Value {
    let Some(schema) = schema.as_object() else {
        return value;
    };
    if let Some(all_of) = schema.get("allOf").and_then(Value::as_array) {
        for nested in all_of {
            value = coerce(value, nested);
        }
    }
    if let Some(any_of) = schema.get("anyOf").and_then(Value::as_array) {
        value = coerce_union(&value, any_of);
    }
    if let Some(one_of) = schema.get("oneOf").and_then(Value::as_array) {
        value = coerce_union(&value, one_of);
    }

    let kinds = schema_types(schema);
    let matches_union = kinds.len() > 1 && kinds.iter().any(|kind| matches_type(&value, kind));
    if !kinds.is_empty() && !matches_union {
        for kind in &kinds {
            let candidate = coerce_primitive(&value, kind);
            if candidate != value {
                value = candidate;
                break;
            }
        }
    }

    if kinds.contains(&"object")
        && let Value::Object(object) = &mut value
    {
        let properties = schema.get("properties").and_then(Value::as_object);
        if let Some(properties) = properties {
            for (key, property_schema) in properties {
                if let Some(property) = object.get_mut(key) {
                    *property = coerce(property.clone(), property_schema);
                }
            }
        }
        if let Some(additional) = schema
            .get("additionalProperties")
            .filter(|value| value.is_object())
        {
            let defined =
                properties.map_or_else(Vec::new, |properties| properties.keys().cloned().collect());
            for (key, property) in object {
                if !defined.contains(key) {
                    *property = coerce(property.clone(), additional);
                }
            }
        }
    }
    if kinds.contains(&"array")
        && let Value::Array(array) = &mut value
        && let Some(items) = schema.get("items")
    {
        if let Some(tuple) = items.as_array() {
            for (value, item_schema) in array.iter_mut().zip(tuple) {
                *value = coerce(value.clone(), item_schema);
            }
        } else if items.is_object() || items.is_boolean() {
            for value in array {
                *value = coerce(value.clone(), items);
            }
        }
    }
    value
}

fn normalize_optional_nulls(value: &mut Value, schema: &Value) {
    let Some(schema) = schema.as_object() else {
        return;
    };
    if let Value::Array(values) = value
        && let Some(items) = schema.get("items")
    {
        if let Some(tuple) = items.as_array() {
            for (value, item_schema) in values.iter_mut().zip(tuple) {
                normalize_optional_nulls(value, item_schema);
            }
        } else {
            for value in values {
                normalize_optional_nulls(value, items);
            }
        }
        return;
    }
    let (Value::Object(object), Some(properties)) =
        (value, schema.get("properties").and_then(Value::as_object))
    else {
        return;
    };
    let required = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>();
    let mut remove = Vec::new();
    for (key, property_schema) in properties {
        let Some(property) = object.get_mut(key) else {
            continue;
        };
        if property.is_null()
            && !required.contains(&key.as_str())
            && property_schema
                .get("$ref")
                .and_then(Value::as_str)
                .is_none()
            && !schema_valid(property_schema, &Value::Null)
        {
            remove.push(key.clone());
        } else {
            normalize_optional_nulls(property, property_schema);
        }
    }
    for key in remove {
        object.remove(&key);
    }
}

pub fn validate_tool_call(
    tools: &[Tool],
    tool_call: &ToolCall,
) -> Result<Value, ToolValidationError> {
    let tool = tools
        .iter()
        .find(|tool| tool.name == tool_call.name)
        .ok_or_else(|| ToolValidationError(format!("Tool \"{}\" not found", tool_call.name)))?;
    validate_tool_arguments(tool, tool_call)
}

pub fn validate_tool_arguments(
    tool: &Tool,
    tool_call: &ToolCall,
) -> Result<Value, ToolValidationError> {
    let mut arguments = tool_call.arguments.clone();
    normalize_optional_nulls(&mut arguments, &tool.parameters);
    arguments = coerce(arguments, &tool.parameters);
    let validator = jsonschema::validator_for(&tool.parameters)
        .map_err(|error| ToolValidationError(error.to_string()))?;
    if validator.is_valid(&arguments) {
        return Ok(arguments);
    }
    let errors = validator
        .iter_errors(&arguments)
        .map(|error| {
            let mut path = error
                .instance_path()
                .to_string()
                .trim_start_matches('/')
                .replace('/', ".");
            if let ValidationErrorKind::Required { property } = error.kind()
                && let Some(property) = property.as_str()
            {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(property);
            }
            if path.is_empty() {
                path.push_str("root");
            }
            format!("  - {path}: {error}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    Err(ToolValidationError(format!(
        "Validation failed for tool \"{}\":\n{}\n\nReceived arguments:\n{}",
        tool_call.name,
        if errors.is_empty() {
            "Unknown validation error"
        } else {
            &errors
        },
        serde_json::to_string_pretty(&tool_call.arguments)
            .unwrap_or_else(|_| "undefined".to_owned())
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;
    use serde_json::json;

    fn validate(schema: Value, input: Value) -> Result<Value, ToolValidationError> {
        let tool = Tool {
            name: "echo".to_owned(),
            description: "Echo".to_owned(),
            parameters: json!({"type":"object","properties":{"value":schema},"required":["value"]}),
            constrained_sampling: None,
        };
        validate_tool_arguments(
            &tool,
            &ToolCall::new("tool-1", "echo", json!({"value":input})),
        )
    }

    #[test]
    fn ports_plain_schema_coercions() {
        for (schema, input, expected) in [
            (json!({"type":"number"}), json!("42"), json!(42.0)),
            (json!({"type":"number"}), json!(true), json!(1)),
            (json!({"type":"number"}), Value::Null, json!(0)),
            (json!({"type":"integer"}), json!("42"), json!(42.0)),
            (json!({"type":"boolean"}), json!("true"), json!(true)),
            (json!({"type":"boolean"}), json!("false"), json!(false)),
            (json!({"type":"boolean"}), json!(1), json!(true)),
            (json!({"type":"boolean"}), json!(0), json!(false)),
            (json!({"type":"string"}), Value::Null, json!("")),
            (json!({"type":"string"}), json!(true), json!("true")),
            (json!({"type":"string"}), json!(1.0), json!("1")),
            (json!({"type":"null"}), json!(""), Value::Null),
            (json!({"type":"null"}), json!(0), Value::Null),
            (json!({"type":"null"}), json!(false), Value::Null),
            (json!({"type":["number","string"]}), json!("1"), json!("1")),
            (json!({"type":["boolean","number"]}), json!("1"), json!(1.0)),
        ] {
            assert_eq!(
                validate(schema, input).expect("valid"),
                json!({"value":expected})
            );
        }
        for (schema, input) in [
            (json!({"type":"boolean"}), json!("1")),
            (json!({"type":"boolean"}), json!("0")),
            (json!({"type":"null"}), json!("null")),
            (json!({"type":"integer"}), json!("42.1")),
        ] {
            assert!(validate(schema, input).is_err());
        }
    }

    #[test]
    fn optional_nonnullable_null_is_omitted() {
        let tool = Tool {
            name: "echo".to_owned(),
            description: "Echo".to_owned(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"number"},"nullable":{"type":["string","null"]},"metadata":{"type":"object","properties":{"enabled":{"type":"boolean"}}}},"required":["path","metadata"]}),
            constrained_sampling: None,
        };
        let call = ToolCall::new(
            "1",
            "echo",
            json!({"path":"x","offset":null,"nullable":null,"metadata":{"enabled":null}}),
        );
        assert_eq!(
            validate_tool_arguments(&tool, &call).expect("valid"),
            json!({"path":"x","nullable":null,"metadata":{}})
        );
    }

    /// Ports pi `test/validation.test.ts:126-192`.
    #[test]
    fn nullable_refs_unions_and_arrays_preserve_null_and_coerce_other_arms() {
        let referenced = Tool {
            name: "echo".to_owned(),
            description: "Echo".to_owned(),
            parameters: json!({
                "type":"object",
                "properties":{"value":{"$ref":"#/$defs/value"}},
                "$defs":{"value":{"anyOf":[{"type":"number"},{"type":"null"}]}}
            }),
            constrained_sampling: None,
        };
        assert_eq!(
            validate_tool_arguments(
                &referenced,
                &ToolCall::new("1", "echo", json!({"value":null}))
            )
            .expect("valid"),
            json!({"value":null})
        );
        assert_eq!(
            validate(
                json!({"oneOf":[{"type":"number"},{"type":"null"}]}),
                Value::Null
            )
            .expect("valid"),
            json!({"value":null})
        );
        assert_eq!(
            validate(
                json!({"anyOf":[{"type":"number"},{"type":"null"}]}),
                json!("42")
            )
            .expect("valid"),
            json!({"value":42.0})
        );
        assert_eq!(
            validate(
                json!({"type":["array","null"],"items":{"type":"string"}}),
                Value::Null
            )
            .expect("valid"),
            json!({"value":null})
        );
    }
}
