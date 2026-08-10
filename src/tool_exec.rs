use crate::{
    AfterToolCallContext, AfterToolCallHook, AgentContext, AgentEvent, AgentEventSink,
    AgentLoopConfig, AgentTool, AgentToolCall, AgentToolResult, AssistantMessage,
    BeforeToolCallContext, BeforeToolCallResult, ToolCallContext, ToolExecutionMode, ToolHookError,
    ToolResultContent, ToolResultMessage, TryAfterToolCallHook, UpdateSink,
    validate_tool_arguments,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

/// Results produced by one assistant tool-call batch.
pub(super) struct ExecutedToolCallBatch {
    pub(super) messages: Vec<ToolResultMessage>,
    pub(super) terminate: bool,
}

#[derive(Clone)]
struct PreparedToolCall {
    tool_call: AgentToolCall,
    tool: Arc<dyn AgentTool>,
    args: Value,
}

struct ExecutedToolCallOutcome {
    result: AgentToolResult,
    is_error: bool,
}

struct AfterToolCallSnapshot {
    try_hook: Option<TryAfterToolCallHook>,
    legacy_hook: Option<AfterToolCallHook>,
    current_context: AgentContext,
    assistant_message: AssistantMessage,
}

struct AfterToolCallHookInput<'a> {
    try_hook: Option<&'a TryAfterToolCallHook>,
    legacy_hook: Option<&'a AfterToolCallHook>,
    current_context: &'a AgentContext,
    assistant_message: &'a AssistantMessage,
}

#[derive(Clone)]
struct FinalizedToolCallOutcome {
    tool_call: AgentToolCall,
    result: AgentToolResult,
    is_error: bool,
}

enum ToolCallPreparation {
    Prepared(PreparedToolCall),
    Immediate(FinalizedToolCallOutcome),
}

enum ToolTaskEvent<T> {
    Update {
        event: Box<AgentEvent>,
        acknowledgement: oneshot::Sender<()>,
    },
    Finished(Box<T>),
}

/// Owner-side guard that closes the shared update gate if execution returns, unwinds, or is
/// cancellation-dropped. Individual [`UpdateSink`] clones deliberately do not close on drop.
struct UpdateSinkCloseGuard(UpdateSink);

impl Drop for UpdateSinkCloseGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

struct ParallelToolCallFinished {
    index: usize,
    finalized: FinalizedToolCallOutcome,
}

/// Fail every call in an output-token-truncated assistant message. The adapter may have
/// salvaged syntactically valid but incomplete JSON, so none of the calls is safe to run.
pub(super) async fn fail_tool_calls_from_truncated_message<S>(
    tool_calls: &[AgentToolCall],
    sink: &mut S,
) -> ExecutedToolCallBatch
where
    S: AgentEventSink + ?Sized,
{
    let mut messages = Vec::with_capacity(tool_calls.len());

    for tool_call in tool_calls {
        emit_tool_execution_start(tool_call, sink).await;
        let finalized = FinalizedToolCallOutcome {
            tool_call: tool_call.clone(),
            result: create_error_tool_result(format!(
                "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                tool_call.name
            )),
            is_error: true,
        };
        emit_tool_execution_end(&finalized, sink).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(&message, sink).await;
        messages.push(message);
    }

    ExecutedToolCallBatch {
        messages,
        terminate: false,
    }
}

/// Execute all calls in an assistant message. Preflight is always source ordered. A global
/// sequential setting, or one sequential tool in the batch, makes the entire batch sequential.
pub(super) async fn execute_tool_calls<S>(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    sink: &mut S,
) -> ExecutedToolCallBatch
where
    S: AgentEventSink + ?Sized,
{
    let tool_calls = assistant_message.tool_calls().cloned().collect::<Vec<_>>();
    let has_sequential_tool = tool_calls.iter().any(|tool_call| {
        current_context
            .tools
            .iter()
            .find(|tool| tool.spec().name == tool_call.name)
            .and_then(|tool| tool.execution_mode())
            == Some(ToolExecutionMode::Sequential)
    });

    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential_tool {
        execute_tool_calls_sequential(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            sink,
        )
        .await
    } else {
        execute_tool_calls_parallel(
            current_context,
            assistant_message,
            &tool_calls,
            config,
            cancel,
            sink,
        )
        .await
    }
}

async fn execute_tool_calls_sequential<S>(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    sink: &mut S,
) -> ExecutedToolCallBatch
where
    S: AgentEventSink + ?Sized,
{
    let mut finalized_calls = Vec::with_capacity(tool_calls.len());
    let mut messages = Vec::with_capacity(tool_calls.len());

    for tool_call in tool_calls {
        emit_tool_execution_start(tool_call, sink).await;

        let finalized = match prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            cancel,
        )
        .await
        {
            ToolCallPreparation::Immediate(finalized) => finalized,
            ToolCallPreparation::Prepared(prepared) => {
                let executed = execute_prepared_tool_call_sequential(&prepared, cancel, sink).await;
                finalize_executed_tool_call(
                    prepared,
                    executed,
                    after_tool_call_input(config, current_context, assistant_message),
                    cancel.clone(),
                )
                .await
            }
        };

        emit_tool_execution_end(&finalized, sink).await;
        let message = create_tool_result_message(&finalized);
        emit_tool_result_message(&message, sink).await;
        finalized_calls.push(finalized);
        messages.push(message);

        // TypeScript checks the signal between sequential calls, after the current result has
        // been finalized and emitted.
        if cancel.is_cancelled() {
            break;
        }
    }

    ExecutedToolCallBatch {
        terminate: should_terminate_tool_batch(&finalized_calls),
        messages,
    }
}

/// Parallel plumbing keeps the loop task as the only event writer. Preflight is performed
/// sequentially, then prepared calls are polled concurrently. End events follow completion order;
/// transcript messages are emitted afterward in source order.
async fn execute_tool_calls_parallel<S>(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_calls: &[AgentToolCall],
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    sink: &mut S,
) -> ExecutedToolCallBatch
where
    S: AgentEventSink + ?Sized,
{
    let mut finalized_by_index = vec![None; tool_calls.len()];
    let mut prepared_calls = Vec::new();

    for (index, tool_call) in tool_calls.iter().enumerate() {
        emit_tool_execution_start(tool_call, sink).await;
        match prepare_tool_call(
            current_context,
            assistant_message,
            tool_call,
            config,
            cancel,
        )
        .await
        {
            ToolCallPreparation::Immediate(finalized) => {
                emit_tool_execution_end(&finalized, sink).await;
                finalized_by_index[index] = Some(finalized);
            }
            ToolCallPreparation::Prepared(prepared) => prepared_calls.push((index, prepared)),
        }

        if cancel.is_cancelled() {
            break;
        }
    }

    let task_count = prepared_calls.len();
    let after_tool_call_snapshot =
        (config.try_after_tool_call.is_some() || config.after_tool_call.is_some()).then(|| {
            Arc::new(AfterToolCallSnapshot {
                try_hook: config.try_after_tool_call.clone(),
                legacy_hook: config.after_tool_call.clone(),
                current_context: current_context.clone(),
                assistant_message: assistant_message.clone(),
            })
        });
    let (event_sender, mut event_receiver) =
        mpsc::unbounded_channel::<ToolTaskEvent<ParallelToolCallFinished>>();
    let mut running = JoinSet::new();

    for (index, prepared) in prepared_calls {
        let after_tool_call_snapshot = after_tool_call_snapshot.clone();
        let cancel = cancel.clone();
        let events = event_sender.clone();
        running.spawn(async move {
            let executed =
                execute_prepared_tool_call_buffered(&prepared, cancel.clone(), events.clone())
                    .await;
            let hook = after_tool_call_snapshot
                .as_deref()
                .map(|snapshot| AfterToolCallHookInput {
                    try_hook: snapshot.try_hook.as_ref(),
                    legacy_hook: snapshot.legacy_hook.as_ref(),
                    current_context: &snapshot.current_context,
                    assistant_message: &snapshot.assistant_message,
                });
            let finalized = finalize_executed_tool_call(prepared, executed, hook, cancel).await;
            let _ = events.send(ToolTaskEvent::Finished(Box::new(
                ParallelToolCallFinished { index, finalized },
            )));
        });
    }
    drop(event_sender);

    // Tool tasks keep running while an event is awaiting the single writer. Update
    // acknowledgements prevent settlement from overtaking an in-flight sink emission. Completion
    // is based on the known task and Finished counts, not channel closure: a settled tool may
    // legitimately retain a closed UpdateSink (and therefore the sender captured by its callback).
    let mut finished_count = 0;
    let mut joined_count = 0;
    while finished_count < task_count || joined_count < task_count {
        tokio::select! {
            maybe_event = event_receiver.recv(), if finished_count < task_count => {
                let Some(event) = maybe_event else {
                    panic!(
                        "parallel tool event channel closed after {finished_count} of {task_count} Finished events"
                    );
                };
                match event {
                    ToolTaskEvent::Update {
                        event,
                        acknowledgement,
                    } => {
                        sink.emit(*event).await;
                        let _ = acknowledgement.send(());
                    }
                    ToolTaskEvent::Finished(finished) => {
                        let ParallelToolCallFinished { index, finalized } = *finished;
                        emit_tool_execution_end(&finalized, sink).await;
                        finalized_by_index[index] = Some(finalized);
                        finished_count += 1;
                    }
                }
            }
            maybe_joined = running.join_next(), if joined_count < task_count => {
                let Some(joined) = maybe_joined else {
                    panic!(
                        "parallel tool JoinSet ended after {joined_count} of {task_count} tasks"
                    );
                };
                handle_tool_task_join(joined);
                joined_count += 1;
            }
        }
    }

    let finalized_calls = finalized_by_index.into_iter().flatten().collect::<Vec<_>>();
    let mut messages = Vec::with_capacity(finalized_calls.len());
    for finalized in &finalized_calls {
        let message = create_tool_result_message(finalized);
        emit_tool_result_message(&message, sink).await;
        messages.push(message);
    }

    ExecutedToolCallBatch {
        terminate: should_terminate_tool_batch(&finalized_calls),
        messages,
    }
}

async fn prepare_tool_call(
    current_context: &AgentContext,
    assistant_message: &AssistantMessage,
    tool_call: &AgentToolCall,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
) -> ToolCallPreparation {
    let Some(tool) = current_context
        .tools
        .iter()
        .find(|tool| tool.spec().name == tool_call.name)
        .cloned()
    else {
        return immediate_error(tool_call, format!("Tool {} not found", tool_call.name));
    };

    // Preparation precedes the one and only validation pass. The before hook receives the
    // validated value by mutable reference; its mutations intentionally are not revalidated.
    // `try_prepare_arguments` is the single preparation entry point: its default adapts the legacy
    // infallible transform, and an explicit fallible override takes precedence. A preparation
    // failure skips validation and execution with an in-band error result, mirroring pi's catch
    // around `prepareArguments`.
    let prepared_arguments = match tool.try_prepare_arguments(tool_call.arguments.clone()) {
        Ok(arguments) => arguments,
        Err(error) => return immediate_error(tool_call, error.to_string()),
    };
    let spec = tool.spec();
    let validated_args = match validate_tool_arguments(&spec, prepared_arguments) {
        Ok(arguments) => arguments,
        Err(error) => return immediate_error(tool_call, error.to_string()),
    };

    let args = if config.try_before_tool_call.is_some() || config.before_tool_call.is_some() {
        let mut hook_context = BeforeToolCallContext {
            assistant_message: assistant_message.clone(),
            tool_call: tool_call.clone(),
            args: validated_args,
            context: current_context.clone(),
        };
        // Each channel resolves exactly once per call: the fallible hook takes precedence when
        // installed, and the two hooks are never both invoked. A hook failure skips execution,
        // mirroring pi's catch around `beforeToolCall`; unlike a blocked call it does not request
        // batch termination.
        let before_result =
            match run_before_tool_call_channel(config, &mut hook_context, cancel).await {
                Ok(before_result) => before_result,
                Err(error) => return immediate_error(tool_call, error.to_string()),
            };
        if cancel.is_cancelled() {
            return immediate_error(tool_call, "Operation aborted");
        }
        if let Some(before_result) = before_result
            && before_result.block
        {
            // TS falsiness: an empty-string reason falls back to the default text.
            let reason = before_result
                .reason
                .filter(|reason| !reason.is_empty())
                .unwrap_or_else(|| "Tool execution was blocked".to_owned());
            let mut outcome = immediate_error(tool_call, reason);
            if before_result.terminate
                && let ToolCallPreparation::Immediate(finalized) = &mut outcome
            {
                finalized.result.terminate = true;
            }
            return outcome;
        }
        hook_context.args
    } else {
        validated_args
    };

    if cancel.is_cancelled() {
        return immediate_error(tool_call, "Operation aborted");
    }

    ToolCallPreparation::Prepared(PreparedToolCall {
        tool_call: tool_call.clone(),
        tool,
        args,
    })
}

/// Invoke the configured before-tool-call channel exactly once: the fallible hook when installed,
/// otherwise the legacy infallible hook adapted to `Ok`. Returns `Ok(None)` when neither is set.
async fn run_before_tool_call_channel(
    config: &AgentLoopConfig,
    hook_context: &mut BeforeToolCallContext,
    cancel: &CancellationToken,
) -> Result<Option<BeforeToolCallResult>, ToolHookError> {
    if let Some(try_hook) = &config.try_before_tool_call {
        try_hook(hook_context, cancel.clone()).await
    } else if let Some(legacy_hook) = &config.before_tool_call {
        Ok(legacy_hook(hook_context, cancel.clone()).await)
    } else {
        Ok(None)
    }
}

/// Build the after-tool-call channel input when either hook form is configured.
fn after_tool_call_input<'a>(
    config: &'a AgentLoopConfig,
    current_context: &'a AgentContext,
    assistant_message: &'a AssistantMessage,
) -> Option<AfterToolCallHookInput<'a>> {
    if config.try_after_tool_call.is_none() && config.after_tool_call.is_none() {
        return None;
    }
    Some(AfterToolCallHookInput {
        try_hook: config.try_after_tool_call.as_ref(),
        legacy_hook: config.after_tool_call.as_ref(),
        current_context,
        assistant_message,
    })
}

async fn execute_prepared_tool_call_sequential<S>(
    prepared: &PreparedToolCall,
    cancel: &CancellationToken,
    sink: &mut S,
) -> ExecutedToolCallOutcome
where
    S: AgentEventSink + ?Sized,
{
    let (event_sender, mut event_receiver) =
        mpsc::unbounded_channel::<ToolTaskEvent<ExecutedToolCallOutcome>>();
    let mut running = JoinSet::new();
    let prepared = prepared.clone();
    let cancel = cancel.clone();
    let events = event_sender.clone();
    running.spawn(async move {
        let executed = execute_prepared_tool_call_buffered(&prepared, cancel, events.clone()).await;
        let _ = events.send(ToolTaskEvent::Finished(Box::new(executed)));
    });
    drop(event_sender);

    // The tool must keep being polled while the single writer awaits an update listener. Its task
    // waits for every update acknowledgement before publishing Finished, so the after hook and end
    // event cannot overtake an update. Neither task completion nor a retained UpdateSink requires
    // the update channel itself to close.
    let mut executed = None;
    let mut task_joined = false;
    while executed.is_none() || !task_joined {
        tokio::select! {
            maybe_event = event_receiver.recv(), if executed.is_none() => {
                let Some(event) = maybe_event else {
                    panic!("sequential tool event channel closed before its Finished event");
                };
                match event {
                    ToolTaskEvent::Update {
                        event,
                        acknowledgement,
                    } => {
                        sink.emit(*event).await;
                        let _ = acknowledgement.send(());
                    }
                    ToolTaskEvent::Finished(outcome) => executed = Some(*outcome),
                }
            }
            maybe_joined = running.join_next(), if !task_joined => {
                let Some(joined) = maybe_joined else {
                    panic!("sequential tool JoinSet ended before its task completed");
                };
                handle_tool_task_join(joined);
                task_joined = true;
            }
        }
    }

    executed.expect("a Finished event is required before sequential dispatch completes")
}

async fn execute_prepared_tool_call_buffered<T>(
    prepared: &PreparedToolCall,
    cancel: CancellationToken,
    event_sender: mpsc::UnboundedSender<ToolTaskEvent<T>>,
) -> ExecutedToolCallOutcome
where
    T: Send + 'static,
{
    let tool_call_id = prepared.tool_call.id.clone();
    let tool_name = prepared.tool_call.name.clone();
    let original_args = prepared.tool_call.arguments.clone();
    let pending_acknowledgements = Arc::new(Mutex::new(Some(Vec::<oneshot::Receiver<()>>::new())));
    let acknowledgements_for_update = pending_acknowledgements.clone();
    let updates = UpdateSink::new(move |partial_result| {
        let mut pending = acknowledgements_for_update
            .lock()
            .expect("parallel update acknowledgement mutex poisoned");
        let Some(pending) = pending.as_mut() else {
            return;
        };

        let (acknowledgement, acknowledged) = oneshot::channel();
        if event_sender
            .send(ToolTaskEvent::Update {
                event: Box::new(AgentEvent::ToolExecutionUpdate {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    args: original_args.clone(),
                    partial_result,
                }),
                acknowledgement,
            })
            .is_ok()
        {
            pending.push(acknowledged);
        }
    });

    let close_updates_on_drop = UpdateSinkCloseGuard(updates.clone());
    let result = prepared
        .tool
        .execute(
            ToolCallContext::new(
                prepared.tool_call.id.clone(),
                prepared.tool_call.name.clone(),
                prepared.args.clone(),
            ),
            cancel,
            updates.clone(),
        )
        .await;
    drop(close_updates_on_drop);

    // Taking the list under the same mutex used by the synchronous callback gives close a firm
    // registration barrier: a callback already inside the critical section is included, while a
    // callback racing after settlement observes None and cannot enqueue another event.
    let acknowledgements = {
        let mut pending = pending_acknowledgements
            .lock()
            .expect("parallel update acknowledgement mutex poisoned");
        pending.take().unwrap_or_default()
    };
    for acknowledged in acknowledgements {
        let _ = acknowledged.await;
    }

    match result {
        Ok(result) => ExecutedToolCallOutcome {
            result,
            is_error: false,
        },
        Err(error) => ExecutedToolCallOutcome {
            result: create_error_tool_result(error.to_string()),
            is_error: true,
        },
    }
}

fn handle_tool_task_join(joined: Result<(), JoinError>) {
    if let Err(error) = joined {
        if error.is_panic() {
            std::panic::resume_unwind(error.into_panic());
        }
        panic!("tool task was cancelled: {error}");
    }
}

async fn finalize_executed_tool_call(
    prepared: PreparedToolCall,
    executed: ExecutedToolCallOutcome,
    after_tool_call: Option<AfterToolCallHookInput<'_>>,
    cancel: CancellationToken,
) -> FinalizedToolCallOutcome {
    let mut result = executed.result;
    let mut is_error = executed.is_error;

    if let Some(after_tool_call) = after_tool_call {
        let hook_context = AfterToolCallContext {
            assistant_message: after_tool_call.assistant_message.clone(),
            tool_call: prepared.tool_call.clone(),
            args: prepared.args,
            result: result.clone(),
            is_error,
            context: after_tool_call.current_context.clone(),
        };
        // Each channel resolves exactly once per call: the fallible hook takes precedence when
        // installed, and the two hooks are never both invoked.
        let after_result = if let Some(try_hook) = after_tool_call.try_hook {
            try_hook(hook_context, cancel).await
        } else if let Some(legacy_hook) = after_tool_call.legacy_hook {
            Ok(legacy_hook(hook_context, cancel).await)
        } else {
            Ok(None)
        };

        match after_result {
            Ok(Some(after_result)) => {
                if let Some(content) = after_result.content {
                    result.content = content;
                }
                if let Some(details) = after_result.details {
                    result.details = details;
                }
                if let Some(usage) = after_result.usage {
                    result.usage = Some(usage);
                }
                if let Some(terminate) = after_result.terminate {
                    result.terminate = terminate;
                }
                if let Some(after_is_error) = after_result.is_error {
                    is_error = after_is_error;
                }
            }
            Ok(None) => {}
            // An after-hook failure replaces the completed result with an in-band error result,
            // mirroring pi's catch around `afterToolCall`: content, details, usage, and any
            // termination request are discarded. Tool side effects are not rolled back —
            // execution already happened; only the model-visible result is replaced.
            Err(error) => {
                result = create_error_tool_result(error.to_string());
                is_error = true;
            }
        }
    }

    FinalizedToolCallOutcome {
        tool_call: prepared.tool_call,
        result,
        is_error,
    }
}

fn immediate_error(tool_call: &AgentToolCall, message: impl Into<String>) -> ToolCallPreparation {
    ToolCallPreparation::Immediate(FinalizedToolCallOutcome {
        tool_call: tool_call.clone(),
        result: create_error_tool_result(message),
        is_error: true,
    })
}

fn create_error_tool_result(message: impl Into<String>) -> AgentToolResult {
    AgentToolResult::new(vec![ToolResultContent::text(message)], json!({}))
}

fn should_terminate_tool_batch(finalized_calls: &[FinalizedToolCallOutcome]) -> bool {
    !finalized_calls.is_empty()
        && finalized_calls
            .iter()
            .all(|finalized| finalized.result.terminate)
}

async fn emit_tool_execution_start<S>(tool_call: &AgentToolCall, sink: &mut S)
where
    S: AgentEventSink + ?Sized,
{
    sink.emit(AgentEvent::ToolExecutionStart {
        tool_call_id: tool_call.id.clone(),
        tool_name: tool_call.name.clone(),
        args: tool_call.arguments.clone(),
    })
    .await;
}

async fn emit_tool_execution_end<S>(finalized: &FinalizedToolCallOutcome, sink: &mut S)
where
    S: AgentEventSink + ?Sized,
{
    sink.emit(AgentEvent::ToolExecutionEnd {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        result: finalized.result.clone(),
        is_error: finalized.is_error,
    })
    .await;
}

fn create_tool_result_message(finalized: &FinalizedToolCallOutcome) -> ToolResultMessage {
    let mut message = ToolResultMessage::new(
        finalized.tool_call.id.clone(),
        finalized.tool_call.name.clone(),
        finalized.result.content.clone(),
    );
    message.details = finalized.result.details.clone();
    message.usage = finalized.result.usage;
    message.added_tool_names = finalized.result.added_tool_names.clone();
    message.is_error = finalized.is_error;
    message
}

async fn emit_tool_result_message<S>(message: &ToolResultMessage, sink: &mut S)
where
    S: AgentEventSink + ?Sized,
{
    sink.emit(AgentEvent::MessageStart {
        message: message.clone().into(),
    })
    .await;
    sink.emit(AgentEvent::MessageEnd {
        message: message.clone().into(),
    })
    .await;
}
