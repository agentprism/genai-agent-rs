//! Provider HTTP error normalization ⇐ pi `src/utils/error-body.ts`.

use serde_json::Value;

use crate::types::{JsString, JsonValue};
use crate::utils::ecma_json::{format_number, stringify};

pub const MAX_PROVIDER_ERROR_BODY_CHARS: f64 = 4_000.0;

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
    Text(JsString),
    Parsed(Value),
    Opaque,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProviderErrorData {
    pub message: String,
    pub status_code: Option<f64>,
    pub status: Option<f64>,
    pub body: Option<ProviderErrorBody>,
    pub error: Option<ProviderErrorBody>,
    pub metadata_http_status_code: Option<f64>,
    pub response_status_code: Option<f64>,
    pub response_body: Option<ProviderErrorBody>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NormalizedProviderError {
    pub status: Option<f64>,
    pub body: Option<JsString>,
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
        .map(|body| trim_javascript_whitespace_js(&body))
        .filter(|body| !body.is_empty())
        .map(|body| truncate_error_text(&body, MAX_PROVIDER_ERROR_BODY_CHARS));
    let message_carries_body = body
        .as_ref()
        .is_none_or(|body| JsString::from(&error.message).contains(body));

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

fn pick_body_text(error: &ProviderErrorData) -> Option<JsString> {
    match error.body.as_ref() {
        Some(ProviderErrorBody::Text(body)) => return Some(body.clone()),
        Some(ProviderErrorBody::Parsed(_)) | Some(ProviderErrorBody::Opaque) | None => {}
    }
    if let Some(ProviderErrorBody::Parsed(value)) = error.error.as_ref()
        && is_plain_nonempty_object(value)
    {
        return Some(safe_json_stringify(value).into());
    }
    match error.response_body.as_ref() {
        Some(ProviderErrorBody::Text(body)) => Some(body.clone()),
        Some(ProviderErrorBody::Parsed(value)) if is_plain_nonempty_object(value) => {
            Some(safe_json_stringify(value).into())
        }
        Some(ProviderErrorBody::Parsed(_)) | Some(ProviderErrorBody::Opaque) | None => None,
    }
}

fn trim_javascript_whitespace_js(value: &JsString) -> JsString {
    let is_whitespace = |unit: &u16| {
        matches!(
            unit,
            0x0009..=0x000d
                | 0x0020
                | 0x00a0
                | 0x1680
                | 0x2000..=0x200a
                | 0x2028
                | 0x2029
                | 0x202f
                | 0x205f
                | 0x3000
                | 0xfeff
        )
    };
    let start = value
        .as_utf16()
        .iter()
        .position(|unit| !is_whitespace(unit))
        .unwrap_or(value.len());
    let end = value
        .as_utf16()
        .iter()
        .rposition(|unit| !is_whitespace(unit))
        .map_or(start, |index| index + 1);
    value.slice(start, end)
}

fn is_plain_nonempty_object(value: &Value) -> bool {
    value.as_object().is_some_and(|object| !object.is_empty())
}

pub fn format_provider_error(error: &NormalizedProviderError, prefix: Option<&str>) -> JsString {
    if error.message_carries_body || error.status.is_none() || error.body.is_none() {
        return match (prefix, error.status) {
            (Some(prefix), Some(status)) => {
                format!("{prefix} ({}): {}", js_f64_string(status), error.message).into()
            }
            _ => error.message.clone().into(),
        };
    }

    let status = js_f64_string(error.status.unwrap_or_default());
    let mut output = match prefix {
        Some(prefix) => JsString::from(format!("{prefix} ({status}): ")),
        None => JsString::from(format!("{status}: ")),
    };
    if let Some(body) = error.body.as_ref() {
        output.push_str(body);
    }
    output
}

pub fn truncate_error_text(text: &JsString, max_chars: f64) -> JsString {
    let length = text.len() as f64;
    if length <= max_chars {
        return text.clone();
    }
    let end = javascript_slice_index(max_chars, text.len());
    let mut output = text.slice(0, end);
    output.push_utf8(&format!(
        "... [truncated {} chars]",
        js_f64_string(length - max_chars)
    ));
    output
}

fn javascript_slice_index(value: f64, length: usize) -> usize {
    if value.is_nan() || value == 0.0 || value == f64::NEG_INFINITY {
        return 0;
    }
    if value == f64::INFINITY {
        return length;
    }
    let integer = value.trunc();
    if integer < 0.0 {
        ((length as f64 + integer).max(0.0).min(length as f64)) as usize
    } else {
        integer.min(length as f64) as usize
    }
}

pub fn safe_json_stringify(value: &Value) -> String {
    stringify(&JsonValue::from(value.clone()))
}

pub fn js_number_string(number: &serde_json::Number) -> String {
    let Some(value) = number.as_f64() else {
        return number.to_string();
    };
    format_number(value)
}

pub fn js_f64_string(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_owned()
    } else if number == f64::INFINITY {
        "Infinity".to_owned()
    } else if number == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else {
        format_number(number)
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
        mistral.status_code = Some(403.0);
        mistral.body = Some(ProviderErrorBody::Text(
            r#"{"error":"blocked by gateway WAF"}"#.into(),
        ));
        let norm = normalize_provider_error(&mistral);
        assert_eq!(norm.status, Some(403.0));
        assert_eq!(
            norm.body.as_deref(),
            Some(r#"{"error":"blocked by gateway WAF"}"#)
        );
        assert!(!norm.message_carries_body);

        let mut openai = data("403 status code (no body)");
        openai.status = Some(403.0);
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
        google.status = Some(403.0);
        let norm = normalize_provider_error(&google);
        assert!(norm.message_carries_body);
        assert_eq!(norm.message, google_body.to_string());

        let mut bedrock = data("UnknownError");
        bedrock.metadata_http_status_code = Some(403.0);
        bedrock.response_status_code = Some(403.0);
        bedrock.response_body = Some(ProviderErrorBody::Text(
            r#"{"message":"blocked by gateway WAF"}"#.into(),
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
            error.metadata_http_status_code = Some(400.0);
            error.response_body = Some(ProviderErrorBody::Opaque);
            let norm = normalize_provider_error(&error);
            assert_eq!(norm.body, None);
            assert!(norm.message_carries_body);
        }

        let mut class_error = data("TLS handshake failed");
        class_error.status = Some(502.0);
        class_error.error = Some(ProviderErrorBody::Opaque);
        assert_eq!(normalize_provider_error(&class_error).body, None);

        let mut plain = data("400 status code (no body)");
        plain.status = Some(400.0);
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
        empty.status = Some(403.0);
        empty.error = Some(ProviderErrorBody::Parsed(json!({})));
        let norm = normalize_provider_error(&empty);
        assert_eq!(norm.body, None);
        assert!(norm.message_carries_body);

        let mut long = data("failed");
        long.status_code = Some(500.0);
        long.body = Some(ProviderErrorBody::Text(
            "x".repeat(MAX_PROVIDER_ERROR_BODY_CHARS as usize + 50).into(),
        ));
        assert!(
            normalize_provider_error(&long)
                .body
                .expect("body")
                .contains("... [truncated 50 chars]")
        );

        let mut carried = data("500: upstream exploded");
        carried.status_code = Some(500.0);
        carried.body = Some(ProviderErrorBody::Text("upstream exploded".into()));
        assert!(normalize_provider_error(&carried).message_carries_body);
    }

    #[test]
    fn ports_error_body_test_formatting_cases() {
        let mut error = data("403 status code (no body)");
        error.status = Some(403.0);
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
        carried.status = Some(403.0);
        assert_eq!(
            format_provider_error(
                &normalize_provider_error(&carried),
                Some("OpenAI API error")
            ),
            format!("OpenAI API error (403): {body}").as_str()
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

    /// Pins pi `src/utils/error-body.ts:137-139`: `slice` may retain one half
    /// of an astral pair at the UTF-16 cap.
    #[test]
    fn truncation_retains_a_split_surrogate_code_unit() {
        let text = JsString::from(format!("{}😀", "x".repeat(3_999)));
        let truncated = truncate_error_text(&text, 4_000.0);
        assert_eq!(&truncated.as_utf16()[3_999..4_000], &[0xd83d]);
        assert!(
            truncated
                .to_json_source()
                .ends_with("\\ud83d... [truncated 1 chars]")
        );
    }

    /// Pins pi `src/utils/error-body.ts:76-81,137-139`: extraction itself is
    /// lossless when the UTF-16 cap bisects an astral pair.
    #[test]
    fn normalization_and_formatting_retain_split_surrogate_bodies() {
        let mut error = data("upstream failed");
        error.status = Some(500.0);
        error.body = Some(ProviderErrorBody::Text(JsString::from(format!(
            "{}😀",
            "x".repeat(3_999)
        ))));
        let normalized = normalize_provider_error(&error);
        let body = normalized.body.as_ref().expect("body");
        assert_eq!(&body.as_utf16()[3_999..4_000], &[0xd83d]);
        assert!(
            body.to_json_source()
                .ends_with("\\ud83d... [truncated 1 chars]")
        );
        let formatted = format_provider_error(&normalized, None);
        assert_eq!(formatted.as_utf16()[5 + 3_999], 0xd83d);
    }

    /// Pins pi `src/utils/error-body.ts:61-66,128-134`: status is any
    /// JavaScript number and template interpolation uses ECMAScript spelling.
    #[test]
    fn negative_nan_and_negative_zero_statuses_format_like_javascript() {
        for (status, expected) in [
            (-1.5, "-1.5: body"),
            (f64::NAN, "NaN: body"),
            (-0.0, "0: body"),
        ] {
            let normalized = NormalizedProviderError {
                status: Some(status),
                body: Some("body".into()),
                message: "failed".into(),
                message_carries_body: false,
            };
            assert_eq!(format_provider_error(&normalized, None), expected);
        }
    }

    /// Pins pi `src/utils/error-body.ts:137-139`: `maxChars` is an arbitrary
    /// JavaScript number, and both `slice` coercion and suffix arithmetic remain observable.
    #[test]
    fn truncation_limit_uses_javascript_number_and_slice_semantics() {
        let text = JsString::from("abcde");
        for (limit, expected) in [
            (2.75, "ab... [truncated 2.25 chars]"),
            (-1.5, "abcd... [truncated 6.5 chars]"),
            (f64::NEG_INFINITY, "... [truncated Infinity chars]"),
            (f64::NAN, "... [truncated NaN chars]"),
        ] {
            assert_eq!(truncate_error_text(&text, limit), expected, "{limit:?}");
        }
        assert_eq!(truncate_error_text(&text, f64::INFINITY), text);
    }
}
