//! Codex Responses **request body** construction from an [`LlmContext`] + options.
//!
//! Port of pi-ai's `buildRequestBody` (openai-codex-responses.ts:530-597),
//! `convertResponsesMessages` and `convertResponsesTools`
//! (openai-responses-shared.ts:136-380). The wire shape is the OpenAI Responses
//! API request, sent over the ChatGPT Codex backend.
//!
//! Fidelity notes (documented deviations from pi, all inputs the crate does not
//! carry):
//! - Request-body **zstd compression** (`Content-Encoding: zstd`,
//!   openai-codex-responses.ts:382-389) is not implemented; the body is sent as
//!   plain JSON. pi itself falls back to plain JSON when zlib is unavailable, so
//!   the backend accepts it.
//! - **grammar / custom tools** and **deferred tool search** are not emitted;
//!   tools become plain `type:"function"` items.
//! - Assistant **reasoning items** (encrypted_content signatures) are not
//!   replayed, so multi-turn tool continuations that require replaying a prior
//!   `reasoning` item are best-effort.

use genai::chat::{
    BinarySource, ChatMessage, ChatOptions, ChatRole, ContentPart, ReasoningEffort, Tool,
    ToolChoice,
};
use genai::LlmContext;
use serde_json::{Map, Value, json};

/// Instance-level request-body knobs supplied by [`crate::codex::CodexStreamFn`].
#[derive(Clone, Debug)]
pub struct BodyConfig {
    /// `reasoning.summary` value when a reasoning effort is requested
    /// (openai-codex-responses.ts:591, `options.reasoningSummary ?? "auto"`).
    pub reasoning_summary: String,
    /// Default `text.verbosity` when options carry none
    /// (openai-codex-responses.ts:560, `options?.textVerbosity || "low"`).
    pub default_verbosity: String,
    /// Optional `prompt_cache_key` (openai-codex-responses.ts:562).
    pub prompt_cache_key: Option<String>,
}

impl Default for BodyConfig {
    fn default() -> Self {
        Self {
            reasoning_summary: "auto".to_string(),
            default_verbosity: "low".to_string(),
            prompt_cache_key: None,
        }
    }
}

/// Build the Codex Responses request body.
///
/// Mirrors `buildRequestBody` field-for-field (openai-codex-responses.ts:554-596):
/// `model`, `store:false`, `stream:true`, `instructions`, `input`,
/// `text.verbosity`, `include:["reasoning.encrypted_content"]`,
/// `prompt_cache_key`, `tool_choice`, `parallel_tool_calls`, and the optional
/// `temperature`, `service_tier`, `tools`, `reasoning`.
pub fn build_request_body(
    model_id: &str,
    context: &LlmContext,
    options: &ChatOptions,
    cfg: &BodyConfig,
) -> Value {
    let instructions = if context.system_prompt.is_empty() {
        "You are a helpful assistant.".to_string()
    } else {
        context.system_prompt.clone()
    };

    let mut body = Map::new();
    body.insert("model".into(), json!(model_id));
    body.insert("store".into(), json!(false));
    body.insert("stream".into(), json!(true));
    body.insert("instructions".into(), json!(instructions));
    body.insert("input".into(), json!(convert_messages(&context.messages)));
    body.insert(
        "text".into(),
        json!({ "verbosity": verbosity_value(options, cfg) }),
    );
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    if let Some(key) = &cfg.prompt_cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    body.insert("tool_choice".into(), json!(tool_choice_value(options)));
    body.insert("parallel_tool_calls".into(), json!(true));

    if let Some(temperature) = options.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(service_tier) = &options.service_tier {
        body.insert("service_tier".into(), json!(service_tier.variant_name()));
    }
    if !context.tools.is_empty() {
        body.insert("tools".into(), json!(convert_tools(&context.tools)));
    }
    if let Some(reasoning) = reasoning_value(options, cfg) {
        body.insert("reasoning".into(), reasoning);
    }

    Value::Object(body)
}

fn verbosity_value(options: &ChatOptions, cfg: &BodyConfig) -> String {
    options
        .verbosity
        .as_ref()
        .map(|v| v.variant_name().to_string())
        .unwrap_or_else(|| cfg.default_verbosity.clone())
}

fn tool_choice_value(options: &ChatOptions) -> &'static str {
    match options.tool_choice {
        Some(ToolChoice::None) => "none",
        Some(ToolChoice::Required) => "required",
        // Auto (or unset) is pi's default (`options?.toolChoice ?? "auto"`).
        _ => "auto",
    }
}

/// `reasoning: { effort, summary }` when a reasoning effort is requested
/// (openai-codex-responses.ts:583-593). `Budget(_)` has no Responses keyword and
/// `Zero` is normalized to OpenAI's `"none"`.
fn reasoning_value(options: &ChatOptions, cfg: &BodyConfig) -> Option<Value> {
    let effort = options.reasoning_effort.as_ref()?;
    let keyword = match effort {
        ReasoningEffort::Zero => "none",
        other => other.as_keyword()?,
    };
    Some(json!({ "effort": keyword, "summary": cfg.reasoning_summary }))
}

/// Convert genai transcript messages into Responses `input` items.
fn convert_messages(messages: &[ChatMessage]) -> Vec<Value> {
    let mut items = Vec::new();
    let mut msg_index = 0usize;

    for message in messages {
        match message.role {
            // The system instruction is carried in `instructions`, not `input`
            // (pi builds with `includeSystemPrompt: false`).
            ChatRole::System => continue,
            ChatRole::User => convert_user_message(message, &mut items),
            ChatRole::Assistant => convert_assistant_message(message, msg_index, &mut items),
            ChatRole::Tool => convert_tool_message(message, &mut items),
        }
        msg_index += 1;
    }

    items
}

fn convert_user_message(message: &ChatMessage, items: &mut Vec<Value>) {
    let mut content = Vec::new();
    for part in message.content.parts() {
        match part {
            ContentPart::Text(text) => {
                content.push(json!({ "type": "input_text", "text": text }));
            }
            ContentPart::Binary(binary) => {
                let image_url = match &binary.source {
                    BinarySource::Url(url) => url.clone(),
                    BinarySource::Base64(data) => {
                        format!("data:{};base64,{}", binary.content_type, data)
                    }
                };
                content.push(json!({
                    "type": "input_image",
                    "detail": "auto",
                    "image_url": image_url,
                }));
            }
            _ => {}
        }
    }
    if !content.is_empty() {
        items.push(json!({ "role": "user", "content": content }));
    }
}

fn convert_assistant_message(message: &ChatMessage, msg_index: usize, items: &mut Vec<Value>) {
    let mut text_block_index = 0usize;
    for part in message.content.parts() {
        match part {
            ContentPart::Text(text) => {
                // pi derives the id from a text signature, falling back to
                // `msg_pi_{idx}` / `msg_pi_{idx}_{n}` (openai-responses-shared.ts:228-229).
                let id = if text_block_index == 0 {
                    format!("msg_pi_{msg_index}")
                } else {
                    format!("msg_pi_{msg_index}_{text_block_index}")
                };
                text_block_index += 1;
                items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text, "annotations": [] }],
                    "status": "completed",
                    "id": id,
                }));
            }
            ContentPart::ToolCall(tool_call) => {
                let (call_id, item_id) = split_tool_id(&tool_call.call_id);
                let arguments = serde_json::to_string(&tool_call.fn_arguments).unwrap_or_default();
                let mut obj = Map::new();
                obj.insert("type".into(), json!("function_call"));
                // Only forward an fc_* item id; drop other ids to avoid the
                // Responses reasoning/tool pairing validation (openai-responses-shared.ts:257-262).
                if let Some(item_id) = item_id.filter(|id| id.starts_with("fc_")) {
                    obj.insert("id".into(), json!(item_id));
                }
                obj.insert("call_id".into(), json!(call_id));
                obj.insert("name".into(), json!(tool_call.fn_name));
                obj.insert("arguments".into(), json!(arguments));
                items.push(Value::Object(obj));
            }
            // Reasoning/thought-signature replay is intentionally not ported.
            _ => {}
        }
    }
}

fn convert_tool_message(message: &ChatMessage, items: &mut Vec<Value>) {
    for part in message.content.parts() {
        if let ContentPart::ToolResponse(response) = part {
            let (call_id, _) = split_tool_id(&response.call_id);
            // pi's convertToolResultOutput emits "(no tool output)" for an empty
            // result rather than an empty string (openai-responses-shared.ts:88).
            let output = if response.content.is_empty() {
                "(no tool output)"
            } else {
                response.content.as_str()
            };
            items.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }));
        }
    }
}

/// Split a composite tool id `callId|itemId` (openai-responses-shared.ts:248,288).
fn split_tool_id(id: &str) -> (String, Option<String>) {
    match id.split_once('|') {
        Some((call_id, item_id)) => (call_id.to_string(), Some(item_id.to_string())),
        None => (id.to_string(), None),
    }
}

/// Convert genai tools into Responses `type:"function"` tool items
/// (openai-responses-shared.ts:344-380; grammar/custom tools not ported).
fn convert_tools(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            let mut obj = Map::new();
            obj.insert("type".into(), json!("function"));
            obj.insert("name".into(), json!(tool.name.as_str()));
            if let Some(description) = &tool.description {
                obj.insert("description".into(), json!(description));
            }
            obj.insert(
                "parameters".into(),
                tool.schema
                    .clone()
                    .unwrap_or_else(|| json!({ "type": "object" })),
            );
            if let Some(strict) = tool.strict {
                obj.insert("strict".into(), json!(strict));
            }
            Value::Object(obj)
        })
        .collect()
}

/// Build the WebSocket `response.create` frame (openai-codex-responses.ts:1504):
/// the request body spread under `{ type: "response.create", … }`.
pub fn build_ws_create_frame(body: &Value) -> String {
    let mut frame = Map::new();
    frame.insert("type".into(), json!("response.create"));
    if let Value::Object(map) = body {
        for (key, value) in map {
            frame.insert(key.clone(), value.clone());
        }
    }
    serde_json::to_string(&Value::Object(frame)).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use genai::chat::{MessageContent, ToolCall, ToolResponse};
    use serde_json::json;

    fn base_body(context: &LlmContext) -> Value {
        build_request_body(
            "gpt-5-codex",
            context,
            &ChatOptions::default(),
            &BodyConfig::default(),
        )
    }

    #[test]
    fn body_has_core_codex_fields() {
        let context = LlmContext {
            system_prompt: "be terse".into(),
            messages: vec![ChatMessage::user("hello")],
            tools: vec![],
        };
        let body = base_body(&context);
        assert_eq!(body["model"], "gpt-5-codex");
        assert_eq!(body["store"], json!(false));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["instructions"], "be terse");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], json!(true));
        assert_eq!(body["text"]["verbosity"], "low");
        // user message maps to input_text.
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        // no tools -> field omitted.
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn empty_system_prompt_defaults_instructions() {
        let context = LlmContext {
            system_prompt: String::new(),
            messages: vec![],
            tools: vec![],
        };
        assert_eq!(
            base_body(&context)["instructions"],
            "You are a helpful assistant."
        );
    }

    #[test]
    fn options_add_temperature_reasoning_verbosity() {
        let options = ChatOptions::default()
            .with_temperature(0.4)
            .with_reasoning_effort(ReasoningEffort::High)
            .with_verbosity(genai::chat::Verbosity::High);
        let context = LlmContext::default();
        let body = build_request_body("gpt-5", &context, &options, &BodyConfig::default());
        assert_eq!(body["temperature"], json!(0.4));
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["text"]["verbosity"], "high");
    }

    #[test]
    fn assistant_and_tool_messages_convert() {
        let assistant = ChatMessage::assistant(MessageContent::from_parts(vec![
            ContentPart::Text("done".into()),
            ContentPart::ToolCall(ToolCall {
                call_id: "call_1|fc_abc".into(),
                fn_name: "search".into(),
                fn_arguments: json!({ "q": "rust" }),
                thought_signatures: None,
            }),
        ]));
        let tool = ChatMessage::tool(MessageContent::from_parts(vec![ContentPart::ToolResponse(
            ToolResponse {
                call_id: "call_1|fc_abc".into(),
                fn_name: Some("search".into()),
                content: "result".into(),
                parts: None,
            },
        )]));
        let context = LlmContext {
            system_prompt: String::new(),
            messages: vec![assistant, tool],
            tools: vec![],
        };
        let input = &base_body(&context)["input"];

        // assistant text -> message item
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "done");
        assert_eq!(input[0]["id"], "msg_pi_0");
        // assistant tool call -> function_call with split call_id and fc_ item id
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_1");
        assert_eq!(input[1]["id"], "fc_abc");
        assert_eq!(input[1]["name"], "search");
        assert_eq!(input[1]["arguments"], r#"{"q":"rust"}"#);
        // tool result -> function_call_output with split call_id
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "call_1");
        assert_eq!(input[2]["output"], "result");
    }

    #[test]
    fn empty_tool_result_emits_placeholder() {
        // L7: an empty tool result becomes "(no tool output)", not "".
        let tool = ChatMessage::tool(MessageContent::from_parts(vec![ContentPart::ToolResponse(
            ToolResponse {
                call_id: "call_9".into(),
                fn_name: Some("noop".into()),
                content: String::new(),
                parts: None,
            },
        )]));
        let context = LlmContext {
            system_prompt: String::new(),
            messages: vec![tool],
            tools: vec![],
        };
        let input = &base_body(&context)["input"];
        assert_eq!(input[0]["type"], "function_call_output");
        assert_eq!(input[0]["call_id"], "call_9");
        assert_eq!(input[0]["output"], "(no tool output)");
    }

    #[test]
    fn tools_convert_to_function_items() {
        let tool = Tool::new("get_weather")
            .with_description("weather")
            .with_schema(json!({ "type": "object", "properties": {} }));
        let context = LlmContext {
            system_prompt: String::new(),
            messages: vec![],
            tools: vec![tool],
        };
        let body = base_body(&context);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["tools"][0]["description"], "weather");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
    }

    #[test]
    fn ws_create_frame_wraps_body() {
        let body = json!({ "model": "gpt-5", "stream": true });
        let frame: Value = serde_json::from_str(&build_ws_create_frame(&body)).unwrap();
        assert_eq!(frame["type"], "response.create");
        assert_eq!(frame["model"], "gpt-5");
        assert_eq!(frame["stream"], json!(true));
    }
}
