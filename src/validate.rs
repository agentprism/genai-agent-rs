//! JSON Schema validation and pi-ai-compatible coercion for tool arguments.
//!
//! Validation always operates on a coerced clone while error messages retain the exact received
//! JSON. Schemas that declare `$schema` select their own draft; schemas without it use Draft 7 for
//! compatibility with TypeBox and provider tool declarations.

use crate::{ToolSpec, ValidationError};
use jsonschema::error::{ValidationError as JsonSchemaValidationError, ValidationErrorKind};
use serde_json::{Map, Number, Value};

/// Validate and coerce raw tool arguments against a tool's JSON Schema.
///
/// Coercion follows the compatibility rules used by pi-ai. It recursively visits declared object
/// properties, schema-valued additional properties, homogeneous arrays, and both legacy and
/// Draft 2020-12 tuples. Primitive conversions include finite numeric strings, `0`/`1` booleans,
/// boolean/number stringification, and the pi-ai null defaults. `allOf` arms apply in sequence;
/// `anyOf` and `oneOf` first preserve an already-valid value, then choose the first arm whose
/// independently coerced candidate validates.
///
/// The returned value is the coerced value that satisfies the complete schema. Coercion is
/// attempted on a clone so a failed union arm cannot contaminate another arm. On failure,
/// [`ValidationError::Invalid`] identifies the tool, reports readable instance paths, and includes
/// the original uncoerced arguments. An invalid schema is reported through the same error variant.
pub fn validate_tool_arguments(
    spec: &ToolSpec,
    arguments: Value,
) -> Result<Value, ValidationError> {
    let received_arguments = arguments;
    let validator = compile_schema(&spec.schema).map_err(|error| {
        invalid_error(
            spec,
            &received_arguments,
            format!("  - root: invalid JSON Schema: {error}"),
        )
    })?;

    let coerced_arguments = coerce_with_json_schema(received_arguments.clone(), &spec.schema);
    if validator.is_valid(&coerced_arguments) {
        return Ok(coerced_arguments);
    }

    let errors = validator
        .iter_errors(&coerced_arguments)
        .map(|error| format!("  - {}: {error}", format_validation_path(&error)))
        .collect::<Vec<_>>();
    let errors = if errors.is_empty() {
        "Unknown validation error".to_owned()
    } else {
        errors.join("\n")
    };

    Err(invalid_error(spec, &received_arguments, errors))
}

fn invalid_error(spec: &ToolSpec, received_arguments: &Value, errors: String) -> ValidationError {
    let received = serde_json::to_string_pretty(received_arguments)
        .unwrap_or_else(|_| received_arguments.to_string());
    ValidationError::Invalid {
        tool_name: spec.name.clone(),
        message: format!("\n{errors}\n\nReceived arguments:\n{received}"),
    }
}

fn coerce_with_json_schema(mut value: Value, schema: &Value) -> Value {
    let Some(schema) = schema.as_object() else {
        return value;
    };

    if let Some(schemas) = schema.get("allOf").and_then(Value::as_array) {
        for nested_schema in schemas {
            value = coerce_with_json_schema(value, nested_schema);
        }
    }

    if let Some(schemas) = schema.get("anyOf").and_then(Value::as_array) {
        value = coerce_with_union_schema(&value, schemas);
    }

    if let Some(schemas) = schema.get("oneOf").and_then(Value::as_array) {
        value = coerce_with_union_schema(&value, schemas);
    }

    let schema_types = get_schema_types(schema);
    let already_matches_union_member = schema_types.len() > 1
        && schema_types
            .iter()
            .any(|schema_type| matches_json_type(&value, schema_type));
    if !schema_types.is_empty() && !already_matches_union_member {
        for schema_type in &schema_types {
            if let Some(candidate) = coerce_primitive_by_type(&value, schema_type) {
                value = candidate;
                break;
            }
        }
    }

    if schema_types.contains(&"object") && value.is_object() {
        apply_schema_object_coercion(&mut value, schema);
    }
    if schema_types.contains(&"array") && value.is_array() {
        apply_schema_array_coercion(&mut value, schema);
    }

    value
}

fn get_schema_types(schema: &Map<String, Value>) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(schema_type)) => vec![schema_type],
        Some(Value::Array(schema_types)) => schema_types.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

fn matches_json_type(value: &Value, schema_type: &str) -> bool {
    match schema_type {
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(is_integer_number),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "null" => value.is_null(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        _ => false,
    }
}

fn coerce_primitive_by_type(value: &Value, schema_type: &str) -> Option<Value> {
    match schema_type {
        "number" => match value {
            Value::Null => Some(Value::Number(Number::from(0))),
            Value::String(value) => parse_finite_number(value).map(Value::Number),
            Value::Bool(value) => Some(Value::Number(Number::from(u8::from(*value)))),
            _ => None,
        },
        "integer" => match value {
            Value::Null => Some(Value::Number(Number::from(0))),
            Value::String(value) => parse_finite_number(value)
                .filter(is_integer_number)
                .map(Value::Number),
            Value::Bool(value) => Some(Value::Number(Number::from(u8::from(*value)))),
            _ => None,
        },
        "boolean" => match value {
            Value::Null => Some(Value::Bool(false)),
            Value::String(value) if value == "true" => Some(Value::Bool(true)),
            Value::String(value) if value == "false" => Some(Value::Bool(false)),
            Value::Number(value) if number_equals(value, 1.0) => Some(Value::Bool(true)),
            Value::Number(value) if number_equals(value, 0.0) => Some(Value::Bool(false)),
            _ => None,
        },
        "string" => match value {
            Value::Null => Some(Value::String(String::new())),
            Value::Number(value) => Some(Value::String(number_to_string(value))),
            Value::Bool(value) => Some(Value::String(value.to_string())),
            _ => None,
        },
        "null" => match value {
            Value::String(value) if value.is_empty() => Some(Value::Null),
            Value::Number(value) if number_equals(value, 0.0) => Some(Value::Null),
            Value::Bool(false) => Some(Value::Null),
            _ => None,
        },
        _ => None,
    }
}

fn apply_schema_object_coercion(value: &mut Value, schema: &Map<String, Value>) {
    let Some(value) = value.as_object_mut() else {
        return;
    };
    let properties = schema.get("properties").and_then(Value::as_object);

    if let Some(properties) = properties {
        for (key, property_schema) in properties {
            if let Some(property_value) = value.get_mut(key) {
                *property_value = coerce_with_json_schema(property_value.clone(), property_schema);
            }
        }
    }

    let Some(additional_schema) = schema
        .get("additionalProperties")
        .filter(|schema| schema.is_object())
    else {
        return;
    };
    for (key, property_value) in value {
        if properties.is_some_and(|properties| properties.contains_key(key)) {
            continue;
        }
        *property_value = coerce_with_json_schema(property_value.clone(), additional_schema);
    }
}

fn apply_schema_array_coercion(value: &mut Value, schema: &Map<String, Value>) {
    let Some(value) = value.as_array_mut() else {
        return;
    };

    // Draft 4/6/7 tuple form, also understood by TypeBox's conversion layer.
    if let Some(item_schemas) = schema.get("items").and_then(Value::as_array) {
        for (item, item_schema) in value.iter_mut().zip(item_schemas) {
            *item = coerce_with_json_schema(item.clone(), item_schema);
        }
        return;
    }

    // Draft 2020-12 tuple form. `items` applies only after the prefix.
    if let Some(prefix_schemas) = schema.get("prefixItems").and_then(Value::as_array) {
        for (item, item_schema) in value.iter_mut().zip(prefix_schemas) {
            *item = coerce_with_json_schema(item.clone(), item_schema);
        }
        if let Some(item_schema) = schema.get("items").filter(|schema| schema.is_object()) {
            for item in value.iter_mut().skip(prefix_schemas.len()) {
                *item = coerce_with_json_schema(item.clone(), item_schema);
            }
        }
        return;
    }

    if let Some(item_schema) = schema.get("items").filter(|schema| schema.is_object()) {
        for item in value {
            *item = coerce_with_json_schema(item.clone(), item_schema);
        }
    }
}

fn coerce_with_union_schema(value: &Value, schemas: &[Value]) -> Value {
    // An already-valid arm is always the best match. In particular, this keeps `null` in a
    // nullable union from being converted by an earlier numeric/string arm.
    for schema in schemas {
        if schema_accepts(schema, value) {
            return value.clone();
        }
    }

    // Each arm receives its own clone; a failed arm must not leave partial coercions behind.
    for schema in schemas {
        let candidate = coerce_with_json_schema(value.clone(), schema);
        if schema_accepts(schema, &candidate) {
            return candidate;
        }
    }

    value.clone()
}

fn schema_accepts(schema: &Value, value: &Value) -> bool {
    compile_schema(schema).is_ok_and(|validator| validator.is_valid(value))
}

fn compile_schema(
    schema: &Value,
) -> Result<jsonschema::Validator, JsonSchemaValidationError<'static>> {
    if schema.get("$schema").is_some() {
        jsonschema::validator_for(schema)
    } else {
        // TypeBox and provider tool schemas conventionally use the Draft 7 vocabulary while
        // omitting `$schema` (notably, tuples use an array-valued `items`).
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .build(schema)
    }
}

fn parse_finite_number(value: &str) -> Option<Number> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    let parsed = parse_radix_number(value).or_else(|| value.parse::<f64>().ok())?;
    number_from_f64(parsed)
}

fn parse_radix_number(value: &str) -> Option<f64> {
    let (digits, radix) = if let Some(digits) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        (digits, 16)
    } else if let Some(digits) = value
        .strip_prefix("0b")
        .or_else(|| value.strip_prefix("0B"))
    {
        (digits, 2)
    } else if let Some(digits) = value
        .strip_prefix("0o")
        .or_else(|| value.strip_prefix("0O"))
    {
        (digits, 8)
    } else {
        return None;
    };

    (!digits.is_empty())
        .then(|| {
            u128::from_str_radix(digits, radix)
                .ok()
                .map(|value| value as f64)
        })
        .flatten()
}

fn number_from_f64(value: f64) -> Option<Number> {
    if !value.is_finite() {
        return None;
    }

    // Preserve ordinary integral conversions as JSON integer values. JavaScript has one numeric
    // type, but doing this keeps the equivalent serde_json value natural and lossless.
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
        return if value >= 0.0 {
            Some(Number::from(value as u64))
        } else {
            Some(Number::from(value as i64))
        };
    }

    Number::from_f64(value)
}

fn is_integer_number(value: &Number) -> bool {
    value.is_i64()
        || value.is_u64()
        || value
            .as_f64()
            .is_some_and(|value| value.is_finite() && value.fract() == 0.0)
}

fn number_equals(value: &Number, expected: f64) -> bool {
    value.as_f64() == Some(expected)
}

fn number_to_string(value: &Number) -> String {
    if let Some(value) = value.as_i64() {
        value.to_string()
    } else if let Some(value) = value.as_u64() {
        value.to_string()
    } else if let Some(value) = value.as_f64() {
        if value == 0.0 {
            "0".to_owned()
        } else {
            value.to_string()
        }
    } else {
        value.to_string()
    }
}

fn format_validation_path(error: &JsonSchemaValidationError<'_>) -> String {
    let mut path = json_pointer_to_path(error.instance_path().as_str());
    if let ValidationErrorKind::Required { property } = error.kind()
        && let Some(property) = property.as_str()
    {
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(property);
    }
    if path.is_empty() {
        "root".to_owned()
    } else {
        path
    }
}

fn json_pointer_to_path(pointer: &str) -> String {
    pointer
        .strip_prefix('/')
        .unwrap_or(pointer)
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.replace("~1", "/").replace("~0", "~"))
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(schema: Value) -> ToolSpec {
        ToolSpec::new("echo", "Echo values", schema)
    }

    #[test]
    fn recursively_coerces_primitives_objects_additional_properties_and_arrays() {
        let spec = spec(json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer" },
                "ratio": { "type": "number" },
                "enabled": { "type": "boolean" },
                "label": { "type": "string" },
                "nothing": { "type": "null" },
                "nullable": { "type": ["number", "null"] },
                "flags": {
                    "type": "array",
                    "items": { "type": "boolean" }
                },
                "nested": {
                    "type": "object",
                    "properties": { "known": { "type": "string" } },
                    "additionalProperties": { "type": "integer" }
                }
            },
            "required": [
                "count", "ratio", "enabled", "label", "nothing", "nullable", "flags", "nested"
            ],
            "additionalProperties": false
        }));
        let received = json!({
            "count": "42",
            "ratio": true,
            "enabled": "false",
            "label": 7,
            "nothing": 0,
            "nullable": null,
            "flags": [1, "false", 0],
            "nested": { "known": 9, "extra": "3" }
        });
        let untouched = received.clone();

        let validated = validate_tool_arguments(&spec, received).expect("valid arguments");

        assert_eq!(
            validated,
            json!({
                "count": 42,
                "ratio": 1,
                "enabled": false,
                "label": "7",
                "nothing": null,
                "nullable": null,
                "flags": [true, false, false],
                "nested": { "known": "9", "extra": 3 }
            })
        );
        assert_eq!(
            untouched["count"],
            json!("42"),
            "validation must coerce a clone"
        );
    }

    #[test]
    fn unions_preserve_valid_arms_and_coerce_each_candidate_from_a_clone() {
        let spec = spec(json!({
            "type": "object",
            "properties": {
                "preserved": { "anyOf": [{ "type": "number" }, { "type": "string" }] },
                "coerced": { "anyOf": [{ "type": "number" }, { "type": "null" }] },
                "nullable": { "oneOf": [{ "type": "number" }, { "type": "null" }] },
                "all": { "allOf": [{ "type": "number" }, { "minimum": 1 }] },
                "choice": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": { "value": { "type": "number", "maximum": 0 } },
                            "required": ["value"]
                        },
                        {
                            "type": "object",
                            "properties": { "value": { "type": "string", "const": "true" } },
                            "required": ["value"]
                        }
                    ]
                }
            },
            "required": ["preserved", "coerced", "nullable", "all", "choice"]
        }));

        let validated = validate_tool_arguments(
            &spec,
            json!({
                "preserved": "42",
                "coerced": "42",
                "nullable": null,
                "all": "2",
                "choice": { "value": true }
            }),
        )
        .expect("valid union arguments");

        assert_eq!(validated["preserved"], json!("42"));
        assert_eq!(validated["coerced"], json!(42));
        assert_eq!(validated["nullable"], Value::Null);
        assert_eq!(validated["all"], json!(2));
        assert_eq!(validated["choice"], json!({ "value": "true" }));
    }

    #[test]
    fn coerces_legacy_tuple_array_items() {
        let spec = spec(json!({
            "type": "array",
            "items": [{ "type": "integer" }, { "type": "boolean" }, { "type": "string" }],
            "additionalItems": false,
            "minItems": 3,
            "maxItems": 3
        }));

        let validated =
            validate_tool_arguments(&spec, json!(["4", 1, false])).expect("valid tuple arguments");

        assert_eq!(validated, json!([4, true, "false"]));
    }

    #[test]
    fn rejects_non_coercible_values_with_readable_paths_and_original_arguments() {
        let spec = spec(json!({
            "type": "object",
            "properties": {
                "profile": {
                    "type": "object",
                    "properties": { "age": { "type": "integer" } },
                    "required": ["age"]
                }
            },
            "required": ["profile"]
        }));

        let error = validate_tool_arguments(&spec, json!({ "profile": { "age": "old" } }))
            .expect_err("age cannot be coerced");
        let ValidationError::Invalid { tool_name, message } = error;
        assert_eq!(tool_name, "echo");
        assert!(message.contains("profile.age:"), "{message}");
        assert!(message.contains("Received arguments:"), "{message}");
        assert!(message.contains(r#""age": "old""#), "{message}");

        let error =
            validate_tool_arguments(&spec, json!({ "profile": {} })).expect_err("age is required");
        let ValidationError::Invalid { message, .. } = error;
        assert!(message.contains("profile.age:"), "{message}");
    }

    #[test]
    fn matches_pi_ai_primitive_coercion_table() {
        let cases = [
            (json!({ "type": "number" }), Value::Null, json!(0)),
            (json!({ "type": "number" }), json!("42"), json!(42)),
            (json!({ "type": "number" }), json!(true), json!(1)),
            (json!({ "type": "integer" }), Value::Null, json!(0)),
            (json!({ "type": "integer" }), json!("42"), json!(42)),
            (json!({ "type": "boolean" }), Value::Null, json!(false)),
            (json!({ "type": "boolean" }), json!("true"), json!(true)),
            (json!({ "type": "boolean" }), json!(1), json!(true)),
            (json!({ "type": "string" }), Value::Null, json!("")),
            (json!({ "type": "string" }), json!(true), json!("true")),
            (json!({ "type": "null" }), json!(""), Value::Null),
            (json!({ "type": "null" }), json!(0), Value::Null),
            (json!({ "type": "null" }), json!(false), Value::Null),
            (
                json!({ "type": ["number", "string"] }),
                json!("1"),
                json!("1"),
            ),
            (
                json!({ "type": ["boolean", "number"] }),
                json!("1"),
                json!(1),
            ),
        ];

        for (schema, input, expected) in cases {
            let actual = validate_tool_arguments(&spec(schema), input).expect("valid conversion");
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn rejects_primitive_conversions_not_supported_by_pi_ai() {
        let cases = [
            (json!({ "type": "boolean" }), json!("1")),
            (json!({ "type": "boolean" }), json!("0")),
            (json!({ "type": "null" }), json!("null")),
            (json!({ "type": "integer" }), json!("42.1")),
        ];

        for (schema, value) in cases {
            assert!(
                validate_tool_arguments(&spec(schema), value).is_err(),
                "unsupported conversion should fail"
            );
        }
    }
}
