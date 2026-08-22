//! Assistant diagnostics ⇐ pi `src/utils/diagnostics.ts`.

use crate::types::{
    AssistantMessage, AssistantMessageDiagnostic, DiagnosticCode, DiagnosticErrorInfo, JsonObject,
};
use std::any::Any;
use std::any::type_name_of_val;
use std::backtrace::Backtrace;
use std::error::Error;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn format_thrown_value(value: &dyn std::fmt::Display) -> String {
    value.to_string()
}

pub fn extract_diagnostic_error<E>(error: &E) -> DiagnosticErrorInfo
where
    E: Error + 'static + ?Sized,
{
    let type_name = type_name_of_val(error)
        .rsplit("::")
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_owned);
    let message = match error.to_string() {
        message if message.is_empty() => type_name.clone().unwrap_or_else(|| "Error".to_owned()),
        message => message,
    };
    DiagnosticErrorInfo {
        name: type_name.map(Into::into),
        message: message.into(),
        stack: Some(Backtrace::force_capture().to_string().into()),
        code: None,
    }
}

pub fn create_assistant_message_diagnostic<E>(
    kind: impl Into<crate::types::JsString>,
    error: &E,
    details: Option<JsonObject>,
) -> AssistantMessageDiagnostic
where
    E: Error + 'static + ?Sized,
{
    AssistantMessageDiagnostic {
        kind: kind.into(),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
            * 1_000.0,
        error: Some(extract_diagnostic_error(error)),
        details,
    }
}

pub fn append_assistant_message_diagnostic(
    message: &mut AssistantMessage,
    diagnostic: AssistantMessageDiagnostic,
) {
    message
        .diagnostics
        .get_or_insert_with(Vec::new)
        .push(diagnostic);
}

pub fn diagnostic_code_string(value: impl Into<crate::types::JsString>) -> DiagnosticCode {
    DiagnosticCode::String(value.into())
}

pub fn format_panic_payload(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|value| (*value).to_owned())
        })
        .unwrap_or_else(|| "Rust task panicked".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::JsonValue;
    use std::fmt;

    #[derive(Debug)]
    struct NamedError;

    impl fmt::Display for NamedError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("failure")
        }
    }

    impl Error for NamedError {}

    #[derive(Debug)]
    struct EmptyError;

    impl fmt::Display for EmptyError {
        fn fmt(&self, _formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            Ok(())
        }
    }

    impl Error for EmptyError {}

    /// Pins pi `src/utils/diagnostics.ts:20-38` for Rust error values.
    #[test]
    fn extracts_and_appends_error_diagnostics() {
        let diagnostic = create_assistant_message_diagnostic(
            "transport",
            &NamedError,
            Some(JsonObject::from_iter([("attempt".into(), JsonValue::from(1))])),
        );
        let error = diagnostic.error.as_ref().expect("error");
        assert_eq!(error.name.as_deref(), Some("NamedError"));
        assert_eq!(error.message, "failure");
        assert!(error.stack.as_ref().is_some_and(|stack| !stack.is_empty()));
        let mut message = AssistantMessage::pending("api", "provider", "model", 1.0);
        append_assistant_message_diagnostic(&mut message, diagnostic.clone());
        append_assistant_message_diagnostic(&mut message, diagnostic);
        assert_eq!(message.diagnostics.expect("diagnostics").len(), 2);
    }

    /// Pins pi `src/utils/diagnostics.ts:26`'s empty-message fallback.
    #[test]
    fn empty_error_message_falls_back_to_name() {
        assert_eq!(extract_diagnostic_error(&EmptyError).message, "EmptyError");
    }
}
