//! Constrained tool sampling ⇐ pi `src/api/constrained-sampling.ts`.

use crate::types::{ConstrainedSamplingConfig, StrictPreference, Tool, ToolConstrainedSampling};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

const UNSUPPORTED_STRICT_SCHEMA_KEYS: [&str; 16] = [
    "$ref",
    "$defs",
    "definitions",
    "allOf",
    "oneOf",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
    "unevaluatedProperties",
    "propertyNames",
    "contains",
    "prefixItems",
    "not",
    "if",
    "then",
    "else",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstrainedSamplingError {
    message: String,
    unsupported_strict_schema: bool,
}

impl ConstrainedSamplingError {
    fn unsupported(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unsupported_strict_schema: true,
        }
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            unsupported_strict_schema: false,
        }
    }
}

impl fmt::Display for ConstrainedSamplingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConstrainedSamplingError {}

fn is_structured_schema(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    let structured_type = match schema.get("type") {
        Some(Value::String(value)) => matches!(value.as_str(), "object" | "array"),
        Some(Value::Array(values)) => values.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|value| matches!(value, "object" | "array"))
        }),
        _ => false,
    };
    structured_type || schema.contains_key("properties") || schema.contains_key("items")
}

fn schema_allows_null(schema: &Value) -> bool {
    let Some(schema) = schema.as_object() else {
        return false;
    };
    let null_type = match schema.get("type") {
        Some(Value::String(value)) => value == "null",
        Some(Value::Array(values)) => values.iter().any(|value| value.as_str() == Some("null")),
        _ => false,
    };
    null_type
        || schema.get("const") == Some(&Value::Null)
        || schema
            .get("enum")
            .and_then(Value::as_array)
            .is_some_and(|values| values.contains(&Value::Null))
        || schema
            .get("anyOf")
            .and_then(Value::as_array)
            .is_some_and(|variants| variants.iter().any(schema_allows_null))
}

fn make_json_schema_node_strict(schema: &mut Value) -> Result<(), ConstrainedSamplingError> {
    let Some(object) = schema.as_object_mut() else {
        return Err(ConstrainedSamplingError::unsupported(
            "boolean schemas are unsupported",
        ));
    };

    for key in UNSUPPORTED_STRICT_SCHEMA_KEYS {
        if object.contains_key(key) {
            return Err(ConstrainedSamplingError::unsupported(format!(
                "{key} schemas are unsupported"
            )));
        }
    }

    if let Some(any_of) = object.get_mut("anyOf") {
        let Some(variants) = any_of.as_array_mut() else {
            return Err(ConstrainedSamplingError::unsupported(
                "anyOf must contain at least one schema",
            ));
        };
        if variants.is_empty() {
            return Err(ConstrainedSamplingError::unsupported(
                "anyOf must contain at least one schema",
            ));
        }
        for variant in variants {
            if is_structured_schema(variant) {
                return Err(ConstrainedSamplingError::unsupported(
                    "object and array unions are unsupported",
                ));
            }
            make_json_schema_node_strict(variant)?;
        }
    }

    if let Some(items) = object.get_mut("items") {
        if items.is_array() {
            return Err(ConstrainedSamplingError::unsupported(
                "tuple schemas are unsupported",
            ));
        }
        make_json_schema_node_strict(items)?;
    }

    let is_object_schema = object.get("type").and_then(Value::as_str) == Some("object");
    if object.contains_key("properties") && !is_object_schema {
        return Err(ConstrainedSamplingError::unsupported(
            "properties require type object",
        ));
    }
    if !is_object_schema {
        return Ok(());
    }
    if object
        .get("additionalProperties")
        .is_some_and(|value| value != &Value::Bool(false))
    {
        return Err(ConstrainedSamplingError::unsupported(
            "schema-valued or true additionalProperties is unsupported",
        ));
    }
    if object
        .get("properties")
        .is_some_and(|properties| !properties.is_object())
    {
        return Err(ConstrainedSamplingError::unsupported(
            "object properties must be a schema map",
        ));
    }
    if let Some(required) = object.get("required") {
        let valid = required
            .as_array()
            .is_some_and(|values| values.iter().all(|value| matches!(value, Value::String(_))));
        if !valid {
            return Err(ConstrainedSamplingError::unsupported(
                "object required must be a string array",
            ));
        }
    }

    let property_names = object
        .get("properties")
        .and_then(Value::as_object)
        .map_or_else(Vec::new, |properties| properties.keys().cloned().collect());
    let required = object
        .get("required")
        .and_then(Value::as_array)
        .map_or_else(BTreeSet::new, |values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        });
    if required
        .iter()
        .any(|key| !property_names.iter().any(|property| property == key))
    {
        return Err(ConstrainedSamplingError::unsupported(
            "required contains an unknown property",
        ));
    }

    if let Some(properties) = object.get_mut("properties").and_then(Value::as_object_mut) {
        for (key, property) in properties {
            make_json_schema_node_strict(property)?;
            if !required.contains(key) && !schema_allows_null(property) {
                *property = json!({"anyOf":[property.clone(), {"type":"null"}]});
            }
        }
    }
    object.insert(
        "required".to_owned(),
        Value::Array(property_names.into_iter().map(Value::String).collect()),
    );
    object.insert("additionalProperties".to_owned(), Value::Bool(false));
    Ok(())
}

pub fn make_strict_json_schema(schema: &Value) -> Result<Value, ConstrainedSamplingError> {
    let mut cloned = schema.clone();
    if !cloned.is_object() {
        return Err(ConstrainedSamplingError::unsupported(
            "root schema must have type object",
        ));
    }
    make_json_schema_node_strict(&mut cloned)?;
    if cloned.get("type").and_then(Value::as_str) != Some("object") {
        return Err(ConstrainedSamplingError::unsupported(
            "root schema must have type object",
        ));
    }
    Ok(cloned)
}

pub fn get_json_schema_tool_parameters(
    tool: &Tool,
    strict: Option<bool>,
) -> Result<Value, ConstrainedSamplingError> {
    if strict == Some(true) {
        make_strict_json_schema(&tool.parameters)
    } else {
        Ok(tool.parameters.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrammarConstrainedSamplingFormat {
    Lark,
    Regex,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarConstrainedSampling {
    pub format: GrammarConstrainedSamplingFormat,
    pub definition: String,
    pub input_property: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrammarToolInputJsonBuffer {
    pub input: String,
    pub started: bool,
    pub closed: bool,
}

pub fn get_grammar_tool_input(
    tool_name: &str,
    arguments: &Value,
    input_property: &str,
) -> Result<String, ConstrainedSamplingError> {
    arguments
        .get(input_property)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            ConstrainedSamplingError::invalid(format!(
                "Grammar tool call \"{tool_name}\" requires argument \"{input_property}\" to be a string."
            ))
        })
}

pub fn append_grammar_tool_input_json_delta(
    buffer: &mut GrammarToolInputJsonBuffer,
    input_property: &str,
    next_input: &str,
    close: bool,
) -> Result<Option<String>, ConstrainedSamplingError> {
    if buffer.closed {
        if close && next_input == buffer.input {
            return Ok(None);
        }
        return Err(ConstrainedSamplingError::invalid(format!(
            "grammar tool input for property \"{input_property}\" changed after it was closed"
        )));
    }
    let Some(input_delta) = next_input.strip_prefix(&buffer.input) else {
        return Err(ConstrainedSamplingError::invalid(format!(
            "grammar tool input for property \"{input_property}\" changed non-monotonically"
        )));
    };
    if !close && input_delta.is_empty() {
        return Ok(None);
    }

    let mut delta = String::new();
    if !buffer.started {
        delta.push('{');
        delta.push_str(&serde_json::to_string(input_property).expect("strings serialize"));
        delta.push_str(":\"");
        buffer.started = true;
    }
    let encoded = serde_json::to_string(input_delta).expect("strings serialize");
    delta.push_str(&encoded[1..encoded.len() - 1]);
    buffer.input = next_input.to_owned();
    if close {
        delta.push_str("\"}");
        buffer.closed = true;
    }
    Ok(Some(delta))
}

fn infer_grammar_input_property(tool: &Tool) -> Result<String, ConstrainedSamplingError> {
    let Some(schema) = tool.parameters.as_object() else {
        return Err(ConstrainedSamplingError::invalid(
            "grammar constrained sampling requires an object parameter schema",
        ));
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(ConstrainedSamplingError::invalid(
            "grammar constrained sampling requires an object parameter schema",
        ));
    }
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Err(ConstrainedSamplingError::invalid(
            "grammar constrained sampling requires exactly one required string property",
        ));
    };
    if required.len() != 1 || !required[0].is_string() {
        return Err(ConstrainedSamplingError::invalid(
            "grammar constrained sampling requires exactly one required string property",
        ));
    }
    let input_property = required[0].as_str().expect("checked above");
    let Some(property) = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|properties| properties.get(input_property))
    else {
        return Err(ConstrainedSamplingError::invalid(format!(
            "grammar constrained sampling requires a properties entry for {input_property}"
        )));
    };
    if property.get("type").and_then(Value::as_str) != Some("string") {
        return Err(ConstrainedSamplingError::invalid(format!(
            "grammar constrained sampling property {input_property} must have type string"
        )));
    }
    Ok(input_property.to_owned())
}

pub fn resolve_json_schema_strict_sampling(
    tool: &Tool,
    supports_strict_mode: bool,
) -> Result<Option<bool>, ConstrainedSamplingError> {
    let Some(ToolConstrainedSampling::Config(ConstrainedSamplingConfig::JsonSchema { strict })) =
        tool.constrained_sampling.as_ref()
    else {
        return Ok(None);
    };

    if supports_strict_mode {
        return match make_strict_json_schema(&tool.parameters) {
            Ok(_) => Ok(Some(true)),
            Err(error)
                if error.unsupported_strict_schema && *strict == StrictPreference::Prefer =>
            {
                Ok(None)
            }
            Err(error) if error.unsupported_strict_schema => {
                Err(ConstrainedSamplingError::invalid(format!(
                    "Tool \"{}\" requires JSON-schema constrained sampling, but {}.",
                    tool.name, error
                )))
            }
            Err(error) => Err(error),
        };
    }
    if *strict == StrictPreference::Require {
        return Err(ConstrainedSamplingError::invalid(format!(
            "Tool \"{}\" requires JSON-schema constrained sampling, but strict tools are unsupported.",
            tool.name
        )));
    }
    Ok(None)
}

pub fn resolve_grammar_constrained_sampling(
    tool: &Tool,
    supports_open_ai_grammar_tools: bool,
) -> Result<Option<GrammarConstrainedSampling>, ConstrainedSamplingError> {
    let Some(ToolConstrainedSampling::Config(ConstrainedSamplingConfig::Grammar { variants })) =
        tool.constrained_sampling.as_ref()
    else {
        return Ok(None);
    };
    if !supports_open_ai_grammar_tools {
        return Ok(None);
    }

    let lark = variants
        .openai_lark
        .as_ref()
        .filter(|definition| !definition.trim().is_empty());
    let regex = variants
        .openai_regex
        .as_ref()
        .filter(|definition| !definition.trim().is_empty());
    let (format, definition) = match (lark, regex) {
        (Some(definition), _) => (GrammarConstrainedSamplingFormat::Lark, definition.clone()),
        (None, Some(definition)) => (GrammarConstrainedSamplingFormat::Regex, definition.clone()),
        (None, None) => {
            return Err(ConstrainedSamplingError::invalid(format!(
                "Tool \"{}\" cannot use grammar constrained sampling: no supported grammar variant was provided.",
                tool.name
            )));
        }
    };
    infer_grammar_input_property(tool)
        .map(|input_property| {
            Some(GrammarConstrainedSampling {
                format,
                definition,
                input_property,
            })
        })
        .map_err(|error| {
            ConstrainedSamplingError::invalid(format!(
                "Tool \"{}\" cannot use grammar constrained sampling: {}.",
                tool.name, error
            ))
        })
}

pub fn create_grammar_tool_input_properties(
    tools: Option<&[Tool]>,
    supports_open_ai_grammar_tools: bool,
) -> Result<BTreeMap<String, String>, ConstrainedSamplingError> {
    let mut properties = BTreeMap::new();
    for tool in tools.unwrap_or_default() {
        if let Some(grammar) =
            resolve_grammar_constrained_sampling(tool, supports_open_ai_grammar_tools)?
        {
            properties.insert(tool.name.clone(), grammar.input_property);
        }
    }
    Ok(properties)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GrammarVariants, StrictPreference};

    fn tool(parameters: Value, constrained_sampling: Option<ToolConstrainedSampling>) -> Tool {
        Tool {
            name: "sample_tool".to_owned(),
            description: "Sample tool".to_owned(),
            parameters,
            constrained_sampling,
        }
    }

    fn json_sampling(strict: StrictPreference) -> Option<ToolConstrainedSampling> {
        Some(ToolConstrainedSampling::Config(
            ConstrainedSamplingConfig::JsonSchema { strict },
        ))
    }

    /// Ports direct helper assertions from pi `test/constrained-sampling.test.ts:82-119`.
    #[test]
    fn resolves_supported_constraints_and_fallbacks() {
        let parameters = json!({
            "type":"object",
            "properties":{"payload":{"type":"string"}},
            "required":["payload"],
            "additionalProperties":false
        });
        assert_eq!(
            resolve_json_schema_strict_sampling(
                &tool(parameters.clone(), json_sampling(StrictPreference::Prefer)),
                true
            ),
            Ok(Some(true))
        );
        let error = resolve_json_schema_strict_sampling(
            &tool(parameters.clone(), json_sampling(StrictPreference::Require)),
            false,
        )
        .expect_err("strict unsupported");
        assert!(
            error
                .to_string()
                .contains("Tool \"sample_tool\" requires JSON-schema constrained sampling")
        );

        let grammar = tool(
            parameters,
            Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::Grammar {
                    variants: GrammarVariants {
                        openai_lark: Some("start: /[a-z]+/".to_owned()),
                        openai_regex: None,
                    },
                },
            )),
        );
        let resolved = resolve_grammar_constrained_sampling(&grammar, true)
            .expect("valid")
            .expect("grammar");
        assert_eq!(resolved.format, GrammarConstrainedSamplingFormat::Lark);
        assert_eq!(resolved.definition, "start: /[a-z]+/");
        assert_eq!(resolved.input_property, "payload");
        assert_eq!(
            resolve_grammar_constrained_sampling(&grammar, false),
            Ok(None)
        );

        let disabled = tool(
            json!({"type":"object"}),
            Some(ToolConstrainedSampling::Disabled),
        );
        assert_eq!(
            resolve_json_schema_strict_sampling(&disabled, true),
            Ok(None)
        );
        assert_eq!(
            resolve_grammar_constrained_sampling(&disabled, true),
            Ok(None)
        );
    }

    /// Ports pi `test/constrained-sampling.test.ts:121-146`.
    #[test]
    fn strict_conversion_is_recursive_and_does_not_mutate_tool_schema() {
        let parameters = json!({
            "type":"object",
            "properties":{
                "path":{"type":"string"},
                "offset":{"type":"number"},
                "metadata":{"type":"object","properties":{"enabled":{"type":"boolean"}},"required":[]},
                "nullable":{"anyOf":[{"type":"string"},{"type":"null"}]}
            },
            "required":["path","metadata"]
        });
        let original = parameters.clone();
        let strict = make_strict_json_schema(&parameters).expect("strict");
        assert_eq!(parameters, original);
        assert_eq!(strict["additionalProperties"], false);
        assert_eq!(
            strict["required"],
            json!(["path", "offset", "metadata", "nullable"])
        );
        assert_eq!(
            strict["properties"]["offset"],
            json!({"anyOf":[{"type":"number"},{"type":"null"}]})
        );
        assert_eq!(
            strict["properties"]["metadata"]["additionalProperties"],
            false
        );
        assert_eq!(
            strict["properties"]["metadata"]["required"],
            json!(["enabled"])
        );
        assert_eq!(
            strict["properties"]["nullable"],
            json!({"anyOf":[{"type":"string"},{"type":"null"}]})
        );
    }

    /// Ports pi `test/constrained-sampling.test.ts:148-191`.
    #[test]
    fn unsupported_schemas_fall_back_or_reject_by_preference() {
        let cases = [
            (
                json!({"type":"object","properties":{"metadata":{"type":"object","additionalProperties":{"type":"string"}}},"required":["metadata"]}),
                "additionalProperties is unsupported",
            ),
            (
                json!({"allOf":[{"type":"object"},{"type":"object"}]}),
                "allOf schemas are unsupported",
            ),
            (
                json!({"type":"object","properties":{"value":{"anyOf":[{"type":"object","properties":{"nested":{"type":"string"}}},{"type":"null"}]}},"required":[]}),
                "object and array unions are unsupported",
            ),
            (
                json!({"type":"object","properties":{"child":{"$ref":"https://example.com/child.json"}},"required":["child"]}),
                "$ref schemas are unsupported",
            ),
        ];
        for (parameters, expected) in cases {
            assert!(
                make_strict_json_schema(&parameters)
                    .expect_err("unsupported")
                    .to_string()
                    .contains(expected)
            );
            let prefer = tool(parameters.clone(), json_sampling(StrictPreference::Prefer));
            assert_eq!(resolve_json_schema_strict_sampling(&prefer, true), Ok(None));
            let require = tool(parameters, json_sampling(StrictPreference::Require));
            assert!(
                resolve_json_schema_strict_sampling(&require, true)
                    .expect_err("required")
                    .to_string()
                    .contains(expected)
            );
        }
    }

    /// Ports pi `test/constrained-sampling.test.ts:250-260`.
    #[test]
    fn grammar_json_deltas_are_append_only() {
        let mut buffer = GrammarToolInputJsonBuffer::default();
        let first = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"", false)
            .expect("first")
            .expect("delta");
        let second = append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true)
            .expect("second")
            .expect("delta");
        assert_eq!(
            serde_json::from_str::<Value>(&format!("{first}{second}")).expect("json"),
            json!({"payload":"a\"\nb"})
        );
        assert_eq!(
            append_grammar_tool_input_json_delta(&mut buffer, "payload", "a\"\nb", true),
            Ok(None)
        );
        assert!(
            append_grammar_tool_input_json_delta(&mut buffer, "payload", "changed", true)
                .expect_err("closed")
                .to_string()
                .contains("changed after it was closed")
        );
    }

    /// Derived from pi `src/api/constrained-sampling.ts:145-155,189-205,230-277`.
    #[test]
    fn grammar_validation_and_property_index_match_pi_errors() {
        let grammar = tool(
            json!({"type":"object","properties":{"payload":{"type":"string"}},"required":["payload"]}),
            Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::Grammar {
                    variants: GrammarVariants {
                        openai_lark: None,
                        openai_regex: Some("[a-z]+".to_owned()),
                    },
                },
            )),
        );
        let properties =
            create_grammar_tool_input_properties(Some(std::slice::from_ref(&grammar)), true)
                .expect("properties");
        assert_eq!(
            properties.get("sample_tool").map(String::as_str),
            Some("payload")
        );
        assert_eq!(
            get_grammar_tool_input("sample_tool", &json!({"payload":"abc"}), "payload")
                .expect("input"),
            "abc"
        );
        assert!(
            get_grammar_tool_input("sample_tool", &json!({}), "payload")
                .expect_err("missing")
                .to_string()
                .contains("requires argument \"payload\" to be a string")
        );

        let no_variants = Tool {
            constrained_sampling: Some(ToolConstrainedSampling::Config(
                ConstrainedSamplingConfig::Grammar {
                    variants: GrammarVariants::default(),
                },
            )),
            ..grammar
        };
        assert!(
            resolve_grammar_constrained_sampling(&no_variants, true)
                .expect_err("variant")
                .to_string()
                .contains("no supported grammar variant was provided")
        );
    }
}
