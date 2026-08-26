//! Deterministic tool preflight and joined sequential/parallel execution
//! (Architecture v2 part 1 §4.6 and part 2 §8.1, §9.1).

use crate::{
    AfterToolCall, AgentContext, AgentRunContext, BeforeToolCall, DefaultToolPolicy,
    LocalAfterToolCall, LocalAgentContext, LocalBeforeToolCall, LocalToolPolicy, LocalToolRegistry,
    LocalToolUpdateSink, ToolAuthorization, ToolExecutionMode, ToolOutput, ToolPolicy,
    ToolRegistry, ToolUpdate, ToolUpdateError, ToolUpdateSink, validate_arguments,
};
use agentprism_ai::{
    AssistantFinishReason, AssistantMessage, CancellationToken, ContentBlockId, LocalBoxFuture,
    LocalBoxStream, SendBoxFuture, SendBoxStream, ToolCall, ToolCallId, ToolResultContent,
};
use futures_channel::mpsc::{Sender, channel};
use futures_util::{StreamExt, future::Either, stream::FuturesUnordered};
use serde_json::{Value, value::RawValue};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
};

const TOOL_UPDATE_SIGNAL_CAPACITY: usize = 32;

/// Position at which a call completed deterministic preflight.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PreflightIndex(pub usize);

/// Position at which a finalized call settled and became completion-event ready.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CompletionIndex(pub usize);

/// Position of a call in the assistant message.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SourceIndex(pub usize);

/// Batch schedule selected before execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExecutionPlan {
    /// All allowed calls execute concurrently after source-ordered preflight.
    ParallelBatch,
    /// Calls are prepared and executed one at a time in source order.
    ///
    /// This plan is selected when the configured mode is sequential or any
    /// resolved tool in the batch declares [`ToolExecutionMode::Sequential`].
    SequentialBatch,
}

/// Finalized outcome of one tool call with all three ordering domains.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolCallOutcome {
    /// Source-ordered preflight position.
    pub preflight_index: PreflightIndex,
    /// Actual finalized completion position.
    pub completion_index: CompletionIndex,
    /// Assistant source position.
    pub source_index: SourceIndex,
    /// Original call committed in the assistant message.
    pub call: ToolCall,
    /// Normalized and validated arguments, or the original value when
    /// preflight failed before normalization completed.
    pub effective_arguments: Value,
    /// Final output after post-execution policy.
    pub output: ToolOutput,
    /// Final error classification after post-execution policy.
    pub is_error: bool,
}

/// Complete joined outcome of one assistant tool-call batch.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolBatchOutcome {
    /// Schedule selected for the batch.
    pub plan: ToolExecutionPlan,
    /// Finalized results in actual completion order.
    pub completion_order: Vec<ToolCallOutcome>,
    /// The same finalized results in assistant source order.
    pub source_order: Vec<ToolCallOutcome>,
    /// Whether every finalized result requested termination.
    pub terminate: bool,
}

/// Live lifecycle item produced while a scheduler polls one joined tool batch.
///
/// Call lifecycle events preserve Pi's cross-call observation order. The
/// terminal batch value is emitted only after every launched future settles
/// and contains source-ordered copies for transcript commitment.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolBatchStreamEvent {
    /// The schedule selected before any call enters preflight.
    BatchStarted {
        /// Schedule governing lifecycle/result-message interleaving.
        plan: ToolExecutionPlan,
    },
    /// A call entered deterministic preflight.
    CallStarted {
        /// Source-ordered preflight position.
        preflight_index: PreflightIndex,
        /// Assistant source position.
        source_index: SourceIndex,
        /// Original assistant call.
        call: ToolCall,
    },
    /// An accepted update arrived while the call was still executing.
    CallUpdated {
        /// Assistant source position.
        source_index: SourceIndex,
        /// Stable call identity.
        call_id: ToolCallId,
        /// Partial result observation.
        update: ToolUpdate,
    },
    /// A call completed preflight or finalized execution.
    CallFinished {
        /// Final result indexed in actual lifecycle completion order.
        outcome: Box<ToolCallOutcome>,
    },
    /// Every launched future has settled and source-order commitment is safe.
    BatchFinished {
        /// Joined batch result.
        outcome: Box<ToolBatchOutcome>,
    },
}

/// Borrowed inputs shared by the Send and local batch schedulers.
pub struct ToolBatchRequest<'a, Tools = ToolRegistry> {
    /// Assistant message that owns the finalized calls.
    pub assistant: &'a AssistantMessage,
    /// Finalized calls in assistant source order.
    pub calls: &'a [ToolCall],
    /// Complete current run-local context supplied to tool policies.
    pub context: &'a AgentRunContext<Tools>,
    /// Agent-level default scheduling mode.
    pub configured_mode: ToolExecutionMode,
    /// Parent run cancellation signal.
    pub cancellation: CancellationToken,
}

/// Send-capable scheduler that owns the tool-policy seam.
#[derive(Clone)]
pub struct ToolScheduler {
    policy: Arc<dyn ToolPolicy>,
}

impl Default for ToolScheduler {
    fn default() -> Self {
        Self::new(Arc::new(DefaultToolPolicy))
    }
}

impl ToolScheduler {
    /// Creates a scheduler with an explicit authorization/finalization policy.
    pub fn new(policy: Arc<dyn ToolPolicy>) -> Self {
        Self { policy }
    }

    /// Returns the bound policy capability.
    pub fn policy(&self) -> &Arc<dyn ToolPolicy> {
        &self.policy
    }

    /// Runs deterministic preflight, executes according to the selected plan,
    /// and joins every launched future before returning.
    pub async fn execute_batch(
        &self,
        tools: &ToolRegistry,
        request: ToolBatchRequest<'_, ToolRegistry>,
    ) -> ToolBatchOutcome {
        let mut events = self.execute_batch_events(tools, request);
        while let Some(event) = events.next().await {
            if let ToolBatchStreamEvent::BatchFinished { outcome } = event {
                return *outcome;
            }
        }
        unreachable!("tool batch lifecycle stream always emits BatchFinished")
    }

    /// Polls preflight and execution while yielding starts, updates, and
    /// finalized completions as they become observable.
    pub fn execute_batch_events<'a>(
        &'a self,
        tools: &'a ToolRegistry,
        request: ToolBatchRequest<'a, ToolRegistry>,
    ) -> SendBoxStream<'a, ToolBatchStreamEvent> {
        send_batch_stream(self.policy.clone(), tools, request, None)
    }

    /// Executes a crash-recovery batch with a per-call durable argument seam.
    ///
    /// An entry in `prepared_arguments` is the already-prepared, validated,
    /// and authorized invocation intent recorded before the original process
    /// began executing that call. Such a call resolves its current executable
    /// tool but deliberately skips argument preparation, schema validation,
    /// and authorization. Calls absent from the map use ordinary preflight so
    /// a harness can recover a mixed batch containing both started and
    /// unstarted calls. Post-execution finalization still runs because it was
    /// not reached before a crash at the durable start boundary.
    pub fn execute_recovery_batch_events<'a>(
        &'a self,
        tools: &'a ToolRegistry,
        request: ToolBatchRequest<'a, ToolRegistry>,
        prepared_arguments: &'a BTreeMap<ToolCallId, Value>,
    ) -> SendBoxStream<'a, ToolBatchStreamEvent> {
        send_batch_stream(
            self.policy.clone(),
            tools,
            request,
            Some(prepared_arguments),
        )
    }
}

impl fmt::Debug for ToolScheduler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ToolScheduler { policy: <dyn ToolPolicy> }")
    }
}

/// Local/WASM scheduler with the same phase and ordering contract.
#[derive(Clone)]
pub struct LocalToolScheduler {
    policy: Rc<dyn LocalToolPolicy>,
}

impl Default for LocalToolScheduler {
    fn default() -> Self {
        Self::new(Rc::new(DefaultToolPolicy))
    }
}

impl LocalToolScheduler {
    /// Creates a local scheduler with an explicit policy.
    pub fn new(policy: Rc<dyn LocalToolPolicy>) -> Self {
        Self { policy }
    }

    /// Returns the bound local policy capability.
    pub fn policy(&self) -> &Rc<dyn LocalToolPolicy> {
        &self.policy
    }

    /// Runs and joins one local tool batch.
    pub async fn execute_batch(
        &self,
        tools: &LocalToolRegistry,
        request: ToolBatchRequest<'_, LocalToolRegistry>,
    ) -> ToolBatchOutcome {
        let mut events = self.execute_batch_events(tools, request);
        while let Some(event) = events.next().await {
            if let ToolBatchStreamEvent::BatchFinished { outcome } = event {
                return *outcome;
            }
        }
        unreachable!("local tool batch lifecycle stream always emits BatchFinished")
    }

    /// Local-executor counterpart of [`ToolScheduler::execute_batch_events`].
    pub fn execute_batch_events<'a>(
        &'a self,
        tools: &'a LocalToolRegistry,
        request: ToolBatchRequest<'a, LocalToolRegistry>,
    ) -> LocalBoxStream<'a, ToolBatchStreamEvent> {
        local_batch_stream(self.policy.clone(), tools, request, None)
    }

    /// Local-executor counterpart of
    /// [`ToolScheduler::execute_recovery_batch_events`].
    pub fn execute_recovery_batch_events<'a>(
        &'a self,
        tools: &'a LocalToolRegistry,
        request: ToolBatchRequest<'a, LocalToolRegistry>,
        prepared_arguments: &'a BTreeMap<ToolCallId, Value>,
    ) -> LocalBoxStream<'a, ToolBatchStreamEvent> {
        local_batch_stream(
            self.policy.clone(),
            tools,
            request,
            Some(prepared_arguments),
        )
    }
}

impl fmt::Debug for LocalToolScheduler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LocalToolScheduler { policy: <dyn LocalToolPolicy> }")
    }
}

#[derive(Clone)]
struct OutcomeBase {
    preflight_index: PreflightIndex,
    source_index: SourceIndex,
    call: ToolCall,
    effective_arguments: Value,
    output: ToolOutput,
    is_error: bool,
}

struct PreparedSendCall {
    preflight_index: PreflightIndex,
    source_index: SourceIndex,
    source_call: ToolCall,
    effective_call: ToolCall,
    validated_arguments: Value,
    tool: Arc<dyn crate::Tool>,
}

enum SendPreflight {
    Immediate(OutcomeBase),
    Prepared(PreparedSendCall),
}

struct PreparedLocalCall {
    preflight_index: PreflightIndex,
    source_index: SourceIndex,
    source_call: ToolCall,
    effective_call: ToolCall,
    validated_arguments: Value,
    tool: Rc<dyn crate::LocalTool>,
}

enum LocalPreflight {
    Immediate(OutcomeBase),
    Prepared(PreparedLocalCall),
}

fn send_plan(
    tools: &ToolRegistry,
    calls: &[ToolCall],
    configured_mode: ToolExecutionMode,
) -> ToolExecutionPlan {
    if configured_mode == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            tools
                .get(&call.name)
                .is_some_and(|tool| tool.execution_mode() == ToolExecutionMode::Sequential)
        })
    {
        ToolExecutionPlan::SequentialBatch
    } else {
        ToolExecutionPlan::ParallelBatch
    }
}

fn local_plan(
    tools: &LocalToolRegistry,
    calls: &[ToolCall],
    configured_mode: ToolExecutionMode,
) -> ToolExecutionPlan {
    if configured_mode == ToolExecutionMode::Sequential
        || calls.iter().any(|call| {
            tools
                .get(&call.name)
                .is_some_and(|tool| tool.execution_mode() == ToolExecutionMode::Sequential)
        })
    {
        ToolExecutionPlan::SequentialBatch
    } else {
        ToolExecutionPlan::ParallelBatch
    }
}

fn send_batch_stream<'a>(
    policy: Arc<dyn ToolPolicy>,
    tools: &'a ToolRegistry,
    request: ToolBatchRequest<'a, ToolRegistry>,
    prepared_arguments: Option<&'a BTreeMap<ToolCallId, Value>>,
) -> SendBoxStream<'a, ToolBatchStreamEvent> {
    Box::pin(async_stream::stream! {
        let truncated = request.assistant.finish.reason == AssistantFinishReason::Length;
        // Pi's length-truncation path is dedicated and does not resolve tools,
        // inspect execution modes, prepare/validate arguments, or invoke hooks.
        // It is sequential so each synthesized result is observable before the
        // next call starts.
        let plan = if truncated {
            ToolExecutionPlan::SequentialBatch
        } else {
            send_plan(tools, request.calls, request.configured_mode)
        };
        yield ToolBatchStreamEvent::BatchStarted { plan };
        let batch_cancellation = request.cancellation.child();
        let (sender, mut receiver) = channel(TOOL_UPDATE_SIGNAL_CAPACITY);
        let mut completed = Vec::new();

        if truncated {
            for (index, call) in request.calls.iter().cloned().enumerate() {
                let preflight_index = PreflightIndex(index);
                let source_index = SourceIndex(index);
                yield ToolBatchStreamEvent::CallStarted {
                    preflight_index,
                    source_index,
                    call: call.clone(),
                };
                yield record_completion(
                    truncated_error(preflight_index, source_index, call),
                    &mut completed,
                );
            }
        } else {
        match plan {
            ToolExecutionPlan::SequentialBatch => {
                for (index, call) in request.calls.iter().cloned().enumerate() {
                    let preflight_index = PreflightIndex(index);
                    let source_index = SourceIndex(index);
                    yield ToolBatchStreamEvent::CallStarted {
                        preflight_index,
                        source_index,
                        call: call.clone(),
                    };
                    let recovered_arguments = prepared_arguments
                        .and_then(|arguments| arguments.get(&call.id))
                        .cloned();
                    match preflight_send(
                        tools,
                        policy.clone(),
                        request.assistant,
                        request.context,
                        preflight_index,
                        source_index,
                        call,
                        recovered_arguments,
                        batch_cancellation.clone(),
                    ).await {
                        SendPreflight::Immediate(outcome) => {
                            yield record_completion(outcome, &mut completed);
                        }
                        SendPreflight::Prepared(prepared) => {
                            let mut running: FuturesUnordered<SendBoxFuture<'static, OutcomeBase>> =
                                FuturesUnordered::new();
                            let execution_sender = sender.clone();
                            let execution_policy = policy.clone();
                            let assistant = request.assistant.clone();
                            let context = request.context.clone();
                            let cancellation = batch_cancellation.clone();
                            running.push(Box::pin(async move {
                                execute_prepared_send(
                                    prepared,
                                    execution_policy,
                                    assistant,
                                    context,
                                    cancellation,
                                    execution_sender,
                                ).await
                            }));
                            while !running.is_empty() {
                                match futures_util::future::select(
                                    receiver.next(),
                                    running.next(),
                                ).await {
                                    Either::Left((Some(signal), _)) => {
                                        yield record_execution_signal(signal);
                                    }
                                    Either::Right((Some(outcome), _)) => {
                                        while let Ok(signal) = receiver.try_recv() {
                                            yield record_execution_signal(signal);
                                        }
                                        yield record_completion(outcome, &mut completed);
                                    }
                                    Either::Left((None, _)) | Either::Right((None, _)) => {}
                                }
                            }
                            while let Ok(signal) = receiver.try_recv() {
                                yield record_execution_signal(signal);
                            }
                        }
                    }
                    if batch_cancellation.is_cancelled() {
                        break;
                    }
                }
            }
            ToolExecutionPlan::ParallelBatch => {
                let mut prepared_calls = Vec::new();
                for (index, call) in request.calls.iter().cloned().enumerate() {
                    let preflight_index = PreflightIndex(index);
                    let source_index = SourceIndex(index);
                    yield ToolBatchStreamEvent::CallStarted {
                        preflight_index,
                        source_index,
                        call: call.clone(),
                    };
                    let recovered_arguments = prepared_arguments
                        .and_then(|arguments| arguments.get(&call.id))
                        .cloned();
                    match preflight_send(
                        tools,
                        policy.clone(),
                        request.assistant,
                        request.context,
                        preflight_index,
                        source_index,
                        call,
                        recovered_arguments,
                        batch_cancellation.clone(),
                    ).await {
                        SendPreflight::Immediate(outcome) => {
                            // Pi exposes an immediate preflight failure before
                            // beginning the next call's lifecycle.
                            yield record_completion(outcome, &mut completed);
                        }
                        SendPreflight::Prepared(prepared) => prepared_calls.push(prepared),
                    }
                    if batch_cancellation.is_cancelled() {
                        break;
                    }
                }

                let mut running: FuturesUnordered<SendBoxFuture<'static, OutcomeBase>> =
                    FuturesUnordered::new();
                for prepared in prepared_calls {
                    let execution_sender = sender.clone();
                    let execution_policy = policy.clone();
                    let assistant = request.assistant.clone();
                    let context = request.context.clone();
                    let cancellation = batch_cancellation.clone();
                    running.push(Box::pin(async move {
                        execute_prepared_send(
                            prepared,
                            execution_policy,
                            assistant,
                            context,
                            cancellation,
                            execution_sender,
                        ).await
                    }));
                }
                while !running.is_empty() {
                    match futures_util::future::select(
                        receiver.next(),
                        running.next(),
                    ).await {
                        Either::Left((Some(signal), _)) => {
                            yield record_execution_signal(signal);
                        }
                        Either::Right((Some(outcome), _)) => {
                            while let Ok(signal) = receiver.try_recv() {
                                yield record_execution_signal(signal);
                            }
                            yield record_completion(outcome, &mut completed);
                        }
                        Either::Left((None, _)) | Either::Right((None, _)) => {}
                    }
                }
                while let Ok(signal) = receiver.try_recv() {
                    yield record_execution_signal(signal);
                }
            }
        }
        }

        batch_cancellation.cancel();
        yield ToolBatchStreamEvent::BatchFinished {
            outcome: Box::new(finish_batch(plan, completed)),
        };
    })
}

#[allow(clippy::too_many_arguments)]
async fn preflight_send(
    tools: &ToolRegistry,
    policy: Arc<dyn ToolPolicy>,
    assistant: &AssistantMessage,
    context: &AgentContext,
    preflight_index: PreflightIndex,
    source_index: SourceIndex,
    call: ToolCall,
    recovered_arguments: Option<Value>,
    cancellation: CancellationToken,
) -> SendPreflight {
    let Some(binding) = tools.binding(&call.name) else {
        let message = format!("Tool {} not found", call.name);
        return SendPreflight::Immediate(immediate_error(
            preflight_index,
            source_index,
            call,
            &message,
            false,
        ));
    };
    if let Some(effective_arguments) = recovered_arguments {
        if cancellation.is_cancelled() {
            return SendPreflight::Immediate(immediate_error(
                preflight_index,
                source_index,
                call,
                "Operation aborted",
                false,
            ));
        }
        let mut effective_call = call.clone();
        effective_call.arguments = effective_arguments.clone();
        return SendPreflight::Prepared(PreparedSendCall {
            preflight_index,
            source_index,
            source_call: call,
            effective_call,
            // The pre-authorization validation evidence is intentionally not
            // durable. Recovery uses the lossless authorized intent for both
            // execution views rather than reconstructing obsolete scratch
            // state under current preparers or schemas.
            validated_arguments: effective_arguments,
            tool: binding.tool.clone(),
        });
    }
    let prepared_arguments = match &binding.preparer {
        Some(preparer) => match preparer.prepare(&call.arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return SendPreflight::Immediate(immediate_error(
                    preflight_index,
                    source_index,
                    call,
                    &error.message,
                    false,
                ));
            }
        },
        None => call.arguments.clone(),
    };
    let mut effective_arguments =
        match validate_arguments(binding.tool.spec(), &binding.validator, &prepared_arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return SendPreflight::Immediate(immediate_error(
                    preflight_index,
                    source_index,
                    call,
                    &error.message,
                    false,
                ));
            }
        };
    let validated_arguments = effective_arguments.clone();
    let authorization = policy
        .authorize(
            BeforeToolCall {
                assistant_message: assistant,
                tool_call: &call,
                args: &mut effective_arguments,
                context,
            },
            cancellation.clone(),
        )
        .await;
    let authorization = match authorization {
        Ok(authorization) => authorization,
        Err(error) => {
            return SendPreflight::Immediate(immediate_error(
                preflight_index,
                source_index,
                call,
                &error.to_string(),
                false,
            ));
        }
    };
    if cancellation.is_cancelled() {
        return SendPreflight::Immediate(immediate_error(
            preflight_index,
            source_index,
            call,
            "Operation aborted",
            false,
        ));
    }
    if let ToolAuthorization::Block { reason, terminate } = authorization {
        return SendPreflight::Immediate(immediate_error(
            preflight_index,
            source_index,
            call,
            reason.as_deref().unwrap_or("Tool execution was blocked"),
            terminate,
        ));
    }
    let mut effective_call = call.clone();
    effective_call.arguments = effective_arguments;
    SendPreflight::Prepared(PreparedSendCall {
        preflight_index,
        source_index,
        source_call: call,
        effective_call,
        validated_arguments,
        tool: binding.tool.clone(),
    })
}

async fn execute_prepared_send(
    prepared: PreparedSendCall,
    policy: Arc<dyn ToolPolicy>,
    assistant: AssistantMessage,
    context: AgentContext,
    cancellation: CancellationToken,
    events: Sender<ExecutionSignal>,
) -> OutcomeBase {
    let updates = Arc::new(SendUpdateCollector::new(
        prepared.source_index,
        prepared.source_call.id.clone(),
        events.clone(),
    ));
    let executed = prepared
        .tool
        .execute(
            crate::ToolCallContext {
                assistant_message_id: assistant.id.clone(),
                call: prepared.effective_call.clone(),
                validated_arguments: prepared.validated_arguments,
            },
            updates.clone(),
            cancellation.clone(),
        )
        .await;
    updates.close();
    let (mut output, mut is_error) = match executed {
        Ok(output) => (output, false),
        Err(error) => (error_output(&error.message, prepared.source_index), true),
    };
    match policy
        .finalize(
            AfterToolCall {
                assistant_message: &assistant,
                tool_call: &prepared.source_call,
                args: &prepared.effective_call.arguments,
                result: &output,
                is_error,
                context: &context,
            },
            cancellation,
        )
        .await
    {
        Ok(patch) => {
            (output, is_error) = patch.apply(output, is_error);
        }
        Err(error) => {
            output = error_output(&error.to_string(), prepared.source_index);
            is_error = true;
        }
    }
    OutcomeBase {
        preflight_index: prepared.preflight_index,
        source_index: prepared.source_index,
        call: prepared.source_call,
        effective_arguments: prepared.effective_call.arguments,
        output,
        is_error,
    }
}

fn local_batch_stream<'a>(
    policy: Rc<dyn LocalToolPolicy>,
    tools: &'a LocalToolRegistry,
    request: ToolBatchRequest<'a, LocalToolRegistry>,
    prepared_arguments: Option<&'a BTreeMap<ToolCallId, Value>>,
) -> LocalBoxStream<'a, ToolBatchStreamEvent> {
    Box::pin(async_stream::stream! {
        let truncated = request.assistant.finish.reason == AssistantFinishReason::Length;
        let plan = if truncated {
            ToolExecutionPlan::SequentialBatch
        } else {
            local_plan(tools, request.calls, request.configured_mode)
        };
        yield ToolBatchStreamEvent::BatchStarted { plan };
        let batch_cancellation = request.cancellation.child();
        let (sender, mut receiver) = channel(TOOL_UPDATE_SIGNAL_CAPACITY);
        let mut completed = Vec::new();

        if truncated {
            for (index, call) in request.calls.iter().cloned().enumerate() {
                let preflight_index = PreflightIndex(index);
                let source_index = SourceIndex(index);
                yield ToolBatchStreamEvent::CallStarted {
                    preflight_index,
                    source_index,
                    call: call.clone(),
                };
                yield record_completion(
                    truncated_error(preflight_index, source_index, call),
                    &mut completed,
                );
            }
        } else {
        match plan {
            ToolExecutionPlan::SequentialBatch => {
                for (index, call) in request.calls.iter().cloned().enumerate() {
                    let preflight_index = PreflightIndex(index);
                    let source_index = SourceIndex(index);
                    yield ToolBatchStreamEvent::CallStarted {
                        preflight_index,
                        source_index,
                        call: call.clone(),
                    };
                    let recovered_arguments = prepared_arguments
                        .and_then(|arguments| arguments.get(&call.id))
                        .cloned();
                    match preflight_local(
                        tools,
                        policy.clone(),
                        request.assistant,
                        request.context,
                        preflight_index,
                        source_index,
                        call,
                        recovered_arguments,
                        batch_cancellation.clone(),
                    ).await {
                        LocalPreflight::Immediate(outcome) => {
                            yield record_completion(outcome, &mut completed);
                        }
                        LocalPreflight::Prepared(prepared) => {
                            let mut running: FuturesUnordered<LocalBoxFuture<'static, OutcomeBase>> =
                                FuturesUnordered::new();
                            let execution_sender = sender.clone();
                            let execution_policy = policy.clone();
                            let assistant = request.assistant.clone();
                            let context = request.context.clone();
                            let cancellation = batch_cancellation.clone();
                            running.push(Box::pin(async move {
                                execute_prepared_local(
                                    prepared,
                                    execution_policy,
                                    assistant,
                                    context,
                                    cancellation,
                                    execution_sender,
                                ).await
                            }));
                            while !running.is_empty() {
                                match futures_util::future::select(
                                    receiver.next(),
                                    running.next(),
                                ).await {
                                    Either::Left((Some(signal), _)) => {
                                        yield record_execution_signal(signal);
                                    }
                                    Either::Right((Some(outcome), _)) => {
                                        while let Ok(signal) = receiver.try_recv() {
                                            yield record_execution_signal(signal);
                                        }
                                        yield record_completion(outcome, &mut completed);
                                    }
                                    Either::Left((None, _)) | Either::Right((None, _)) => {}
                                }
                            }
                            while let Ok(signal) = receiver.try_recv() {
                                yield record_execution_signal(signal);
                            }
                        }
                    }
                    if batch_cancellation.is_cancelled() {
                        break;
                    }
                }
            }
            ToolExecutionPlan::ParallelBatch => {
                let mut prepared_calls = Vec::new();
                for (index, call) in request.calls.iter().cloned().enumerate() {
                    let preflight_index = PreflightIndex(index);
                    let source_index = SourceIndex(index);
                    yield ToolBatchStreamEvent::CallStarted {
                        preflight_index,
                        source_index,
                        call: call.clone(),
                    };
                    let recovered_arguments = prepared_arguments
                        .and_then(|arguments| arguments.get(&call.id))
                        .cloned();
                    match preflight_local(
                        tools,
                        policy.clone(),
                        request.assistant,
                        request.context,
                        preflight_index,
                        source_index,
                        call,
                        recovered_arguments,
                        batch_cancellation.clone(),
                    ).await {
                        LocalPreflight::Immediate(outcome) => {
                            yield record_completion(outcome, &mut completed);
                        }
                        LocalPreflight::Prepared(prepared) => prepared_calls.push(prepared),
                    }
                    if batch_cancellation.is_cancelled() {
                        break;
                    }
                }

                let mut running: FuturesUnordered<LocalBoxFuture<'static, OutcomeBase>> =
                    FuturesUnordered::new();
                for prepared in prepared_calls {
                    let execution_sender = sender.clone();
                    let execution_policy = policy.clone();
                    let assistant = request.assistant.clone();
                    let context = request.context.clone();
                    let cancellation = batch_cancellation.clone();
                    running.push(Box::pin(async move {
                        execute_prepared_local(
                            prepared,
                            execution_policy,
                            assistant,
                            context,
                            cancellation,
                            execution_sender,
                        ).await
                    }));
                }
                while !running.is_empty() {
                    match futures_util::future::select(
                        receiver.next(),
                        running.next(),
                    ).await {
                        Either::Left((Some(signal), _)) => {
                            yield record_execution_signal(signal);
                        }
                        Either::Right((Some(outcome), _)) => {
                            while let Ok(signal) = receiver.try_recv() {
                                yield record_execution_signal(signal);
                            }
                            yield record_completion(outcome, &mut completed);
                        }
                        Either::Left((None, _)) | Either::Right((None, _)) => {}
                    }
                }
                while let Ok(signal) = receiver.try_recv() {
                    yield record_execution_signal(signal);
                }
            }
        }
        }

        batch_cancellation.cancel();
        yield ToolBatchStreamEvent::BatchFinished {
            outcome: Box::new(finish_batch(plan, completed)),
        };
    })
}

#[allow(clippy::too_many_arguments)]
async fn preflight_local(
    tools: &LocalToolRegistry,
    policy: Rc<dyn LocalToolPolicy>,
    assistant: &AssistantMessage,
    context: &LocalAgentContext,
    preflight_index: PreflightIndex,
    source_index: SourceIndex,
    call: ToolCall,
    recovered_arguments: Option<Value>,
    cancellation: CancellationToken,
) -> LocalPreflight {
    let Some(binding) = tools.binding(&call.name) else {
        let message = format!("Tool {} not found", call.name);
        return LocalPreflight::Immediate(immediate_error(
            preflight_index,
            source_index,
            call,
            &message,
            false,
        ));
    };
    if let Some(effective_arguments) = recovered_arguments {
        if cancellation.is_cancelled() {
            return LocalPreflight::Immediate(immediate_error(
                preflight_index,
                source_index,
                call,
                "Operation aborted",
                false,
            ));
        }
        let mut effective_call = call.clone();
        effective_call.arguments = effective_arguments.clone();
        return LocalPreflight::Prepared(PreparedLocalCall {
            preflight_index,
            source_index,
            source_call: call,
            effective_call,
            validated_arguments: effective_arguments,
            tool: binding.tool.clone(),
        });
    }
    let prepared_arguments = match &binding.preparer {
        Some(preparer) => match preparer.prepare(&call.arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return LocalPreflight::Immediate(immediate_error(
                    preflight_index,
                    source_index,
                    call,
                    &error.message,
                    false,
                ));
            }
        },
        None => call.arguments.clone(),
    };
    let mut effective_arguments =
        match validate_arguments(binding.tool.spec(), &binding.validator, &prepared_arguments) {
            Ok(arguments) => arguments,
            Err(error) => {
                return LocalPreflight::Immediate(immediate_error(
                    preflight_index,
                    source_index,
                    call,
                    &error.message,
                    false,
                ));
            }
        };
    let validated_arguments = effective_arguments.clone();
    let authorization = policy
        .authorize(
            LocalBeforeToolCall {
                assistant_message: assistant,
                tool_call: &call,
                args: &mut effective_arguments,
                context,
            },
            cancellation.clone(),
        )
        .await;
    let authorization = match authorization {
        Ok(authorization) => authorization,
        Err(error) => {
            return LocalPreflight::Immediate(immediate_error(
                preflight_index,
                source_index,
                call,
                &error.to_string(),
                false,
            ));
        }
    };
    if cancellation.is_cancelled() {
        return LocalPreflight::Immediate(immediate_error(
            preflight_index,
            source_index,
            call,
            "Operation aborted",
            false,
        ));
    }
    if let ToolAuthorization::Block { reason, terminate } = authorization {
        return LocalPreflight::Immediate(immediate_error(
            preflight_index,
            source_index,
            call,
            reason.as_deref().unwrap_or("Tool execution was blocked"),
            terminate,
        ));
    }
    let mut effective_call = call.clone();
    effective_call.arguments = effective_arguments;
    LocalPreflight::Prepared(PreparedLocalCall {
        preflight_index,
        source_index,
        source_call: call,
        effective_call,
        validated_arguments,
        tool: binding.tool.clone(),
    })
}

async fn execute_prepared_local(
    prepared: PreparedLocalCall,
    policy: Rc<dyn LocalToolPolicy>,
    assistant: AssistantMessage,
    context: LocalAgentContext,
    cancellation: CancellationToken,
    events: Sender<ExecutionSignal>,
) -> OutcomeBase {
    let updates = Rc::new(LocalUpdateCollector::new(
        prepared.source_index,
        prepared.source_call.id.clone(),
        events.clone(),
    ));
    let executed = prepared
        .tool
        .execute(
            crate::ToolCallContext {
                assistant_message_id: assistant.id.clone(),
                call: prepared.effective_call.clone(),
                validated_arguments: prepared.validated_arguments,
            },
            updates.clone(),
            cancellation.clone(),
        )
        .await;
    updates.close();
    let (mut output, mut is_error) = match executed {
        Ok(output) => (output, false),
        Err(error) => (error_output(&error.message, prepared.source_index), true),
    };
    match policy
        .finalize(
            LocalAfterToolCall {
                assistant_message: &assistant,
                tool_call: &prepared.source_call,
                args: &prepared.effective_call.arguments,
                result: &output,
                is_error,
                context: &context,
            },
            cancellation,
        )
        .await
    {
        Ok(patch) => {
            (output, is_error) = patch.apply(output, is_error);
        }
        Err(error) => {
            output = error_output(&error.to_string(), prepared.source_index);
            is_error = true;
        }
    }
    OutcomeBase {
        preflight_index: prepared.preflight_index,
        source_index: prepared.source_index,
        call: prepared.source_call,
        effective_arguments: prepared.effective_call.arguments,
        output,
        is_error,
    }
}

fn truncated_error(
    preflight_index: PreflightIndex,
    source_index: SourceIndex,
    call: ToolCall,
) -> OutcomeBase {
    let message = format!(
        "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
        call.name
    );
    immediate_error(preflight_index, source_index, call, &message, false)
}

fn immediate_error(
    preflight_index: PreflightIndex,
    source_index: SourceIndex,
    call: ToolCall,
    message: &str,
    terminate: bool,
) -> OutcomeBase {
    let effective_arguments = call.arguments.clone();
    let mut output = error_output(message, source_index);
    output.terminate = terminate;
    OutcomeBase {
        preflight_index,
        source_index,
        call,
        effective_arguments,
        output,
        is_error: true,
    }
}

fn error_output(message: &str, source_index: SourceIndex) -> ToolOutput {
    let mut output = ToolOutput::new(vec![ToolResultContent::Text {
        id: ContentBlockId::new(format!("agent-tool-error-{}", source_index.0)),
        text: message.into(),
    }]);
    output.details = Some(
        RawValue::from_string("{}".into())
            .expect("the static empty tool-error details object is valid JSON"),
    );
    output
}

fn finish_batch(
    plan: ToolExecutionPlan,
    completion_order: Vec<ToolCallOutcome>,
) -> ToolBatchOutcome {
    let terminate = !completion_order.is_empty()
        && completion_order
            .iter()
            .all(|outcome| outcome.output.terminate);
    let mut source_order = completion_order.clone();
    source_order.sort_by_key(|outcome| outcome.source_index);
    ToolBatchOutcome {
        plan,
        completion_order,
        source_order,
        terminate,
    }
}

enum ExecutionSignal {
    Updated {
        source_index: SourceIndex,
        call_id: ToolCallId,
        update: ToolUpdate,
    },
}

fn record_execution_signal(signal: ExecutionSignal) -> ToolBatchStreamEvent {
    match signal {
        ExecutionSignal::Updated {
            source_index,
            call_id,
            update,
        } => ToolBatchStreamEvent::CallUpdated {
            source_index,
            call_id,
            update,
        },
    }
}

fn record_completion(
    outcome: OutcomeBase,
    completed: &mut Vec<ToolCallOutcome>,
) -> ToolBatchStreamEvent {
    let outcome = ToolCallOutcome {
        preflight_index: outcome.preflight_index,
        completion_index: CompletionIndex(completed.len()),
        source_index: outcome.source_index,
        call: outcome.call,
        effective_arguments: outcome.effective_arguments,
        output: outcome.output,
        is_error: outcome.is_error,
    };
    completed.push(outcome.clone());
    ToolBatchStreamEvent::CallFinished {
        outcome: Box::new(outcome),
    }
}

struct SendUpdateCollector {
    state: Mutex<UpdateCollectorState>,
    source_index: SourceIndex,
    call_id: ToolCallId,
}

struct UpdateCollectorState {
    closed: bool,
    events: Sender<ExecutionSignal>,
}

impl SendUpdateCollector {
    fn new(
        source_index: SourceIndex,
        call_id: ToolCallId,
        events: Sender<ExecutionSignal>,
    ) -> Self {
        Self {
            state: Mutex::new(UpdateCollectorState {
                closed: false,
                events,
            }),
            source_index,
            call_id,
        }
    }

    fn close(&self) {
        let mut state = lock_unpoisoned(&self.state);
        state.closed = true;
    }
}

impl ToolUpdateSink for SendUpdateCollector {
    fn update(&self, update: ToolUpdate) -> Result<(), ToolUpdateError> {
        let mut state = lock_unpoisoned(&self.state);
        enqueue_update(&mut state, self.source_index, &self.call_id, update)
    }
}

struct LocalUpdateCollector {
    state: RefCell<UpdateCollectorState>,
    source_index: SourceIndex,
    call_id: ToolCallId,
}

impl LocalUpdateCollector {
    fn new(
        source_index: SourceIndex,
        call_id: ToolCallId,
        events: Sender<ExecutionSignal>,
    ) -> Self {
        Self {
            state: RefCell::new(UpdateCollectorState {
                closed: false,
                events,
            }),
            source_index,
            call_id,
        }
    }

    fn close(&self) {
        let mut state = self.state.borrow_mut();
        state.closed = true;
    }
}

impl LocalToolUpdateSink for LocalUpdateCollector {
    fn update(&self, update: ToolUpdate) -> Result<(), ToolUpdateError> {
        let mut state = self.state.borrow_mut();
        enqueue_update(&mut state, self.source_index, &self.call_id, update)
    }
}

fn enqueue_update(
    state: &mut UpdateCollectorState,
    source_index: SourceIndex,
    call_id: &ToolCallId,
    update: ToolUpdate,
) -> Result<(), ToolUpdateError> {
    if state.closed {
        return Ok(());
    }
    state
        .events
        .try_send(ExecutionSignal::Updated {
            source_index,
            call_id: call_id.clone(),
            update,
        })
        .map_err(|error| {
            if error.is_full() {
                ToolUpdateError::new("tool update signal buffer is full")
            } else {
                ToolUpdateError::new("tool update signal receiver is closed")
            }
        })
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
