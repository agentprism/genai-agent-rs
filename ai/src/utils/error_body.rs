//! Provider HTTP error normalization ⇐ pi `src/utils/error-body.ts`.

use serde_json::Value;

pub const MAX_PROVIDER_ERROR_BODY_CHARS: usize = 4_000;

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
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
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
}
