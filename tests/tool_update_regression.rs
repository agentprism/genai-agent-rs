//! Focused regressions for tool-update dispatch and settlement barriers.

#![cfg(feature = "testing")]

use std::sync::{
    Arc, Barrier, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use genai::adapter::AdapterKind;
use genai::{ModelIden, ModelSpec};
use rust_genai_agent::testing::{EventRecorder, MockStreamFn};
use rust_genai_agent::{
    AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentTool, AgentToolCall,
    AgentToolResult, AssistantContent, AssistantMessage, FnTool, StopReason, ToolExecutionMode,
    ToolSpec, UpdateSink, default_convert_to_llm, run_agent_loop,
};
use serde_json::json;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

fn model_iden() -> ModelIden {
    ModelIden::new(AdapterKind::OpenAIResp, "mock")
}

fn config(mode: ToolExecutionMode) -> AgentLoopConfig {
    let mut config =
        AgentLoopConfig::new(ModelSpec::from_iden(model_iden()), default_convert_to_llm());
    config.tool_execution = mode;
    config
}

fn tool_spec(name: &str) -> ToolSpec {
    ToolSpec::new(
        name,
        format!("{name} regression tool"),
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false,
        }),
    )
}

fn tool_response(name: &str) -> AssistantMessage {
    AssistantMessage::completed(
        model_iden(),
        vec![AssistantContent::tool_call(AgentToolCall::new(
            "tool-1",
            name,
            json!({}),
        ))],
        StopReason::ToolUse,
    )
}

fn terminating_result(text: &str) -> AgentToolResult {
    AgentToolResult::text(text).with_terminate(true)
}

fn context_with(tool: Arc<dyn AgentTool>) -> AgentContext {
    AgentContext::new("").with_tools(vec![tool])
}

fn stream_for(name: &str) -> Arc<MockStreamFn> {
    Arc::new(MockStreamFn::from_messages(vec![tool_response(name)]))
}

#[test]
fn update_sink_close_waits_for_an_accepted_synchronous_callback() {
    let callback_entered = Arc::new(Barrier::new(2));
    let release_callback = Arc::new(Barrier::new(2));
    let close_started = Arc::new(Barrier::new(2));
    let callback_completed = Arc::new(AtomicBool::new(false));

    let callback_entered_by_sink = callback_entered.clone();
    let release_callback_by_sink = release_callback.clone();
    let callback_completed_by_sink = callback_completed.clone();
    let sink = UpdateSink::new(move |_update| {
        callback_entered_by_sink.wait();
        release_callback_by_sink.wait();
        callback_completed_by_sink.store(true, Ordering::Release);
    });

    let emitting_sink = sink.clone();
    let emitter = thread::spawn(move || emitting_sink.emit(AgentToolResult::text("accepted")));
    callback_entered.wait();

    let closing_sink = sink.clone();
    let close_started_by_thread = close_started.clone();
    let callback_completed_by_closer = callback_completed.clone();
    let (close_returned, observe_close_returned) = mpsc::channel();
    let closer = thread::spawn(move || {
        close_started_by_thread.wait();
        closing_sink.close();
        close_returned
            .send(callback_completed_by_closer.load(Ordering::Acquire))
            .expect("close observer was dropped");
    });
    close_started.wait();

    // The barrier puts the closer immediately before `close`. A generous bounded probe makes the
    // old atomic check/callback race deterministic without leaving blocked threads on failure.
    let early_close = match observe_close_returned.recv_timeout(Duration::from_millis(250)) {
        Ok(saw_completed_callback) => Some(saw_completed_callback),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("close thread disconnected before reporting")
        }
    };

    release_callback.wait();
    assert!(emitter.join().expect("emit thread panicked"));
    let close_saw_completed_callback = early_close.unwrap_or_else(|| {
        observe_close_returned
            .recv_timeout(TEST_TIMEOUT)
            .expect("close did not return after the callback completed")
    });
    closer.join().expect("close thread panicked");

    assert!(
        early_close.is_none(),
        "close returned before an already accepted callback completed"
    );
    assert!(close_saw_completed_callback);
    assert!(sink.is_closed());
    assert!(!sink.emit(AgentToolResult::text("late")));
}

#[tokio::test(flavor = "current_thread")]
async fn parallel_dispatch_does_not_wait_for_a_retained_update_sink_sender() {
    let retained = Arc::new(Mutex::new(None::<UpdateSink>));
    let retained_by_tool = retained.clone();
    let tool = Arc::new(FnTool::new(
        tool_spec("retain_updates"),
        move |_call, _cancel, updates| {
            let retained = retained_by_tool.clone();
            async move {
                *retained.lock().expect("retained sink mutex poisoned") = Some(updates.clone());
                Ok(terminating_result("done"))
            }
        },
    ));
    let mut recorder = EventRecorder::new();

    timeout(
        TEST_TIMEOUT,
        run_agent_loop(
            vec![AgentMessage::user("run")],
            context_with(tool),
            config(ToolExecutionMode::Parallel),
            &mut recorder,
            CancellationToken::new(),
            Some(stream_for("retain_updates")),
        ),
    )
    .await
    .expect("parallel dispatch hung on a sender retained by a closed UpdateSink")
    .expect("agent loop failed");

    let retained = retained
        .lock()
        .expect("retained sink mutex poisoned")
        .take()
        .expect("tool did not retain its update sink");
    assert!(retained.is_closed(), "settlement must close retained sinks");
    assert!(
        !retained.emit(AgentToolResult::text("late")),
        "updates after settlement must be ignored"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn aborting_a_pending_run_closes_its_retained_update_sink() {
    let retained = Arc::new(Mutex::new(None::<UpdateSink>));
    let sink_captured = Arc::new(Notify::new());
    let retained_by_tool = retained.clone();
    let sink_captured_by_tool = sink_captured.clone();
    let tool = Arc::new(FnTool::new(
        tool_spec("abort_pending"),
        move |_call, _cancel, updates| {
            let retained = retained_by_tool.clone();
            let sink_captured = sink_captured_by_tool.clone();
            async move {
                {
                    *retained.lock().expect("retained sink mutex poisoned") = Some(updates.clone());
                }
                sink_captured.notify_one();
                std::future::pending::<()>().await;
                Ok(terminating_result("unreachable"))
            }
        },
    ));

    let loop_task = tokio::spawn(async move {
        let mut recorder = EventRecorder::new();
        run_agent_loop(
            vec![AgentMessage::user("run")],
            context_with(tool),
            config(ToolExecutionMode::Parallel),
            &mut recorder,
            CancellationToken::new(),
            Some(stream_for("abort_pending")),
        )
        .await
    });

    timeout(TEST_TIMEOUT, sink_captured.notified())
        .await
        .expect("tool did not capture its update sink");
    loop_task.abort();
    let join_error = timeout(TEST_TIMEOUT, loop_task)
        .await
        .expect("aborted loop task did not stop")
        .expect_err("aborted loop task unexpectedly completed");
    assert!(join_error.is_cancelled());

    timeout(TEST_TIMEOUT, async {
        loop {
            let closed = retained
                .lock()
                .expect("retained sink mutex poisoned")
                .as_ref()
                .is_some_and(UpdateSink::is_closed);
            if closed {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping the pending execution future did not close its update sink");

    let retained = retained
        .lock()
        .expect("retained sink mutex poisoned")
        .take()
        .expect("tool did not retain its update sink");
    assert!(retained.is_closed());
    assert!(
        !retained.emit(AgentToolResult::text("late")),
        "an externally aborted execution accepted a late update"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn parallel_after_hook_waits_for_a_blocked_update_listener() {
    let listener_started = Arc::new(Notify::new());
    let release_listener = Arc::new(Notify::new());
    let allow_tool_to_settle = Arc::new(Notify::new());
    let tool_returning = Arc::new(Notify::new());
    let update_completed = Arc::new(AtomicBool::new(false));
    let after_called = Arc::new(AtomicBool::new(false));
    let after_saw_completed_update = Arc::new(AtomicBool::new(false));

    let allow_tool_to_settle_by_tool = allow_tool_to_settle.clone();
    let tool_returning_by_tool = tool_returning.clone();
    let tool = Arc::new(FnTool::new(
        tool_spec("blocked_update"),
        move |_call, _cancel, updates| {
            let allow_tool_to_settle = allow_tool_to_settle_by_tool.clone();
            let tool_returning = tool_returning_by_tool.clone();
            async move {
                assert!(updates.emit(AgentToolResult::text("partial")));
                allow_tool_to_settle.notified().await;
                tool_returning.notify_one();
                Ok(terminating_result("done"))
            }
        },
    ));

    let mut loop_config = config(ToolExecutionMode::Parallel);
    let update_completed_by_hook = update_completed.clone();
    let after_called_by_hook = after_called.clone();
    let after_saw_completed_update_by_hook = after_saw_completed_update.clone();
    loop_config.after_tool_call = Some(Arc::new(move |_context, _cancel| {
        let update_completed = update_completed_by_hook.clone();
        let after_called = after_called_by_hook.clone();
        let after_saw_completed_update = after_saw_completed_update_by_hook.clone();
        Box::pin(async move {
            after_saw_completed_update
                .store(update_completed.load(Ordering::Acquire), Ordering::Release);
            after_called.store(true, Ordering::Release);
            None
        })
    }));

    let listener_started_by_sink = listener_started.clone();
    let release_listener_by_sink = release_listener.clone();
    let update_completed_by_sink = update_completed.clone();
    let loop_task = tokio::spawn(async move {
        let mut sink = move |event: AgentEvent| {
            let listener_started = listener_started_by_sink.clone();
            let release_listener = release_listener_by_sink.clone();
            let update_completed = update_completed_by_sink.clone();
            async move {
                if matches!(event, AgentEvent::ToolExecutionUpdate { .. }) {
                    listener_started.notify_one();
                    release_listener.notified().await;
                    update_completed.store(true, Ordering::Release);
                }
            }
        };
        run_agent_loop(
            vec![AgentMessage::user("run")],
            context_with(tool),
            loop_config,
            &mut sink,
            CancellationToken::new(),
            Some(stream_for("blocked_update")),
        )
        .await
    });

    timeout(TEST_TIMEOUT, listener_started.notified())
        .await
        .expect("update listener was never entered");
    allow_tool_to_settle.notify_one();
    timeout(TEST_TIMEOUT, tool_returning.notified())
        .await
        .expect("tool did not settle");
    tokio::task::yield_now().await;
    assert!(
        !after_called.load(Ordering::Acquire),
        "after_tool_call ran while the update listener was still blocked"
    );

    release_listener.notify_one();
    timeout(TEST_TIMEOUT, loop_task)
        .await
        .expect("agent loop did not finish after the listener was released")
        .expect("agent loop task panicked")
        .expect("agent loop failed");
    assert!(after_called.load(Ordering::Acquire));
    assert!(
        after_saw_completed_update.load(Ordering::Acquire),
        "after_tool_call did not observe the completed update emission"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sequential_update_backpressure_does_not_stop_polling_the_tool() {
    let tool_done = Arc::new(AtomicBool::new(false));
    let tool_done_notification = Arc::new(Notify::new());
    let update_completed = Arc::new(AtomicBool::new(false));
    let after_saw_completed_update = Arc::new(AtomicBool::new(false));

    let tool_done_by_tool = tool_done.clone();
    let tool_done_notification_by_tool = tool_done_notification.clone();
    let tool = Arc::new(FnTool::new(
        tool_spec("sequential_backpressure"),
        move |_call, _cancel, updates| {
            let tool_done = tool_done_by_tool.clone();
            let tool_done_notification = tool_done_notification_by_tool.clone();
            async move {
                assert!(updates.emit(AgentToolResult::text("partial")));
                tokio::task::yield_now().await;
                tool_done.store(true, Ordering::Release);
                tool_done_notification.notify_one();
                Ok(terminating_result("done"))
            }
        },
    ));

    let mut loop_config = config(ToolExecutionMode::Sequential);
    let update_completed_by_hook = update_completed.clone();
    let after_saw_completed_update_by_hook = after_saw_completed_update.clone();
    loop_config.after_tool_call = Some(Arc::new(move |_context, _cancel| {
        let update_completed = update_completed_by_hook.clone();
        let after_saw_completed_update = after_saw_completed_update_by_hook.clone();
        Box::pin(async move {
            after_saw_completed_update
                .store(update_completed.load(Ordering::Acquire), Ordering::Release);
            None
        })
    }));

    let tool_done_by_sink = tool_done.clone();
    let tool_done_notification_by_sink = tool_done_notification.clone();
    let update_completed_by_sink = update_completed.clone();
    let mut sink = move |event: AgentEvent| {
        let tool_done = tool_done_by_sink.clone();
        let tool_done_notification = tool_done_notification_by_sink.clone();
        let update_completed = update_completed_by_sink.clone();
        async move {
            if matches!(event, AgentEvent::ToolExecutionUpdate { .. }) {
                while !tool_done.load(Ordering::Acquire) {
                    tool_done_notification.notified().await;
                }
                update_completed.store(true, Ordering::Release);
            }
        }
    };

    timeout(
        TEST_TIMEOUT,
        run_agent_loop(
            vec![AgentMessage::user("run")],
            context_with(tool),
            loop_config,
            &mut sink,
            CancellationToken::new(),
            Some(stream_for("sequential_backpressure")),
        ),
    )
    .await
    .expect("sequential update backpressure stopped polling the tool")
    .expect("agent loop failed");

    assert!(tool_done.load(Ordering::Acquire));
    assert!(update_completed.load(Ordering::Acquire));
    assert!(
        after_saw_completed_update.load(Ordering::Acquire),
        "after_tool_call overtook the sequential update listener"
    );
}
