use std::time::Duration;

use futures::StreamExt;
use genai::ModelIden;
use genai::adapter::AdapterKind;
use rust_genai_agent::{
    AgentContext, AgentLoopConfig, AgentMessage, AssistantContent, AssistantMessage,
    AssistantMessageEvent, AssistantMessageEventStream, CancellationToken, CustomMessage,
    LoopError, NoopEventSink, StopReason, run_agent_loop_continue,
};
use serde_json::json;
use tokio::time::timeout;

fn message(text: &str, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage::completed(
        ModelIden::new(AdapterKind::OpenAIResp, "protocol-regression"),
        vec![AssistantContent::text(text)],
        stop_reason,
    )
}

fn done(message: AssistantMessage) -> AssistantMessageEvent {
    AssistantMessageEvent::Done {
        reason: message.stop_reason,
        message,
    }
}

#[tokio::test]
async fn low_level_continue_rejects_a_custom_assistant_role() {
    let context = AgentContext::new("").with_messages(vec![AgentMessage::Custom(
        CustomMessage::new("assistant", json!({ "kind": "extension-message" })),
    )]);
    let mut sink = NoopEventSink;

    let result = run_agent_loop_continue(
        context,
        AgentLoopConfig::default(),
        &mut sink,
        CancellationToken::new(),
        None,
    )
    .await;

    assert!(matches!(result, Err(LoopError::ContinueFromAssistant)));
}

#[tokio::test]
async fn channel_result_and_events_use_the_first_queued_terminal() {
    let first = message("first", StopReason::Stop);
    let second = message("second", StopReason::Length);
    let (sender, mut stream) = AssistantMessageEventStream::channel();
    let result = stream.result_handle();

    sender.send(done(first.clone())).unwrap();
    sender.send(done(second)).unwrap();

    // Channel-backed streams publish their result from the producer, before the stream is polled.
    let resolved = timeout(Duration::from_secs(30), result.get())
        .await
        .expect("a queued terminal should resolve the result")
        .expect("the terminal result should be valid");
    let terminal = stream.next().await.expect("the first terminal event");
    let trailing = timeout(Duration::from_secs(30), stream.next())
        .await
        .expect("the stream should fuse immediately after its terminal event");

    assert_eq!(terminal.terminal_message(), Some(&first));
    assert!(
        trailing.is_none(),
        "events after completion must be ignored"
    );
    assert_eq!(resolved, first);
}

#[tokio::test]
async fn arbitrary_upstream_is_fused_after_its_first_terminal() {
    let first = message("first", StopReason::Stop);
    let second = message("second", StopReason::Length);
    let mut stream =
        AssistantMessageEventStream::from_events(vec![done(first.clone()), done(second)]);
    let result = stream.result_handle();

    let terminal = stream.next().await.expect("the first terminal event");
    let trailing = stream.next().await;
    let resolved = result.get().await.expect("the terminal result");

    assert_eq!(terminal.terminal_message(), Some(&first));
    assert!(
        trailing.is_none(),
        "upstream events after completion must be ignored"
    );
    assert_eq!(resolved, first);
}
