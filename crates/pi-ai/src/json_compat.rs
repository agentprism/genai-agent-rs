//! JavaScript `JSON.stringify`-compatible serialization.
//!
//! Pi uses JavaScript JSON serialization both for provider wire values and for
//! token estimation. Rust's ordinary Serde JSON writer intentionally follows
//! different number-formatting rules, so compatibility-sensitive paths share
//! this writer instead.

use serde::Serialize;
use serde_json::{Map, Number, Value};

/// Serializes a value with the observable `JSON.stringify` rules used by Pi.
///
/// Object insertion order is retained except for ECMAScript array-index keys,
/// which precede other keys in ascending numeric order. Finite numbers are
/// rendered as JavaScript `Number` values, including `-0` becoming `0` and the
/// fixed-versus-exponent thresholds used by `JSON.stringify`.
pub fn json_stringify_compatible<T>(value: &T) -> Result<String, serde_json::Error>
where
    T: Serialize + ?Sized,
{
    let value = serde_json::to_value(value)?;
    let mut output = String::new();
    write_value(&value, &mut output);
    Ok(output)
}

fn write_value(value: &Value, output: &mut String) {
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::Number(value) => write_number(value, output),
        Value::String(value) => {
            output.push_str(
                &serde_json::to_string(value).expect("serializing a JSON string cannot fail"),
            );
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                write_value(value, output);
            }
            output.push(']');
        }
        Value::Object(values) => write_object(values, output),
    }
}

fn write_number(number: &Number, output: &mut String) {
    let Some(value) = number.as_f64() else {
        // JavaScript cannot retain a JSON number outside its finite Number
        // domain. `JSON.stringify` serializes a non-finite Number as null.
        output.push_str("null");
        return;
    };
    if !value.is_finite() {
        output.push_str("null");
        return;
    }
    output.push_str(ryu_js::Buffer::new().format(value));
}

fn write_object(values: &Map<String, Value>, output: &mut String) {
    let mut array_indexes = values
        .iter()
        .filter_map(|(key, value)| ecmascript_array_index(key).map(|index| (index, key, value)))
        .collect::<Vec<_>>();
    array_indexes.sort_unstable_by_key(|(index, _, _)| *index);

    output.push('{');
    let mut first = true;
    for (_, key, value) in array_indexes {
        write_member(key, value, &mut first, output);
    }
    for (key, value) in values {
        if ecmascript_array_index(key).is_none() {
            write_member(key, value, &mut first, output);
        }
    }
    output.push('}');
}

fn write_member(key: &str, value: &Value, first: &mut bool, output: &mut String) {
    if !*first {
        output.push(',');
    }
    *first = false;
    output
        .push_str(&serde_json::to_string(key).expect("serializing a JSON object key cannot fail"));
    output.push(':');
    write_value(value, output);
}

fn ecmascript_array_index(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(index)
}
