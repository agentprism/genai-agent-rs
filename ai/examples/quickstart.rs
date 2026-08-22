//! pi-ai README "Quick Start" (pin 496185f6, README.md:100-228), recreated on the Rust port.
//!
//! Differences from the TypeScript listing are limited to what the port makes explicit:
//! - provider/model are `deepseek` / `deepseek-v4-flash` (the owner's choice for this run);
//! - the tool result uses a fixed clock so both ports hand the model identical text.
//!
//! Run: `DEEPSEEK_API_KEY=… cargo run -p agentprism-ai --example quickstart`

use ai::event_stream::AssistantMessageEvent;
use ai::models::ModelsApiStreamOptions;
use ai::providers::all::builtin_models;
use ai::types::{
    AssistantContent, AssistantMessage, Context, Message, TextContent, Tool, ToolResultContent,
    ToolResultMessage, ToolResultRole, UserContent, UserMessage, UserRole,
};
use futures::StreamExt;
use serde_json::json;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn json_name<T: serde::Serialize>(value: &T) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(e) => e.to_string(),
    }
}

fn tool_calls(message: &AssistantMessage) -> Vec<&ai::types::ToolCall> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::ToolCall(call) => Some(call),
            _ => None,
        })
        .collect()
}

#[tokio::main]
async fn main() {
    // A Models collection with every built-in provider registered
    let models = builtin_models(None);

    // Sync lookup against the collection
    let mut model = models
        .get_model("deepseek", "deepseek-v4-flash")
        .expect("deepseek/deepseek-v4-flash in the built-in catalog");
    // Optional: point the same catalog entry at a local capture server (wire-fidelity checks).
    if let Ok(base_url) = std::env::var("PI_BASE_URL") {
        model.base_url = base_url;
    }
    println!(
        "model: {}/{} api={} baseUrl={} reasoning={} ctx={} max={}",
        model.provider.as_str(),
        model.id,
        model.api.as_str(),
        model.base_url,
        model.reasoning,
        model.context_window,
        model.max_tokens
    );

    // Define tools (JSON Schema; this is exactly what pi's TypeBox `Type.Object` emits)
    let tools = vec![Tool {
        name: "get_time".to_owned(),
        description: "Get the current time".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "timezone": {
                    "type": "string",
                    "description": "Optional timezone (e.g., America/New_York)"
                }
            }
        }),
        constrained_sampling: None,
    }];

    // Build a conversation context (easily serializable and transferable between models)
    let mut context = Context {
        system_prompt: Some(("You are a helpful assistant.".to_owned()).into()),
        messages: vec![Message::User(Box::new(UserMessage {
            role: UserRole::User,
            content: UserContent::Text(("What time is it?".to_owned()).into()),
            timestamp: (now() as f64),
        }))],
        tools: Some(tools),
    };

    // Option 1: Streaming with all event types.
    // Auth resolves through the provider (DEEPSEEK_API_KEY from the environment here).
    let mut s = models.stream(&model, &context, ModelsApiStreamOptions::default());
    while let Some(event) = s.next().await {
        match &event {
            AssistantMessageEvent::Start { partial } => {
                println!("Starting with {}", partial.model);
            }
            AssistantMessageEvent::TextStart { .. } => println!("\n[Text started]"),
            AssistantMessageEvent::TextDelta { delta, .. } => {
                print!("{delta}");
                std::io::stdout().flush().ok();
            }
            AssistantMessageEvent::TextEnd { .. } => println!("\n[Text ended]"),
            AssistantMessageEvent::ThinkingStart { .. } => println!("[Model is thinking...]"),
            AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                print!("{delta}");
                std::io::stdout().flush().ok();
            }
            AssistantMessageEvent::ThinkingEnd { .. } => println!("[Thinking complete]"),
            AssistantMessageEvent::ToolCallStart { content_index, .. } => {
                println!("\n[Tool call started: index {content_index}]");
            }
            AssistantMessageEvent::ToolCallDelta {
                content_index,
                partial,
                ..
            } => {
                // Partial tool arguments are being streamed
                let index = (*content_index >= 0.0
                    && content_index.is_finite()
                    && content_index.fract() == 0.0)
                    .then_some(*content_index as usize);
                if let Some(AssistantContent::ToolCall(call)) =
                    index.and_then(|index| partial.content.get(index))
                {
                    println!("[Streaming args for {}]", call.name);
                }
            }
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                println!("\nTool called: {}", tool_call.name);
                println!("Arguments: {}", tool_call.arguments);
            }
            AssistantMessageEvent::Done { reason, .. } => {
                println!("\nFinished: {}", json_name(reason));
            }
            AssistantMessageEvent::Error { error, .. } => {
                eprintln!("Error: {}", error.error_message.as_deref().unwrap_or(""));
            }
        }
    }

    // Get the final message after streaming, add it to the context
    let final_message = s.result().await.expect("stream settles with a message");
    context
        .messages
        .push(Message::Assistant(Box::new(final_message.clone())));

    // Handle tool calls if any
    let calls = tool_calls(&final_message);
    for call in &calls {
        let result = if call.name == "get_time" {
            // Fixed clock: 2026-08-21T17:30:00Z formatted the way the TS example does it.
            let timezone = call
                .arguments
                .get("timezone")
                .and_then(|v| v.as_str())
                .filter(|tz| !tz.is_empty())
                .and_then(|value| value.to_utf8().ok())
                .unwrap_or_else(|| "UTC".to_owned());
            match timezone.as_str() {
                "UTC" => "Friday, August 21, 2026 at 5:30:00 PM UTC".to_owned(),
                other => format!("Friday, August 21, 2026 at 5:30:00 PM {other}"),
            }
        } else {
            "Unknown tool".to_owned()
        };

        // Add tool result to context (supports text and images)
        context
            .messages
            .push(Message::ToolResult(Box::new(ToolResultMessage {
                role: ToolResultRole::ToolResult,
                tool_call_id: call.id.clone(),
                tool_name: call.name.clone(),
                content: vec![ToolResultContent::Text(TextContent::new(result))],
                details: None,
                usage: None,
                added_tool_names: None,
                is_error: false,
                timestamp: (now() as f64),
            })));
    }

    // Continue if there were tool calls
    if !calls.is_empty() {
        let continuation = models
            .complete(&model, &context, ModelsApiStreamOptions::default())
            .await
            .expect("continuation");
        context
            .messages
            .push(Message::Assistant(Box::new(continuation.clone())));
        println!(
            "After tool execution: {}",
            serde_json::to_string(&continuation.content).expect("content JSON")
        );
    }

    println!(
        "Total tokens: {} in, {} out",
        final_message.usage.input, final_message.usage.output
    );
    println!("Cost: ${:.4}", final_message.usage.cost.total);
    println!(
        "finalMessage JSON: {}",
        serde_json::to_string(&final_message).expect("message JSON")
    );

    // Option 2: Get complete response without streaming
    let response = models
        .complete(&model, &context, ModelsApiStreamOptions::default())
        .await
        .expect("complete");
    for block in response.content.iter() {
        match block {
            AssistantContent::Text(text) => println!("{}", text.text),
            AssistantContent::ToolCall(call) => {
                println!("Tool: {}({})", call.name, call.arguments);
            }
            _ => {}
        }
    }
}
