use std::sync::{Arc, Mutex};

use genai::adapter::AdapterKind;
use genai::{ModelIden, ModelSpec};
use rust_genai_agent::{
    AgentContext, AgentEvent, AgentEventSink, AgentLoopConfig, AgentMessage,
    AssistantMessageEventStream, AssistantMessageResult, CancellationToken, StopReason, StreamFn,
    StreamProtocolError, StreamRequest, default_convert_to_llm, run_agent_loop,
};

#[derive(Default)]
struct EventRecorder {
    events: Vec<AgentEvent>,
}

#[async_trait::async_trait]
impl AgentEventSink for EventRecorder {
    async fn emit(&mut self, event: AgentEvent) {
        self.events.push(event);
    }
}

#[tokio::test]
async fn malformed_stream_is_committed_as_an_in_band_error_with_a_complete_lifecycle() {
    let configured_model = ModelIden::new(AdapterKind::OpenAIResp, "configured-malformed-model");
    let config = AgentLoopConfig::new(
        ModelSpec::from_iden(configured_model.clone()),
        default_convert_to_llm(),
    );
    let prompt = AgentMessage::user("exercise malformed stream handling");
    let expected_error = StreamProtocolError::MissingTerminalEvent.to_string();
    let captured_result = Arc::new(Mutex::new(None::<AssistantMessageResult>));
    let captured_result_for_stream = captured_result.clone();
    let stream_fn: Arc<dyn StreamFn> = Arc::new(move |_request: StreamRequest| {
        let captured_result = captured_result_for_stream.clone();
        async move {
            let stream = AssistantMessageEventStream::from_events(Vec::new());
            *captured_result.lock().expect("result capture lock") = Some(stream.result_handle());
            stream
        }
    });
    let mut sink = EventRecorder::default();

    let messages = run_agent_loop(
        vec![prompt.clone()],
        AgentContext::new(""),
        config,
        &mut sink,
        CancellationToken::new(),
        Some(stream_fn),
    )
    .await
    .expect("a malformed provider stream should become an in-band assistant error");

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0], prompt);
    let AgentMessage::Assistant(error) = &messages[1] else {
        panic!("the synthesized message must be an assistant message");
    };
    assert_eq!(error.stop_reason, StopReason::Error);
    assert_eq!(error.model, configured_model);
    assert_eq!(
        error.error_message.as_deref(),
        Some(expected_error.as_str())
    );

    let synthesized = messages[1].clone();
    assert_eq!(
        sink.events,
        vec![
            AgentEvent::AgentStart,
            AgentEvent::TurnStart,
            AgentEvent::MessageStart {
                message: prompt.clone(),
            },
            AgentEvent::MessageEnd { message: prompt },
            AgentEvent::MessageStart {
                message: synthesized.clone(),
            },
            AgentEvent::MessageEnd {
                message: synthesized.clone(),
            },
            AgentEvent::TurnEnd {
                message: synthesized,
                tool_results: Vec::new(),
            },
            AgentEvent::AgentEnd {
                messages: messages.clone(),
            },
        ]
    );

    let direct_result = captured_result
        .lock()
        .expect("result capture lock")
        .take()
        .expect("the stream function should expose its result handle");
    assert_eq!(
        direct_result.get().await,
        Err(StreamProtocolError::MissingTerminalEvent),
        "direct result consumers must still observe the protocol violation"
    );
}
