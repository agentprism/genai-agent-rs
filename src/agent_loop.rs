//! Low-level agent-loop entry points and optional event-stream observation.
//!
//! Direct runners take one mutable [`crate::AgentEventSink`] and await each emission before doing
//! later loop work. Convenience constructors instead spawn the same loop behind an unbounded event
//! channel: consuming events is optional, while an [`AgentLoopResult`] provides an independent final
//! outcome and participates in the spawned task's lifetime.
//!
//! Provider and tool failures follow the in-band message/event protocol. Returned
//! [`crate::LoopError`] values are reserved for invocation guards, an unavailable stream function,
//! or a convenience task that violates the no-panic contract.

#[path = "tool_exec.rs"]
mod tool_exec;

use crate::{
    AgentContext, AgentEvent, AgentEventSink, AgentLoopConfig, AgentMessage, AssistantMessage,
    AssistantMessageEvent, LlmContext, LoopError, StopReason, StreamFn, StreamRequest,
    get_default_stream_fn,
};
use futures::stream::FusedStream;
use futures::{FutureExt, Stream, StreamExt};
use genai::adapter::AdapterKind;
use genai::{ModelIden, ModelSpec};
use std::any::Any;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tool_exec::{execute_tool_calls, fail_tool_calls_from_truncated_message};

type AgentLoopOutcome = Result<Vec<AgentMessage>, LoopError>;

/// Cloneable handle for awaiting a spawned convenience loop independently of its events.
///
/// Each live clone keeps the spawned task alive. If the event stream and every result handle are
/// dropped before completion, the task is aborted.
#[derive(Clone)]
pub struct AgentLoopResult {
    receiver: watch::Receiver<Option<AgentLoopOutcome>>,
    _task_lifetime: Arc<AbortTaskOnDrop>,
}

impl std::fmt::Debug for AgentLoopResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentLoopResult").finish_non_exhaustive()
    }
}

impl AgentLoopResult {
    /// Consume this handle and wait for the spawned loop to finish.
    ///
    /// Every clone observes the same outcome and receives its own cloned transcript or error. The
    /// transcript contains messages produced by the invocation, not its starting context. A panic
    /// in the task is reported as [`LoopError::TaskPanicked`].
    pub async fn get(mut self) -> AgentLoopOutcome {
        loop {
            if let Some(outcome) = self.receiver.borrow_and_update().clone() {
                return outcome;
            }
            if self.receiver.changed().await.is_err() {
                return Err(LoopError::TaskPanicked(
                    "agent loop task terminated without publishing an outcome".to_owned(),
                ));
            }
        }
    }
}

/// Optional event observation for a spawned low-level loop plus an independent final result.
///
/// Events use an unbounded, single-writer channel: polling them is optional and never applies
/// backpressure to loop execution, though a slow consumer can allow buffered memory to grow. Event
/// order matches the awaited sink emission order in the spawned task. Keeping a result handle alive
/// keeps the task alive even if event iteration is dropped. Once no stream or result owner remains,
/// an unfinished task is aborted.
#[must_use = "dropping the stream and all result handles aborts the loop task"]
pub struct AgentEventStream {
    receiver: mpsc::UnboundedReceiver<AgentEvent>,
    result: AgentLoopResult,
    terminated: bool,
}

impl std::fmt::Debug for AgentEventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentEventStream")
            .field("terminated", &self.terminated)
            .finish_non_exhaustive()
    }
}

impl AgentEventStream {
    /// Clone a result handle that resolves independently of event polling.
    ///
    /// The returned handle keeps an unfinished loop task alive if this stream is dropped.
    pub fn result_handle(&self) -> AgentLoopResult {
        self.result.clone()
    }

    /// Wait for the final outcome without taking ownership of event iteration.
    ///
    /// Events need not be polled for this future to resolve and remain available in the stream's
    /// unbounded channel until consumed or the stream is dropped.
    pub async fn result(&self) -> AgentLoopOutcome {
        self.result.clone().get().await
    }
}

impl Stream for AgentEventStream {
    type Item = AgentEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }

        match self.receiver.poll_recv(cx) {
            Poll::Ready(None) => {
                self.terminated = true;
                Poll::Ready(None)
            }
            poll => poll,
        }
    }
}

impl FusedStream for AgentEventStream {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

struct AbortTaskOnDrop {
    handle: tokio::task::AbortHandle,
}

impl Drop for AbortTaskOnDrop {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

enum AgentLoopInvocation {
    Start {
        prompts: Vec<AgentMessage>,
        context: AgentContext,
    },
    Continue {
        context: AgentContext,
    },
}

/// Spawn a new loop that appends prompt messages to the supplied context.
///
/// The process default [`StreamFn`] is resolved inside the task when `stream_fn` is `None`; resolution
/// failure is available through the result handle. Event observation is optional. Call
/// [`AgentEventStream::result`] or retain [`AgentEventStream::result_handle`] to obtain the messages
/// produced by the invocation without consuming every event.
pub fn agent_loop(
    prompts: Vec<AgentMessage>,
    context: AgentContext,
    config: AgentLoopConfig,
    cancel: CancellationToken,
    stream_fn: Option<Arc<dyn StreamFn>>,
) -> AgentEventStream {
    spawn_agent_loop(
        AgentLoopInvocation::Start { prompts, context },
        config,
        cancel,
        stream_fn,
    )
}

/// Spawn a loop that continues an existing transcript.
///
/// Empty transcripts and assistant-role tails are rejected synchronously before a task is spawned.
/// Role checks use [`AgentMessage::role`], so custom messages declaring the `"assistant"` role are
/// rejected too. The process default [`StreamFn`] is resolved inside the task when `stream_fn` is
/// `None`.
pub fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    cancel: CancellationToken,
    stream_fn: Option<Arc<dyn StreamFn>>,
) -> Result<AgentEventStream, LoopError> {
    validate_continue_context(&context)?;
    Ok(spawn_agent_loop(
        AgentLoopInvocation::Continue { context },
        config,
        cancel,
        stream_fn,
    ))
}

fn spawn_agent_loop(
    invocation: AgentLoopInvocation,
    config: AgentLoopConfig,
    cancel: CancellationToken,
    stream_fn: Option<Arc<dyn StreamFn>>,
) -> AgentEventStream {
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let (result_sender, result_receiver) = watch::channel(None);

    let task = tokio::spawn(async move {
        // This is the only event-channel writer. A dropped observer is not a loop failure: the
        // result handle may still be live, so continue driving the low-level run to completion.
        let mut sink = move |event| {
            let _send_failed = event_sender.send(event).is_err();
            std::future::ready(())
        };
        let run = async {
            match invocation {
                AgentLoopInvocation::Start { prompts, context } => {
                    run_agent_loop(prompts, context, config, &mut sink, cancel, stream_fn).await
                }
                AgentLoopInvocation::Continue { context } => {
                    run_agent_loop_continue(context, config, &mut sink, cancel, stream_fn).await
                }
            }
        };
        let outcome = match std::panic::AssertUnwindSafe(run).catch_unwind().await {
            Ok(outcome) => outcome,
            Err(payload) => Err(LoopError::TaskPanicked(panic_payload_message(
                payload.as_ref(),
            ))),
        };
        let _previous_outcome = result_sender.send_replace(Some(outcome));
    });
    let task_lifetime = Arc::new(AbortTaskOnDrop {
        handle: task.abort_handle(),
    });
    drop(task);

    AgentEventStream {
        receiver: event_receiver,
        result: AgentLoopResult {
            receiver: result_receiver,
            _task_lifetime: task_lifetime,
        },
        terminated: false,
    }
}

fn validate_continue_context(context: &AgentContext) -> Result<(), LoopError> {
    let last_role = context
        .messages
        .last()
        .map(AgentMessage::role)
        .ok_or(LoopError::EmptyContext)?;
    if last_role == "assistant" {
        return Err(LoopError::ContinueFromAssistant);
    }
    Ok(())
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "non-string panic payload".to_owned()
    }
}

/// Run a new low-level invocation by appending prompt messages to a context.
///
/// The returned vector contains only messages produced by this invocation, including the supplied
/// prompts; it does not return the starting transcript. Events are sent through one mutable sink and
/// every emission is awaited before later loop work. Provider, tool, and cancellation outcomes are
/// represented in-band and therefore normally return `Ok`; guard failures use [`LoopError`].
pub async fn run_agent_loop<S>(
    prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    config: AgentLoopConfig,
    sink: &mut S,
    cancel: CancellationToken,
    stream_fn: Option<Arc<dyn StreamFn>>,
) -> Result<Vec<AgentMessage>, LoopError>
where
    S: AgentEventSink + ?Sized,
{
    let stream_fn = stream_fn
        .or_else(get_default_stream_fn)
        .ok_or(LoopError::NoDefaultStreamFn)?;

    let mut new_messages = prompts.clone();
    context.messages.extend(prompts.iter().cloned());

    sink.emit(AgentEvent::AgentStart).await;
    sink.emit(AgentEvent::TurnStart).await;
    for prompt in prompts {
        sink.emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        })
        .await;
        sink.emit(AgentEvent::MessageEnd { message: prompt }).await;
    }

    run_loop(context, &mut new_messages, config, sink, cancel, stream_fn).await?;
    Ok(new_messages)
}

/// Continue a low-level invocation from an existing user, tool-result, or custom message.
///
/// The returned vector contains only messages produced by the continuation. Empty contexts and
/// assistant-role tails fail before any lifecycle event is emitted. As with [`run_agent_loop`], the
/// loop awaits every sink emission and carries runtime outcomes in-band.
pub async fn run_agent_loop_continue<S>(
    context: AgentContext,
    config: AgentLoopConfig,
    sink: &mut S,
    cancel: CancellationToken,
    stream_fn: Option<Arc<dyn StreamFn>>,
) -> Result<Vec<AgentMessage>, LoopError>
where
    S: AgentEventSink + ?Sized,
{
    validate_continue_context(&context)?;
    let stream_fn = stream_fn
        .or_else(get_default_stream_fn)
        .ok_or(LoopError::NoDefaultStreamFn)?;

    let mut new_messages = Vec::new();
    sink.emit(AgentEvent::AgentStart).await;
    sink.emit(AgentEvent::TurnStart).await;

    run_loop(context, &mut new_messages, config, sink, cancel, stream_fn).await?;
    Ok(new_messages)
}

async fn run_loop<S>(
    mut current_context: AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    sink: &mut S,
    cancel: CancellationToken,
    stream_fn: Arc<dyn StreamFn>,
) -> Result<(), LoopError>
where
    S: AgentEventSink + ?Sized,
{
    let mut first_turn = true;
    // A user may have queued input while the preceding owner was waiting to enter the loop.
    let mut pending_messages = match &config.get_steering_messages {
        Some(get_messages) => get_messages().await,
        None => Vec::new(),
    };

    // The outer loop restarts only when follow-up messages arrive after the agent would stop.
    loop {
        let mut has_more_tool_calls = true;

        // The inner loop covers automatic tool continuations and steering messages.
        while has_more_tool_calls || !pending_messages.is_empty() {
            if first_turn {
                first_turn = false;
            } else {
                sink.emit(AgentEvent::TurnStart).await;
            }

            if !pending_messages.is_empty() {
                for message in std::mem::take(&mut pending_messages) {
                    sink.emit(AgentEvent::MessageStart {
                        message: message.clone(),
                    })
                    .await;
                    sink.emit(AgentEvent::MessageEnd {
                        message: message.clone(),
                    })
                    .await;
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            let message = stream_assistant_response(
                &mut current_context,
                &config,
                &cancel,
                sink,
                stream_fn.as_ref(),
            )
            .await?;
            new_messages.push(message.clone().into());

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                sink.emit(AgentEvent::TurnEnd {
                    message: message.clone().into(),
                    tool_results: Vec::new(),
                })
                .await;
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(());
            }

            let tool_calls = message.tool_calls().cloned().collect::<Vec<_>>();
            let mut tool_results = Vec::new();
            has_more_tool_calls = false;

            if !tool_calls.is_empty() {
                let batch = if message.stop_reason == StopReason::Length {
                    fail_tool_calls_from_truncated_message(&tool_calls, sink).await
                } else {
                    execute_tool_calls(&current_context, &message, &config, &cancel, sink).await
                };
                has_more_tool_calls = !batch.terminate;
                tool_results = batch.messages;

                // Tool-result message events are emitted inside the executor first. Only after the
                // complete batch settles do results enter context, preserving the pre-batch hook
                // snapshot used by parallel execution.
                for result in &tool_results {
                    let message = AgentMessage::ToolResult(result.clone());
                    current_context.messages.push(message.clone());
                    new_messages.push(message);
                }
            }

            sink.emit(AgentEvent::TurnEnd {
                message: message.clone().into(),
                tool_results: tool_results.clone(),
            })
            .await;

            if let Some(prepare_next_turn) = config.prepare_next_turn.clone()
                && let Some(update) = prepare_next_turn(crate::PrepareNextTurnContext {
                    message: message.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                })
                .await
            {
                if let Some(context) = update.context {
                    current_context = context;
                }
                if let Some(model) = update.model {
                    config.model = model;
                }
                if let Some(thinking_level) = update.thinking_level {
                    config.chat_options.reasoning_effort = thinking_level.reasoning_effort();
                }
            }

            if let Some(should_stop_after_turn) = config.should_stop_after_turn.clone()
                && should_stop_after_turn(crate::ShouldStopAfterTurnContext {
                    message: message.clone(),
                    tool_results: tool_results.clone(),
                    context: current_context.clone(),
                    new_messages: new_messages.clone(),
                })
                .await
            {
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                })
                .await;
                return Ok(());
            }

            pending_messages = match &config.get_steering_messages {
                Some(get_messages) => get_messages().await,
                None => Vec::new(),
            };
        }

        let follow_up_messages = match &config.get_follow_up_messages {
            Some(get_messages) => get_messages().await,
            None => Vec::new(),
        };
        if follow_up_messages.is_empty() {
            break;
        }
        pending_messages = follow_up_messages;
    }

    sink.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    })
    .await;
    Ok(())
}

/// Transform and convert the widened transcript only at the provider boundary, then reduce the
/// assistant protocol while keeping the partial assistant message in the live context.
async fn stream_assistant_response<S>(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    cancel: &CancellationToken,
    sink: &mut S,
    stream_fn: &dyn StreamFn,
) -> Result<AssistantMessage, LoopError>
where
    S: AgentEventSink + ?Sized,
{
    let agent_messages = match &config.transform_context {
        Some(transform) => transform(context.messages.clone(), cancel.clone()).await,
        None => context.messages.clone(),
    };
    let llm_messages = (config.convert_to_llm)(agent_messages).await;
    let llm_context = LlmContext {
        system_prompt: context.system_prompt.clone(),
        messages: llm_messages,
        tools: context
            .tools
            .iter()
            .map(|tool| tool.spec().to_genai())
            .collect(),
    };
    let request = StreamRequest::new(config.model.clone(), llm_context)
        .with_options(config.chat_options.clone())
        .with_transport(config.transport)
        .with_cancellation(cancel.clone());
    let mut response = stream_fn.stream(request).await;
    let final_result = response.result_handle();

    let mut partial_message = None::<AssistantMessage>;
    let mut added_partial = false;

    loop {
        let event = tokio::select! {
            // Prefer a protocol event that is already ready over a simultaneous cancellation.
            biased;
            event = response.next() => event,
            _ = cancel.cancelled() => {
                let aborted = aborted_message(partial_message.as_ref(), &config.model);
                commit_final_message(context, sink, aborted.clone(), added_partial).await;
                return Ok(aborted);
            }
        };

        let Some(event) = event else {
            let final_message = match final_result.get().await {
                Ok(message) => message,
                Err(error) => AssistantMessage::error(
                    model_iden(&config.model),
                    StopReason::Error,
                    error.to_string(),
                ),
            };
            commit_final_message(context, sink, final_message.clone(), added_partial).await;
            return Ok(final_message);
        };

        match event {
            AssistantMessageEvent::Start { partial } => {
                partial_message = Some(partial.clone());
                context.messages.push(partial.clone().into());
                added_partial = true;
                sink.emit(AgentEvent::MessageStart {
                    message: partial.into(),
                })
                .await;
            }
            AssistantMessageEvent::Done { message, .. } => {
                commit_final_message(context, sink, message.clone(), added_partial).await;
                return Ok(message);
            }
            AssistantMessageEvent::Error { error, .. } => {
                commit_final_message(context, sink, error.clone(), added_partial).await;
                return Ok(error);
            }
            update => {
                if added_partial {
                    let partial = update.partial().clone();
                    partial_message = Some(partial.clone());
                    if let Some(last) = context.messages.last_mut() {
                        *last = partial.clone().into();
                    }
                    sink.emit(AgentEvent::MessageUpdate {
                        message: partial.into(),
                        assistant_message_event: update,
                    })
                    .await;
                }
            }
        }
    }
}

async fn commit_final_message<S>(
    context: &mut AgentContext,
    sink: &mut S,
    final_message: AssistantMessage,
    added_partial: bool,
) where
    S: AgentEventSink + ?Sized,
{
    if added_partial {
        if let Some(last) = context.messages.last_mut() {
            *last = final_message.clone().into();
        } else {
            // Defensive fallback for a context replacement by a non-conforming stream consumer.
            context.messages.push(final_message.clone().into());
        }
    } else {
        context.messages.push(final_message.clone().into());
        sink.emit(AgentEvent::MessageStart {
            message: final_message.clone().into(),
        })
        .await;
    }
    sink.emit(AgentEvent::MessageEnd {
        message: final_message.into(),
    })
    .await;
}

fn aborted_message(partial: Option<&AssistantMessage>, model: &ModelSpec) -> AssistantMessage {
    if let Some(partial) = partial {
        let mut message = partial.clone();
        message.stop_reason = StopReason::Aborted;
        message.error_message = Some("Operation aborted".to_owned());
        message
    } else {
        AssistantMessage::error(model_iden(model), StopReason::Aborted, "Operation aborted")
    }
}

fn model_iden(model: &ModelSpec) -> ModelIden {
    match model {
        ModelSpec::Iden(model) => model.clone(),
        ModelSpec::Target(target) => target.model.clone(),
        ModelSpec::Name(name) => {
            let name = name.to_string();
            ModelIden::new(
                AdapterKind::from_model(&name).unwrap_or(AdapterKind::Ollama),
                name,
            )
        }
    }
}
