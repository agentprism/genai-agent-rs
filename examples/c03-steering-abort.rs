//! Live steering and cancellation of an active `Agent` run.
//!
//! Run with `OPENAI_API_KEY=... cargo run --example c03-steering-abort`.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use genai::Client;
use rust_genai_agent::{
    Agent, AgentConfig, AgentEvent, AgentMessage, AgentState, AssistantMessageEvent, GenaiStreamFn,
    StopReason,
};

const MODEL: &str = "gpt-4o-mini";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = AgentState {
        model: MODEL.into(),
        system_prompt: "Work step by step and respond to steering between turns.".into(),
        ..AgentState::default()
    };

    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(Arc::new(GenaiStreamFn::new(Client::default()))),
    );

    let subscription = agent.subscribe_fn(|event, _cancel| async move {
        if let AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } = event
        {
            match assistant_message_event {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    print!("{delta}");
                    let _ = std::io::stdout().flush();
                }
                AssistantMessageEvent::Error {
                    reason: StopReason::Aborted,
                    ..
                } => {
                    eprintln!(
                        "
[run aborted]"
                    );
                }
                _ => {}
            }
        }
    });

    let running_agent = agent.clone();
    let run = tokio::spawn(async move {
        running_agent
            .prompt("Write a detailed plan for a weekend astronomy workshop.")
            .await
    });

    tokio::time::sleep(Duration::from_millis(250)).await;
    agent.steer(AgentMessage::user(
        "Steer toward activities suitable for complete beginners.",
    ));

    // This deadline is illustrative. If the run already finished, `abort` is a no-op.
    tokio::time::sleep(Duration::from_secs(2)).await;
    agent.abort();
    run.await??;
    agent.wait_for_idle().await;

    if let Some(error) = agent.state().error_message {
        eprintln!("terminal status: {error}");
    }

    drop(subscription);
    Ok(())
}
