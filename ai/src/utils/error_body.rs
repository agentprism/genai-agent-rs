//! Provider HTTP error normalization ⇐ pi `src/utils/error-body.ts`.

use serde_json::Value;

pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4_000;

pub(crate) fn trim_javascript_whitespace(value: &str) -> &str {
    value.trim_matches(|character| {
        matches!(
            character,
            '\u{0009}'
                | '\u{000a}'
                | '\u{000b}'
                | '\u{000c}'
                | '\u{000d}'
                | '\u{0020}'
                | '\u{00a0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200a}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202f}'
                    | '\u{205f}'
                    | '\u{3000}'
                    | '\u{feff}'
        )
    })
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderErrorBody {
    Text(String),
    Parsed(Value),
    Opaque,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderErrorData {
    pub message: String,
    pub status_code: Option<i64>,
    pub status: Option<i64>,
    pub body: Option<ProviderErrorBody>,
    pub error: Option<ProviderErrorBody>,
    pub metadata_http_status_code: Option<i64>,
    pub response_status_code: Option<i64>,
    pub response_body: Option<ProviderErrorBody>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedProviderError {
    pub status: Option<i64>,
    pub body: Option<String>,
    pub message: String,
    pub message_carries_body: bool,
}

pub fn normalize_provider_error(error: &ProviderErrorData) -> NormalizedProviderError {
    let status = error
        .status_code
        .or(error.status)
        .or(error.metadata_http_status_code)
        .or(error.response_status_code);
    let body = pick_body_text(error)
        .map(|body| body.trim().to_owned())
        .filter(|body| !body.is_empty())
        .map(|body| truncate_error_text(&body, MAX_PROVIDER_ERROR_BODY_CHARS));
    let message_carries_body = body
        .as_ref()
        .is_none_or(|body| error.message.contains(body));

    NormalizedProviderError {
        status,
        body,
        message: error.message.clone(),
        message_carries_body,
    }
}

pub fn normalize_provider_error_value(value: &Value) -> NormalizedProviderError {
    NormalizedProviderError {
        status: None,
        body: None,
        message: safe_json_stringify(value),
        message_carries_body: false,
    }
}

fn pick_body_text(error: &ProviderErrorData) -> Option<String> {
    match error.body.as_ref() {
        Some(ProviderErrorBody::Text(body)) => return Some(body.clone()),
        Some(ProviderErrorBody::Parsed(_)) | Some(ProviderErrorBody::Opaque) | None => {}
    }
    if let Some(ProviderErrorBody::Parsed(value)) = error.error.as_ref()
        && is_plain_nonempty_object(value)
    {
        return Some(safe_json_stringify(value));
    }
    match error.response_body.as_ref() {
        Some(ProviderErrorBody::Text(body)) => Some(body.clone()),
        Some(ProviderErrorBody::Parsed(value)) if is_plain_nonempty_object(value) => {
            Some(safe_json_stringify(value))
        }
        Some(ProviderErrorBody::Parsed(_)) | Some(ProviderErrorBody::Opaque) | None => None,
    }
}

fn is_plain_nonempty_object(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}

pub fn format_provider_error(error: &NormalizedProviderError, prefix: Option<&str>) -> String {
    if error.message_carries_body || error.status.is_none() || error.body.is_none() {
        return match (prefix, error.status) {
            (Some(prefix), Some(status)) => format!("{prefix} ({status}): {}", error.message),
            _ => error.message.clone(),
        };
    }

    let status = error.status.expect("checked above");
    let body = error.body.as_deref().expect("checked above");
    match prefix {
        Some(prefix) => format!("{prefix} ({status}): {body}"),
        None => format!("{status}: {body}"),
    }
}

pub fn truncate_error_text(text: &str, max_chars: usize) -> String {
    let utf16_len = text.encode_utf16().count();
    if utf16_len <= max_chars {
        return text.to_owned();
    }

    let mut end = 0;
    let mut units = 0;
    for (index, character) in text.char_indices() {
        let width = character.len_utf16();
        if units + width > max_chars {
            break;
        }
        units += width;
        end = index + character.len_utf8();
    }
    format!(
        "{}... [truncated {} chars]",
        &text[..end],
        utf16_len - max_chars
    )
}

pub fn safe_json_stringify(value: &Value) -> String {
    serde_json::to_string(&normalize_json_numbers(value)).unwrap_or_else(|_| value.to_string())
}

pub fn js_number_string(number: &serde_json::Number) -> String {
    let Some(value) = number.as_f64() else {
        return number.to_string();
    };
    if value == 0.0 {
        return "0".to_owned();
    }
    if value.fract() == 0.0 && value.abs() < 1e21 {
        return format!("{value:.0}");
    }
    let rendered = number.to_string();
    let Some((mantissa, exponent)) = rendered
        .split_once('e')
        .or_else(|| rendered.split_once('E'))
    else {
        return rendered;
    };
    let Ok(exponent) = exponent.parse::<i32>() else {
        return rendered;
    };
    if (-6..21).contains(&exponent) {
        let negative = mantissa.starts_with('-');
        let mantissa = mantissa.trim_start_matches('-');
        let dot = mantissa.find('.').unwrap_or(mantissa.len());
        let digits = mantissa.replace('.', "");
        let decimal = i32::try_from(dot).unwrap_or(i32::MAX) + exponent;
        let mut output = if decimal <= 0 {
            format!("0.{}{}", "0".repeat((-decimal) as usize), digits)
        } else if usize::try_from(decimal).is_ok_and(|decimal| decimal >= digits.len()) {
            format!(
                "{}{}",
                digits,
                "0".repeat(usize::try_from(decimal).unwrap_or(usize::MAX) - digits.len())
            )
        } else {
            let decimal = usize::try_from(decimal).expect("positive decimal position");
            format!("{}.{}", &digits[..decimal], &digits[decimal..])
        };
        if negative {
            output.insert(0, '-');
        }
        output
    } else {
        format!(
            "{mantissa}e{}{exponent}",
            if exponent >= 0 { "+" } else { "" }
        )
    }
}

pub fn js_f64_string(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_owned()
    } else if number == f64::INFINITY {
        "Infinity".to_owned()
    } else if number == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else {
        js_number_string(&serde_json::Number::from_f64(number).expect("finite JSON number"))
    }
}

fn normalize_json_numbers(value: &Value) -> Value {
    match value {
        Value::Number(number) if number.is_f64() => {
            let value = number.as_f64().expect("f64 JSON number");
            if value == 0.0 {
                Value::from(0)
            } else if value.is_finite()
                && value.fract() == 0.0
                && value >= i64::MIN as f64
                && value <= i64::MAX as f64
            {
                Value::from(value as i64)
            } else {
                Value::Number(number.clone())
            }
        }
        Value::Array(values) => Value::Array(values.iter().map(normalize_json_numbers).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), normalize_json_numbers(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data(message: &str) -> ProviderErrorData {
        ProviderErrorData {
            message: message.to_owned(),
            ..ProviderErrorData::default()
        }
    }

    #[test]
    fn ports_error_body_test_sdk_shapes_and_edges() {
        let mut mistral = data("Mistral request failed");
        mistral.status_code = Some(403);
        mistral.body = Some(ProviderErrorBody::Text(
            r#"{"error":"blocked by gateway WAF"}"#.to_owned(),
        ));
        let norm = normalize_provider_error(&mistral);
        assert_eq!(norm.status, Some(403));
        assert_eq!(
            norm.body.as_deref(),
            Some(r#"{"error":"blocked by gateway WAF"}"#)
        );
        assert!(!norm.message_carries_body);

        let mut openai = data("403 status code (no body)");
        openai.status = Some(403);
        openai.error = Some(ProviderErrorBody::Parsed(
            json!({"error":"blocked by gateway WAF"}),
        ));
        let norm = normalize_provider_error(&openai);
        assert_eq!(
            norm.body.as_deref(),
            Some(r#"{"error":"blocked by gateway WAF"}"#)
        );
        assert!(!norm.message_carries_body);

        let google_body = json!({"error":{"code":403,"message":"Permission denied"}});
        let mut google = data(&google_body.to_string());
        google.status = Some(403);
        let norm = normalize_provider_error(&google);
        assert!(norm.message_carries_body);
        assert_eq!(norm.message, google_body.to_string());

        let mut bedrock = data("UnknownError");
        bedrock.metadata_http_status_code = Some(403);
        bedrock.response_status_code = Some(403);
        bedrock.response_body = Some(ProviderErrorBody::Text(
            r#"{"message":"blocked by gateway WAF"}"#.to_owned(),
        ));
        assert_eq!(
            normalize_provider_error(&bedrock).body.as_deref(),
            Some(r#"{"message":"blocked by gateway WAF"}"#)
        );

        for opaque in [
            "Invocation of model ID anthropic.claude-opus-5 with on-demand throughput isn't supported.",
            "Input is too long for requested model.",
        ] {
            let mut error = data(opaque);
            error.metadata_http_status_code = Some(400);
            error.response_body = Some(ProviderErrorBody::Opaque);
            let norm = normalize_provider_error(&error);
            assert_eq!(norm.body, None);
            assert!(norm.message_carries_body);
        }

        let mut class_error = data("TLS handshake failed");
        class_error.status = Some(502);
        class_error.error = Some(ProviderErrorBody::Opaque);
        assert_eq!(normalize_provider_error(&class_error).body, None);

        let mut plain = data("400 status code (no body)");
        plain.status = Some(400);
        plain.error = Some(ProviderErrorBody::Parsed(
            json!({"message":"schema validation failed","field":"tools[0]"}),
        ));
        assert_eq!(
            normalize_provider_error(&plain).body.as_deref(),
            Some(r#"{"message":"schema validation failed","field":"tools[0]"}"#)
        );

        let non_error = normalize_provider_error_value(&json!({"reason":"boom"}));
        assert_eq!(non_error.message, r#"{"reason":"boom"}"#);
        assert!(!non_error.message_carries_body);

        let mut empty = data("403 status code (no body)");
        empty.status = Some(403);
        empty.error = Some(ProviderErrorBody::Parsed(json!({})));
        let norm = normalize_provider_error(&empty);
        assert_eq!(norm.body, None);
        assert!(norm.message_carries_body);

        let mut long = data("failed");
        long.status_code = Some(500);
        long.body = Some(ProviderErrorBody::Text(
            "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS + 50),
        ));
        assert!(
            normalize_provider_error(&long)
                .body
                .expect("body")
                .contains("... [truncated 50 chars]")
        );

        let mut carried = data("500: upstream exploded");
        carried.status_code = Some(500);
        carried.body = Some(ProviderErrorBody::Text("upstream exploded".to_owned()));
        assert!(normalize_provider_error(&carried).message_carries_body);
    }

    #[test]
    fn ports_error_body_test_formatting_cases() {
        let mut error = data("403 status code (no body)");
        error.status = Some(403);
        error.error = Some(ProviderErrorBody::Parsed(
            json!({"error":"blocked by gateway WAF"}),
        ));
        let norm = normalize_provider_error(&error);
        assert_eq!(
            format_provider_error(&norm, None),
            r#"403: {"error":"blocked by gateway WAF"}"#
        );
        assert_eq!(
            format_provider_error(&norm, Some("OpenAI API error")),
            r#"OpenAI API error (403): {"error":"blocked by gateway WAF"}"#
        );

        let body = json!({"error":{"message":"Permission denied"}}).to_string();
        let mut carried = data(&body);
        carried.status = Some(403);
        assert_eq!(
            format_provider_error(
                &normalize_provider_error(&carried),
                Some("OpenAI API error")
            ),
            format!("OpenAI API error (403): {body}")
        );
        assert_eq!(
            format_provider_error(
                &normalize_provider_error_value(&json!({"reason":"boom"})),
                None
            ),
            r#"{"reason":"boom"}"#
        );
    }

    /// Pins pi `src/utils/error-body.ts:142-145`: JSON.stringify emits integral
    /// finite numbers without a decimal suffix throughout nested error bodies.
    #[test]
    fn safe_json_stringify_uses_json_stringify_number_spelling() {
        assert_eq!(
            safe_json_stringify(&json!({
                "whole": 1.0,
                "nested": [-0.0, 2.5, {"alsoWhole": 3.0}]
            })),
            r#"{"whole":1,"nested":[0,2.5,{"alsoWhole":3}]}"#
        );
    }
}
