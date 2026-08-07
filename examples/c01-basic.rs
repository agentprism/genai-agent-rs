//! Basic live `Agent` streaming with the default `genai` client.
//!
//! Run with `OPENAI_API_KEY=... cargo run --example c01-basic`.

use std::io::Write;
use std::sync::Arc;

use genai::Client;
use rust_genai_agent::{
    Agent, AgentConfig, AgentEvent, AgentState, AssistantMessageEvent, GenaiStreamFn,
};

const MODEL: &str = "gpt-4o-mini";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = AgentState {
        model: MODEL.into(),
        system_prompt: "Answer clearly and in no more than three sentences.".into(),
        ..AgentState::default()
    };

    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(Arc::new(GenaiStreamFn::new(Client::default()))),
    );

    // `Subscription` unsubscribes on drop, so retain it through `prompt`.
    let subscription = agent.subscribe_fn(|event, _cancel| async move {
        if let AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } = event
        {
            print!("{delta}");
            let _ = std::io::stdout().flush();
        }
    });

    agent
        .prompt("Why does the moon appear to change shape?")
        .await?;
    println!();

    if let Some(error) = agent.state().error_message {
        eprintln!("provider run failed: {error}");
    }

    drop(subscription);
    Ok(())
}
