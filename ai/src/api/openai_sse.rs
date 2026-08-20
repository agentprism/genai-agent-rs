use crate::types::{
    AbortSignal, FetchFunction, ProviderBodyStream, ProviderHttpRequest, ProviderHttpResponse,
    ProviderResponse,
};
use crate::utils::error_body::{
    ProviderErrorBody, ProviderErrorData, format_provider_error, normalize_provider_error,
    safe_json_stringify,
};
use crate::utils::provider_retry::{ProviderErrorMetadata, ProviderRetryClassify};
use futures::future::pending;
use futures::stream::{BoxStream, StreamExt};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct OpenAiSseRequest {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub fetch: Option<Arc<dyn FetchFunction>>,
    pub signal: Option<Arc<dyn AbortSignal>>,
    pub timeout_ms: Option<u64>,
}

pub(crate) struct AcquiredSse {
    pub response: ProviderResponse,
    pub stream: BoxStream<'static, Result<Value, OpenAiSseError>>,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenAiHttpError {
    metadata: ProviderErrorMetadata,
    provider_data: Box<ProviderErrorData>,
    raw_metadata: Option<Value>,
    aborted: bool,
}

impl OpenAiHttpError {
    fn transport(message: impl Into<String>, aborted: bool) -> Self {
        Self {
            metadata: ProviderErrorMetadata::default(),
            provider_data: Box::new(ProviderErrorData {
                message: message.into(),
                ..ProviderErrorData::default()
            }),
            raw_metadata: None,
            aborted,
        }
    }

    fn http(status: u16, headers: BTreeMap<String, String>, body: &[u8]) -> Self {
        let text = String::from_utf8_lossy(body).into_owned();
        let parsed = serde_json::from_str::<Value>(&text).ok();
        let error = parsed
            .as_ref()
            .and_then(|value| value.get("error"))
            .cloned();
        let message = sdk_error_message(
            status,
            error.as_ref(),
            parsed
                .as_ref()
                .is_none_or(|value| !js_truthy(value))
                .then_some(&text),
        );
        let raw_metadata = error
            .as_ref()
            .and_then(|value| value.get("metadata"))
            .and_then(|value| value.get("raw"))
            .filter(|value| js_truthy(value))
            .cloned();
        Self {
            metadata: ProviderErrorMetadata {
                status: Some(status),
                headers,
            },
            provider_data: Box::new(ProviderErrorData {
                message,
                status: Some(i64::from(status)),
                error: error.map(ProviderErrorBody::Parsed),
                ..ProviderErrorData::default()
            }),
            raw_metadata,
            aborted: false,
        }
    }

    pub fn aborted(&self) -> bool {
        self.aborted
    }

    pub fn formatted(&self, prefix: Option<&str>, append_raw_metadata: bool) -> String {
        let mut message =
            format_provider_error(&normalize_provider_error(&self.provider_data), prefix);
        if append_raw_metadata && let Some(raw) = self.raw_metadata.as_ref() {
            let raw = js_string(raw);
            if !message.contains(&raw) {
                message.push('\n');
                message.push_str(&raw);
            }
        }
        message
    }
}

impl fmt::Display for OpenAiHttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.provider_data.message)
    }
}

impl ProviderRetryClassify for OpenAiHttpError {
    fn provider_error_metadata(&self) -> Option<&ProviderErrorMetadata> {
        Some(&self.metadata)
    }

    fn provider_error_message(&self) -> String {
        self.provider_data.message.clone()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct OpenAiSseError {
    message: String,
    raw_metadata: Option<Value>,
    aborted: bool,
}

impl OpenAiSseError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            raw_metadata: None,
            aborted: false,
        }
    }

    fn api(error: Value) -> Self {
        let message = sdk_stream_error_message(&error);
        let raw_metadata = error
            .get("metadata")
            .and_then(|value| value.get("raw"))
            .filter(|value| js_truthy(value))
            .cloned();
        Self {
            message,
            raw_metadata,
            aborted: false,
        }
    }

    fn aborted() -> Self {
        Self {
            message: "Request was aborted".to_owned(),
            raw_metadata: None,
            aborted: true,
        }
    }

    pub fn aborted_flag(&self) -> bool {
        self.aborted
    }

    pub fn formatted(&self, append_raw_metadata: bool) -> String {
        let mut message = self.message.clone();
        if append_raw_metadata && let Some(raw) = self.raw_metadata.as_ref() {
            let raw = js_string(raw);
            if !message.contains(&raw) {
                message.push('\n');
                message.push_str(&raw);
            }
        }
        message
    }
}

impl fmt::Display for OpenAiSseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

pub(crate) async fn acquire_sse(
    request: &OpenAiSseRequest,
) -> Result<AcquiredSse, OpenAiHttpError> {
    let response = send_request(request).await?;
    if !(200..300).contains(&response.status) {
        let status = response.status;
        let headers = response.headers.clone();
        let body = read_body(response.body, request.signal.clone()).await?;
        return Err(OpenAiHttpError::http(status, headers, &body));
    }
    let metadata = ProviderResponse {
        status: response.status,
        headers: response.headers,
    };
    let stream = response.body.map_or_else(
        || {
            futures::stream::once(async {
                Err(OpenAiSseError::new(
                    "Attempted to iterate over a response with no body",
                ))
            })
            .boxed()
        },
        |body| sse_json_stream(body, request.signal.clone()),
    );
    Ok(AcquiredSse {
        response: metadata,
        stream,
    })
}

async fn send_request(request: &OpenAiSseRequest) -> Result<ProviderHttpResponse, OpenAiHttpError> {
    let send = async {
        if let Some(fetch) = &request.fetch {
            return fetch
                .fetch(ProviderHttpRequest {
                    method: "POST".to_owned(),
                    url: request.url.clone(),
                    headers: request.headers.clone(),
                    body: request.body.clone(),
                    signal: request.signal.clone(),
                })
                .await
                .map_err(|_| OpenAiHttpError::transport("Connection error.", false));
        }

        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        let headers = request_headers(&request.headers)?;
        let response = CLIENT
            .get_or_init(reqwest::Client::new)
            .post(&request.url)
            .headers(headers)
            .body(request.body.clone())
            .send()
            .await
            .map_err(|_| OpenAiHttpError::transport("Connection error.", false))?;
        let status = response.status();
        let headers = headers_to_record(response.headers());
        let body = response
            .bytes_stream()
            .map(|chunk| {
                chunk
                    .map(|bytes| bytes.to_vec())
                    .map_err(|error| error.to_string())
            })
            .boxed();
        Ok(ProviderHttpResponse {
            status: status.as_u16(),
            status_text: status.canonical_reason().unwrap_or_default().to_owned(),
            headers,
            body: Some(body),
        })
    };
    tokio::pin!(send);

    let timeout_ms = request.timeout_ms.unwrap_or(600_000);
    tokio::select! {
        () = wait_for_abort(request.signal.clone()) => {
            Err(OpenAiHttpError::transport("Request was aborted.", true))
        }
        () = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
            Err(OpenAiHttpError::transport("Request timed out.", false))
        }
        response = &mut send => response,
    }
}

fn request_headers(headers: &BTreeMap<String, String>) -> Result<HeaderMap, OpenAiHttpError> {
    let mut result = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|error| OpenAiHttpError::transport(error.to_string(), false))?;
        let value = HeaderValue::from_str(value)
            .map_err(|error| OpenAiHttpError::transport(error.to_string(), false))?;
        result.insert(name, value);
    }
    Ok(result)
}

fn headers_to_record(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_owned(), value.to_owned()))
        })
        .collect()
}

async fn read_body(
    mut body: Option<ProviderBodyStream>,
    signal: Option<Arc<dyn AbortSignal>>,
) -> Result<Vec<u8>, OpenAiHttpError> {
    let Some(body) = body.as_mut() else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::select! {
            () = wait_for_abort(signal.clone()) => {
                return Err(OpenAiHttpError::transport("Request was aborted.", true));
            }
            chunk = body.next() => chunk,
        };
        match chunk {
            Some(Ok(chunk)) => bytes.extend(chunk),
            Some(Err(_)) => return Err(OpenAiHttpError::transport("Connection error.", false)),
            None => return Ok(bytes),
        }
    }
}

async fn wait_for_abort(signal: Option<Arc<dyn AbortSignal>>) {
    match signal {
        Some(signal) => signal.cancelled().await,
        None => pending::<()>().await,
    }
}

fn sse_json_stream(
    body: ProviderBodyStream,
    signal: Option<Arc<dyn AbortSignal>>,
) -> BoxStream<'static, Result<Value, OpenAiSseError>> {
    struct State {
        body: ProviderBodyStream,
        signal: Option<Arc<dyn AbortSignal>>,
        decoder: SseDecoder,
        pending: VecDeque<String>,
        ended: bool,
    }

    futures::stream::unfold(
        State {
            body,
            signal,
            decoder: SseDecoder::default(),
            pending: VecDeque::new(),
            ended: false,
        },
        |mut state| async move {
            if state.ended {
                return None;
            }
            loop {
                if let Some(data) = state.pending.pop_front() {
                    if data.starts_with("[DONE]") {
                        return None;
                    }
                    let item = if data.is_empty() {
                        Err(OpenAiSseError::new("Unexpected end of JSON input"))
                    } else {
                        serde_json::from_str(&data)
                            .map_err(|error| OpenAiSseError::new(error.to_string()))
                            .and_then(|value: Value| {
                                value
                                    .get("error")
                                    .filter(|error| js_truthy(error))
                                    .cloned()
                                    .map_or(Ok(value), |error| Err(OpenAiSseError::api(error)))
                            })
                    };
                    if item.is_err() {
                        state.ended = true;
                    }
                    return Some((item, state));
                }
                let chunk = tokio::select! {
                    () = wait_for_abort(state.signal.clone()) => {
                        state.ended = true;
                        return Some((Err(OpenAiSseError::aborted()), state));
                    }
                    chunk = state.body.next() => chunk,
                };
                match chunk {
                    Some(Ok(bytes)) => state.pending.extend(state.decoder.push(&bytes, false)),
                    Some(Err(_)) => {
                        state.ended = true;
                        return Some((Err(OpenAiSseError::new("Connection error.")), state));
                    }
                    None => {
                        state.pending.extend(state.decoder.push(&[], true));
                        if state.pending.is_empty() {
                            return None;
                        }
                    }
                }
            }
        },
    )
    .boxed()
}

#[derive(Default)]
struct SseDecoder {
    buffer: Vec<u8>,
    event: Option<Vec<u8>>,
    data_lines: Vec<Vec<u8>>,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8], eof: bool) -> Vec<String> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(line) = self.take_line(eof) {
            if line.is_empty() {
                self.dispatch(&mut events);
                continue;
            }
            if line.first() == Some(&b':') {
                continue;
            }
            let colon = line.iter().position(|byte| *byte == b':');
            let (field, mut value) = colon.map_or_else(
                || (line.as_slice(), &[][..]),
                |index| (&line[..index], &line[index + 1..]),
            );
            if value.first() == Some(&b' ') {
                value = &value[1..];
            }
            if field == b"data" {
                self.data_lines.push(value.to_vec());
            } else if field == b"event" {
                self.event = Some(value.to_vec());
            }
        }
        events
    }

    fn take_line(&mut self, eof: bool) -> Option<Vec<u8>> {
        let index = self
            .buffer
            .iter()
            .position(|byte| matches!(byte, b'\r' | b'\n'));
        let Some(index) = index else {
            return (eof && !self.buffer.is_empty()).then(|| self.buffer.drain(..).collect());
        };
        if self.buffer[index] == b'\r' && index + 1 == self.buffer.len() && !eof {
            return None;
        }
        let delimiter = if self.buffer[index] == b'\r' && self.buffer.get(index + 1) == Some(&b'\n')
        {
            2
        } else {
            1
        };
        let line = self.buffer.drain(..index).collect();
        self.buffer.drain(..delimiter);
        Some(line)
    }

    fn dispatch(&mut self, events: &mut Vec<String>) {
        let has_event = self.event.as_ref().is_some_and(|event| !event.is_empty());
        self.event = None;
        if self.data_lines.is_empty() && !has_event {
            return;
        }
        let length = self.data_lines.iter().map(Vec::len).sum::<usize>()
            + self.data_lines.len().saturating_sub(1);
        let mut data = Vec::with_capacity(length);
        for (index, line) in self.data_lines.drain(..).enumerate() {
            if index > 0 {
                data.push(b'\n');
            }
            data.extend(line);
        }
        events.push(String::from_utf8_lossy(&data).into_owned());
    }
}

fn sdk_error_message(status: u16, error: Option<&Value>, text: Option<&String>) -> String {
    let detail = error.and_then(|error| {
        error
            .get("message")
            .filter(|message| js_truthy(message))
            .map(|message| match message {
                Value::String(message) => message.clone(),
                _ => safe_json_stringify(message),
            })
            .or_else(|| js_truthy(error).then(|| safe_json_stringify(error)))
    });
    detail
        .or_else(|| {
            text.filter(|text| !text.is_empty())
                .map(|text| (*text).clone())
        })
        .map_or_else(
            || format!("{status} status code (no body)"),
            |detail| format!("{status} {detail}"),
        )
}

fn sdk_stream_error_message(error: &Value) -> String {
    error
        .get("message")
        .filter(|message| js_truthy(message))
        .map(|message| match message {
            Value::String(message) => message.clone(),
            _ => safe_json_stringify(message),
        })
        .unwrap_or_else(|| safe_json_stringify(error))
}

fn js_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value
            .as_f64()
            .is_some_and(|value| value != 0.0 && !value.is_nan()),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn js_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(_) => safe_json_stringify(value),
        Value::String(value) => value.clone(),
        Value::Array(values) => values
            .iter()
            .map(|value| match value {
                Value::Null => String::new(),
                value => js_string(value),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pins pi OpenAI SDK framing at `src/api/openai-completions.ts:505-506`
    /// and the SSE behavior audited against `openai`'s streaming decoder.
    #[test]
    fn decoder_preserves_split_utf8_and_joins_multiline_data() {
        let wire = "data: {\"text\":\"café\",\ndata: \"done\":true}\n\n".as_bytes();
        let split = wire
            .windows(2)
            .position(|window| window == "é".as_bytes())
            .expect("multibyte scalar")
            + 1;
        let mut decoder = SseDecoder::default();
        assert!(decoder.push(&wire[..split], false).is_empty());
        assert_eq!(
            decoder.push(&wire[split..], false),
            vec!["{\"text\":\"café\",\n\"done\":true}".to_owned()]
        );
    }

    /// Pins pi OpenAI SDK `core/streaming.js:255-267`: a truthy `event:` field
    /// dispatches even without `data:`, so JSON.parse receives an empty string.
    #[tokio::test]
    async fn event_only_message_surfaces_the_json_parse_error() {
        let body =
            futures::stream::iter(vec![Ok::<_, String>(b"event: future\n\n".to_vec())]).boxed();
        let mut stream = sse_json_stream(body, None);
        let error = stream.next().await.expect("dispatched event").unwrap_err();
        assert_eq!(error.to_string(), "Unexpected end of JSON input");
        assert!(stream.next().await.is_none());
    }

    /// Pins pi OpenAI SDK `core/error.js:21-37`,
    /// `src/utils/error-body.ts:142-145`, and
    /// `src/api/openai-completions.ts:678-680` number stringification.
    #[tokio::test]
    async fn streamed_api_error_matches_sdk_stringification() {
        let error = OpenAiSseError::api(json!({
            "message": 1.0,
            "metadata": {"raw": 2.0}
        }));
        assert_eq!(error.to_string(), "1");
        assert_eq!(error.formatted(true), "1\n2");

        let body = futures::stream::iter(vec![Ok::<_, String>(
            b"data: {\"error\":\"scalar failure\"}\n\n".to_vec(),
        )])
        .boxed();
        let mut stream = sse_json_stream(body, None);
        let error = stream.next().await.expect("dispatched event").unwrap_err();
        assert_eq!(error.to_string(), r#""scalar failure""#);
    }

    /// Pins pi `src/api/openai-completions.ts:672-681`,
    /// `src/api/openai-responses.ts:88-90`, and `src/utils/error-body.ts:38-53`.
    #[test]
    fn http_error_formatting_retains_parsed_and_raw_bodies() {
        let body = br#"{"error":{"message":"bad request","param":"input[0]","metadata":{"raw":{"upstream":"detail"}}}}"#;
        let error = OpenAiHttpError::http(400, BTreeMap::new(), body);
        assert_eq!(
            error.formatted(None, true),
            concat!(
                r#"400: {"message":"bad request","param":"input[0]","metadata":{"raw":{"upstream":"detail"}}}"#,
                "\n[object Object]"
            )
        );
        let error = OpenAiHttpError::http(502, BTreeMap::new(), b"upstream exploded");
        assert_eq!(
            error.formatted(Some("OpenAI API error"), false),
            "OpenAI API error (502): 502 upstream exploded"
        );
    }
}
