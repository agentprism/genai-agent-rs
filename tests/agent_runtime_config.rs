//! Stateful-agent runtime configuration and reasoning-budget regression coverage.

#![cfg(feature = "testing")]

use genai::chat::{ChatOptions, ReasoningEffort};
use rust_genai_agent::testing::{MockStreamFn, ScriptedStream, fixtures, script};
use rust_genai_agent::{
    AfterToolCallHook, Agent, AgentConfig, AgentError, AgentPrepareNextTurnHook,
    AgentPrepareNextTurnWithContextHook, AgentShouldStopAfterTurnHook, BeforeToolCallHook,
    BusyContext, ConvertToLlm, StreamFn, ThinkingLevel, ToolExecutionMode, TransformContextHook,
    default_convert_to_llm,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

#[test]
fn named_thinking_levels_keep_their_existing_reasoning_mappings() {
    assert!(ThinkingLevel::Off.reasoning_effort().is_none());
    for (level, expected) in [
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ] {
        assert_eq!(
            level
                .reasoning_effort()
                .as_ref()
                .map(ReasoningEffort::variant_name),
            Some(expected)
        );
    }
}

#[tokio::test]
async fn thinking_budget_is_forwarded_to_stream_request_options() {
    let stream = Arc::new(MockStreamFn::from_streams(vec![script::text_response(
        "ok",
    )]));
    let agent = Agent::new(AgentConfig {
        stream_fn: Some(stream.clone()),
        chat_options: ChatOptions::default().with_reasoning_effort(ReasoningEffort::High),
        ..AgentConfig::default()
    });

    agent.set_thinking_level(ThinkingLevel::Budget(4_096));
    agent.prompt("budget please").await.unwrap();

    let calls = stream.calls();
    assert_eq!(calls.len(), 1);
    assert!(matches!(
        calls[0].options.reasoning_effort.as_ref(),
        Some(ReasoningEffort::Budget(4_096))
    ));
}

#[tokio::test]
async fn replacing_stream_function_applies_to_the_next_run() {
    let original = Arc::new(MockStreamFn::from_streams(vec![script::text_response(
        "original",
    )]));
    let replacement = Arc::new(MockStreamFn::from_streams(vec![script::text_response(
        "replacement",
    )]));
    let agent = Agent::new(AgentConfig {
        stream_fn: Some(original.clone()),
        ..AgentConfig::default()
    });

    agent.prompt("first").await.unwrap();
    agent
        .set_stream_fn(replacement.clone() as Arc<dyn StreamFn>)
        .unwrap();
    agent.prompt("second").await.unwrap();

    assert_eq!(original.call_count(), 1);
    assert_eq!(replacement.call_count(), 1);
}

#[tokio::test]
async fn replacing_hook_and_chat_options_applies_to_the_next_run() {
    let old_hook_calls = Arc::new(AtomicUsize::new(0));
    let old_calls = old_hook_calls.clone();
    let old_hook: TransformContextHook = Arc::new(move |messages, _cancel| {
        old_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { messages })
    });
    let new_hook_calls = Arc::new(AtomicUsize::new(0));
    let new_calls = new_hook_calls.clone();
    let new_hook: TransformContextHook = Arc::new(move |messages, _cancel| {
        new_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { messages })
    });
    let stream = Arc::new(MockStreamFn::from_streams(vec![script::text_response(
        "ok",
    )]));
    let agent = Agent::new(AgentConfig {
        stream_fn: Some(stream.clone()),
        transform_context: Some(old_hook),
        session_id: Some("session-stays-stable".into()),
        ..AgentConfig::default()
    });

    agent.set_transform_context(Some(new_hook)).unwrap();
    agent
        .set_chat_options(ChatOptions::default().with_temperature(0.25))
        .unwrap();
    agent.prompt("use replacements").await.unwrap();

    assert_eq!(old_hook_calls.load(Ordering::SeqCst), 0);
    assert_eq!(new_hook_calls.load(Ordering::SeqCst), 1);
    let calls = stream.calls();
    assert_eq!(calls[0].options.temperature, Some(0.25));
    assert_eq!(
        calls[0].options.prompt_cache_key.as_deref(),
        Some("session-stays-stable")
    );
    assert_eq!(agent.session_id().as_deref(), Some("session-stays-stable"));
}

#[tokio::test]
async fn every_runtime_config_setter_rejects_updates_during_an_active_run() {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let stream = Arc::new(MockStreamFn::from_fn({
        let entered = entered.clone();
        let release = release.clone();
        move |_request| {
            let entered = entered.clone();
            let release = release.clone();
            ScriptedStream::from_driver(move |sender| async move {
                entered.add_permits(1);
                let permit = release.acquire().await.unwrap();
                permit.forget();
                sender
                    .send(rust_genai_agent::AssistantMessageEvent::Done {
                        reason: rust_genai_agent::StopReason::Stop,
                        message: fixtures::text_msg("done"),
                    })
                    .expect("agent consumes terminal event");
            })
        }
    }));
    let agent = Agent::new(AgentConfig {
        stream_fn: Some(stream),
        ..AgentConfig::default()
    });

    let running_agent = agent.clone();
    let prompt = tokio::spawn(async move { running_agent.prompt("block").await });
    let permit = tokio::time::timeout(Duration::from_secs(2), entered.acquire())
        .await
        .expect("stream function was entered")
        .unwrap();
    permit.forget();

    let replacement_stream: Arc<dyn StreamFn> =
        Arc::new(MockStreamFn::from_streams(vec![script::text_response(
            "unused",
        )]));
    let convert: ConvertToLlm = default_convert_to_llm();
    let transform: TransformContextHook =
        Arc::new(|messages, _cancel| Box::pin(async move { messages }));
    let before: BeforeToolCallHook = Arc::new(|_context, _cancel| Box::pin(async move { None }));
    let after: AfterToolCallHook = Arc::new(|_context, _cancel| Box::pin(async move { None }));
    let should_stop: AgentShouldStopAfterTurnHook =
        Arc::new(|_context, _cancel| Box::pin(async move { false }));
    let prepare: AgentPrepareNextTurnHook = Arc::new(|_cancel| Box::pin(async move { None }));
    let prepare_with_context: AgentPrepareNextTurnWithContextHook =
        Arc::new(|_context, _cancel| Box::pin(async move { None }));

    assert_eq!(
        agent.set_stream_fn(replacement_stream),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_convert_to_llm(convert),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_transform_context(Some(transform)),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_before_tool_call(Some(before)),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_after_tool_call(Some(after)),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_should_stop_after_turn(Some(should_stop)),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_prepare_next_turn(Some(prepare)),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_prepare_next_turn_with_context(Some(prepare_with_context)),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_tool_execution(ToolExecutionMode::Sequential),
        Err(AgentError::Busy(BusyContext::Other))
    );
    assert_eq!(
        agent.set_chat_options(ChatOptions::default()),
        Err(AgentError::Busy(BusyContext::Other))
    );

    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), prompt)
        .await
        .expect("prompt settles after release")
        .unwrap()
        .unwrap();
}
