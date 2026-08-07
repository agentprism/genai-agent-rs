//! Live schema-validated tool execution through `Agent`.
//!
//! Run with `OPENAI_API_KEY=... cargo run --example c02-tools`.

use std::io::Write;
use std::sync::Arc;

use genai::Client;
use rust_genai_agent::{
    Agent, AgentConfig, AgentEvent, AgentState, AgentTool, AgentToolResult, AssistantMessageEvent,
    FnTool, GenaiStreamFn, ToolError, ToolSpec,
};
use serde_json::json;

const MODEL: &str = "gpt-4o-mini";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let weather = FnTool::from_value_fn(
        ToolSpec::new(
            "get_weather",
            "Get the current weather for a city",
            json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string", "description": "City name" },
                    "unit": { "type": "string", "enum": ["celsius", "fahrenheit"] }
                },
                "required": ["city", "unit"],
                "additionalProperties": false
            }),
        ),
        |args| async move {
            // Replace this deterministic value with a real service call.
            Ok::<_, ToolError>(AgentToolResult::json(json!({
                "city": args["city"],
                "unit": args["unit"],
                "temperature": 21,
                "condition": "clear"
            })))
        },
    );

    let state = AgentState {
        model: MODEL.into(),
        system_prompt: "Use the weather tool, then answer in one sentence.".into(),
        tools: vec![Arc::new(weather) as Arc<dyn AgentTool>],
        ..AgentState::default()
    };

    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(Arc::new(GenaiStreamFn::new(Client::default()))),
    );

    let subscription = agent.subscribe_fn(|event, _cancel| async move {
        match event {
            AgentEvent::MessageUpdate {
                assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                ..
            } => {
                print!("{delta}");
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolExecutionStart { tool_name, .. } => {
                eprintln!(
                    "
[calling {tool_name}]"
                );
            }
            AgentEvent::ToolExecutionEnd {
                tool_name,
                is_error,
                ..
            } => {
                eprintln!("[{tool_name} finished; error={is_error}]");
            }
            _ => {}
        }
    });

    agent
        .prompt("What is the weather in Tokyo in celsius?")
        .await?;
    println!();

    if let Some(error) = agent.state().error_message {
        eprintln!("provider run failed: {error}");
    }

    drop(subscription);
    Ok(())
}
