//! pi-agent-runtime-tokio: Tokio environment implementation, the Send actor facade, native process execution. Architecture v2 part 2 §9.4–§9.6.
//!
//! Governing statement: `docs/porting-pi-ai-and-agent-core-docs/goal.md`. The architecture
//! documents beside it are the authority for shape; pi's pinned source
//! (`c49906ec77788625aacbdc53ebca6fbe65bd20f5`) is the reference for behavior.

#![deny(missing_docs)]

use futures_util::StreamExt;
use pi_agent_core::{
    Agent, AgentControl, AgentError, AgentEvent, AgentRecord, AgentSnapshot, MessageRole,
    PromptText, RunOutcome,
};
use pi_ai::{
    ApiId, AssistantAssembler, AssistantEvent, AssistantMessage, AssistantMessageSnapshot,
    CancellationToken, MessageId, ReplayEnvelope, ReplayScope, RunId, SendBoxFuture, Timestamp,
    Usage, UsageSource,
};
use std::{fmt, sync::Arc};
use tokio::sync::{mpsc, oneshot, watch};

/// Default capacity of the serialized actor command mailbox.
pub const DEFAULT_COMMAND_CAPACITY: usize = 64;

/// Default capacity of each run's observational event channel.
pub const DEFAULT_EVENT_CAPACITY: usize = 128;

/// Stable identity of one registered acknowledged event sink.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventSinkId(u64);

/// An ordered asynchronous observer whose completion is a producer barrier.
///
/// The actor invokes sinks in registration order for every event. It does not
/// poll the core run stream again until every returned future settles. In
/// particular, [`AgentEvent::RunFinished`] is not considered settled until all
/// sinks have acknowledged it.
pub trait AgentEventSink: Send + Sync + 'static {
    /// Observes one event with the active run cancellation capability.
    fn on_event(
        &self,
        event: AgentEvent,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, ()>;
}

impl<F> AgentEventSink for F
where
    F: Fn(AgentEvent, CancellationToken) -> SendBoxFuture<'static, ()> + Send + Sync + 'static,
{
    fn on_event(
        &self,
        event: AgentEvent,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, ()> {
        self(event, cancellation)
    }
}

/// Failure reported by the Tokio actor facade.
#[non_exhaustive]
#[derive(Debug)]
pub enum TokioAgentError {
    /// No Tokio runtime was active when the actor was constructed.
    NoRuntime,
    /// The actor command mailbox or owner task has closed.
    Closed,
    /// The core state machine rejected an operation.
    Agent(AgentError),
    /// The core stream ended without its mandatory terminal event.
    MissingRunFinished,
    /// The adapter could not mirror an event already accepted by the core.
    SnapshotInvariant {
        /// Sanitized invariant diagnostic.
        message: String,
    },
}

impl fmt::Display for TokioAgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoRuntime => {
                formatter.write_str("TokioAgentHandle requires an active Tokio runtime")
            }
            Self::Closed => formatter.write_str("Tokio agent actor is closed"),
            Self::Agent(error) => error.fmt(formatter),
            Self::MissingRunFinished => {
                formatter.write_str("agent stream ended without RunFinished")
            }
            Self::SnapshotInvariant { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for TokioAgentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Agent(error) => Some(error),
            Self::NoRuntime
            | Self::Closed
            | Self::MissingRunFinished
            | Self::SnapshotInvariant { .. } => None,
        }
    }
}

impl From<AgentError> for TokioAgentError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

/// One accepted actor-owned run.
///
/// Events are observational and delivered over a bounded channel. A caller
/// that retains this value should drain [`Self::events`] or call
/// [`Self::next_event`] while the run is active. Registered event sinks, rather
/// than this receiver, provide the explicit acknowledgement barrier contract.
pub struct TokioAgentRun {
    events: mpsc::Receiver<AgentEvent>,
    completion: oneshot::Receiver<Result<RunOutcome, TokioAgentError>>,
}

impl TokioAgentRun {
    /// Returns the bounded ordered event receiver for this run.
    pub fn events(&mut self) -> &mut mpsc::Receiver<AgentEvent> {
        &mut self.events
    }

    /// Receives the next observational event.
    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    /// Waits for the run and its `RunFinished` sink barriers to settle.
    pub async fn outcome(self) -> Result<RunOutcome, TokioAgentError> {
        self.completion.await.map_err(|_| TokioAgentError::Closed)?
    }
}

/// Tokio owner-task facade for the Send [`Agent`] family.
///
/// One task exclusively owns the agent. Prompt, continuation, retry, reset,
/// subscription, and shutdown commands are serialized through a bounded
/// mailbox. Queue ingress and cancellation retain the separate cloneable
/// [`AgentControl`] capability required by Architecture v2 part 2 §8.4, so
/// concurrent producers remain usable while an acknowledged sink is pending.
#[derive(Clone)]
pub struct TokioAgentHandle {
    command_tx: mpsc::Sender<AgentCommand>,
    state_rx: watch::Receiver<AgentSnapshot>,
    idle_rx: watch::Receiver<bool>,
    control: AgentControl,
    event_capacity: usize,
}

impl TokioAgentHandle {
    /// Starts an owner task using the default bounded capacities.
    pub fn new(agent: Agent) -> Result<Self, TokioAgentError> {
        Self::with_capacities(agent, DEFAULT_COMMAND_CAPACITY, DEFAULT_EVENT_CAPACITY)
    }

    /// Alias for [`Self::new`] emphasizing that construction starts the actor.
    pub fn spawn(agent: Agent) -> Result<Self, TokioAgentError> {
        Self::new(agent)
    }

    /// Starts an owner task with explicit bounded channel capacities.
    pub fn with_capacities(
        agent: Agent,
        command_capacity: usize,
        event_capacity: usize,
    ) -> Result<Self, TokioAgentError> {
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| TokioAgentError::NoRuntime)?;
        let command_capacity = command_capacity.max(1);
        let event_capacity = event_capacity.max(1);
        let snapshot = agent.snapshot();
        let control = agent.control();
        let (command_tx, command_rx) = mpsc::channel(command_capacity);
        let (state_tx, state_rx) = watch::channel(snapshot);
        let (idle_tx, idle_rx) = watch::channel(true);
        runtime.spawn(actor_loop(agent, command_rx, state_tx, idle_tx));
        Ok(Self {
            command_tx,
            state_rx,
            idle_rx,
            control,
            event_capacity,
        })
    }

    /// Starts a run from a text-and-image prompt.
    pub async fn prompt_text(&self, prompt: PromptText) -> Result<TokioAgentRun, TokioAgentError> {
        self.request_run(None, |channels| AgentCommand::PromptText {
            prompt,
            channels,
        })
        .await
    }

    /// Starts a run with an acknowledged sink scoped to that accepted run.
    ///
    /// The sink and prompt are submitted as one actor command. If another run
    /// is active, the command is rejected without registering or invoking the
    /// sink. An accepted sink observes only the events from the run created by
    /// this command and is removed automatically when that run settles.
    pub async fn prompt_text_with_sink(
        &self,
        prompt: PromptText,
        sink: Arc<dyn AgentEventSink>,
    ) -> Result<TokioAgentRun, TokioAgentError> {
        self.request_run(Some(sink), |channels| AgentCommand::PromptText {
            prompt,
            channels,
        })
        .await
    }

    /// Starts a run from an already identified record batch.
    pub async fn prompt_records(
        &self,
        records: impl IntoIterator<Item = AgentRecord>,
    ) -> Result<TokioAgentRun, TokioAgentError> {
        let records = records.into_iter().collect();
        self.request_run(None, |channels| AgentCommand::PromptRecords {
            records,
            channels,
        })
        .await
    }

    /// Continues from a user or tool-result tail, including Pi queue draining.
    pub async fn continue_run(&self) -> Result<TokioAgentRun, TokioAgentError> {
        self.request_run(None, |channels| AgentCommand::Continue { channels })
            .await
    }

    /// Retries the request boundary preceding an errored or aborted assistant.
    pub async fn retry_last_turn(&self) -> Result<TokioAgentRun, TokioAgentError> {
        self.request_run(None, |channels| AgentCommand::Retry { channels })
            .await
    }

    /// Enqueues steering through the independent bounded control capability.
    pub async fn steer(
        &self,
        message: AgentRecord,
    ) -> Result<pi_agent_core::QueueReceipt, pi_agent_core::ControlError> {
        self.control.steer(message).await
    }

    /// Enqueues follow-up work through the independent bounded control capability.
    pub async fn follow_up(
        &self,
        message: AgentRecord,
    ) -> Result<pi_agent_core::QueueReceipt, pi_agent_core::ControlError> {
        self.control.follow_up(message).await
    }

    /// Cancels the active run with the matching identity.
    pub fn cancel(&self, run_id: RunId) -> Result<(), pi_agent_core::ControlError> {
        self.control.cancel(run_id)
    }

    /// Registers one acknowledged sink after all previously registered sinks.
    pub async fn subscribe(
        &self,
        sink: Arc<dyn AgentEventSink>,
    ) -> Result<EventSinkId, TokioAgentError> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::Subscribe { sink, response })
            .await
            .map_err(|_| TokioAgentError::Closed)?;
        receiver.await.map_err(|_| TokioAgentError::Closed)
    }

    /// Removes an acknowledged sink after all earlier actor commands settle.
    pub async fn unsubscribe(&self, id: EventSinkId) -> Result<bool, TokioAgentError> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::Unsubscribe { id, response })
            .await
            .map_err(|_| TokioAgentError::Closed)?;
        receiver.await.map_err(|_| TokioAgentError::Closed)
    }

    /// Clears transcript and transient state with Pi's reset retention rules.
    pub async fn reset_transcript(&self) -> Result<(), TokioAgentError> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::ResetTranscript { response })
            .await
            .map_err(|_| TokioAgentError::Closed)?;
        receiver.await.map_err(|_| TokioAgentError::Closed)?
    }

    /// Restores builder defaults and clears transcript/run state.
    pub async fn reset_all(&self) -> Result<(), TokioAgentError> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::ResetAll { response })
            .await
            .map_err(|_| TokioAgentError::Closed)?;
        receiver.await.map_err(|_| TokioAgentError::Closed)?
    }

    /// Returns the most recently published owned agent snapshot.
    ///
    /// The actor publishes state before invoking event sinks, matching Pi's
    /// high-level reducer-before-listener ordering.
    pub fn snapshot(&self) -> AgentSnapshot {
        self.state_rx.borrow().clone()
    }

    /// Returns a watch receiver for published agent snapshots.
    pub fn snapshots(&self) -> watch::Receiver<AgentSnapshot> {
        self.state_rx.clone()
    }

    /// Resolves only after the current run and all `RunFinished` sinks settle.
    pub async fn wait_for_idle(&self) -> Result<(), TokioAgentError> {
        let mut idle = self.idle_rx.clone();
        loop {
            if *idle.borrow_and_update() {
                return Ok(());
            }
            idle.changed().await.map_err(|_| TokioAgentError::Closed)?;
        }
    }

    /// Gracefully stops the owner task, cancelling and settling an active run.
    pub async fn shutdown(&self) -> Result<(), TokioAgentError> {
        let (response, receiver) = oneshot::channel();
        self.command_tx
            .send(AgentCommand::Shutdown { response })
            .await
            .map_err(|_| TokioAgentError::Closed)?;
        receiver.await.map_err(|_| TokioAgentError::Closed)
    }

    async fn request_run(
        &self,
        sink: Option<Arc<dyn AgentEventSink>>,
        command: impl FnOnce(RunChannels) -> AgentCommand,
    ) -> Result<TokioAgentRun, TokioAgentError> {
        let (events_tx, events) = mpsc::channel(self.event_capacity);
        let (completion, completion_rx) = oneshot::channel();
        let (accepted, accepted_rx) = oneshot::channel();
        self.command_tx
            .send(command(RunChannels {
                events: events_tx,
                completion,
                accepted: Some(accepted),
                sink,
            }))
            .await
            .map_err(|_| TokioAgentError::Closed)?;
        accepted_rx.await.map_err(|_| TokioAgentError::Closed)??;
        Ok(TokioAgentRun {
            events,
            completion: completion_rx,
        })
    }
}

struct RunChannels {
    events: mpsc::Sender<AgentEvent>,
    completion: oneshot::Sender<Result<RunOutcome, TokioAgentError>>,
    accepted: Option<oneshot::Sender<Result<(), TokioAgentError>>>,
    sink: Option<Arc<dyn AgentEventSink>>,
}

enum AgentCommand {
    PromptText {
        prompt: PromptText,
        channels: RunChannels,
    },
    PromptRecords {
        records: Vec<AgentRecord>,
        channels: RunChannels,
    },
    Continue {
        channels: RunChannels,
    },
    Retry {
        channels: RunChannels,
    },
    ResetTranscript {
        response: oneshot::Sender<Result<(), TokioAgentError>>,
    },
    ResetAll {
        response: oneshot::Sender<Result<(), TokioAgentError>>,
    },
    Subscribe {
        sink: Arc<dyn AgentEventSink>,
        response: oneshot::Sender<EventSinkId>,
    },
    Unsubscribe {
        id: EventSinkId,
        response: oneshot::Sender<bool>,
    },
    Shutdown {
        response: oneshot::Sender<()>,
    },
}

struct RegisteredSink {
    id: EventSinkId,
    sink: Arc<dyn AgentEventSink>,
}

struct DriveResult {
    outcome: Result<RunOutcome, TokioAgentError>,
    shutdown_responses: Vec<oneshot::Sender<()>>,
    shutdown_requested: bool,
}

struct DriveContext<'a> {
    commands: &'a mut mpsc::Receiver<AgentCommand>,
    sinks: &'a mut Vec<RegisteredSink>,
    next_sink_id: &'a mut u64,
    state_tx: &'a watch::Sender<AgentSnapshot>,
    run_sink: Option<Arc<dyn AgentEventSink>>,
}

async fn actor_loop(
    mut agent: Agent,
    mut commands: mpsc::Receiver<AgentCommand>,
    state_tx: watch::Sender<AgentSnapshot>,
    idle_tx: watch::Sender<bool>,
) {
    let mut sinks = Vec::<RegisteredSink>::new();
    let mut next_sink_id = 1_u64;

    while let Some(command) = commands.recv().await {
        match command {
            AgentCommand::PromptText {
                prompt,
                mut channels,
            } => {
                let cancellation = CancellationToken::new();
                let stream = agent.prompt_text(prompt, cancellation.clone());
                if !accept_run(&idle_tx, &mut channels) {
                    drop(stream);
                    continue;
                }
                let RunChannels {
                    events,
                    completion,
                    accepted: _,
                    sink,
                } = channels;
                let result = drive_run(
                    stream,
                    cancellation,
                    agent_snapshot_seed(&state_tx),
                    events,
                    DriveContext {
                        commands: &mut commands,
                        sinks: &mut sinks,
                        next_sink_id: &mut next_sink_id,
                        state_tx: &state_tx,
                        run_sink: sink,
                    },
                )
                .await;
                if finish_owned_run(&agent, &state_tx, &idle_tx, completion, result) {
                    return;
                }
            }
            AgentCommand::PromptRecords {
                records,
                mut channels,
            } => {
                let cancellation = CancellationToken::new();
                let stream = agent.prompt_records(records, cancellation.clone());
                if !accept_run(&idle_tx, &mut channels) {
                    drop(stream);
                    continue;
                }
                let RunChannels {
                    events,
                    completion,
                    accepted: _,
                    sink,
                } = channels;
                let result = drive_run(
                    stream,
                    cancellation,
                    agent_snapshot_seed(&state_tx),
                    events,
                    DriveContext {
                        commands: &mut commands,
                        sinks: &mut sinks,
                        next_sink_id: &mut next_sink_id,
                        state_tx: &state_tx,
                        run_sink: sink,
                    },
                )
                .await;
                if finish_owned_run(&agent, &state_tx, &idle_tx, completion, result) {
                    return;
                }
            }
            AgentCommand::Continue { mut channels } => {
                let cancellation = CancellationToken::new();
                let stream = match agent.continue_run(cancellation.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        reject_run(channels, error.into());
                        continue;
                    }
                };
                if !accept_run(&idle_tx, &mut channels) {
                    drop(stream);
                    continue;
                }
                let RunChannels {
                    events,
                    completion,
                    accepted: _,
                    sink,
                } = channels;
                let result = drive_run(
                    stream,
                    cancellation,
                    agent_snapshot_seed(&state_tx),
                    events,
                    DriveContext {
                        commands: &mut commands,
                        sinks: &mut sinks,
                        next_sink_id: &mut next_sink_id,
                        state_tx: &state_tx,
                        run_sink: sink,
                    },
                )
                .await;
                if finish_owned_run(&agent, &state_tx, &idle_tx, completion, result) {
                    return;
                }
            }
            AgentCommand::Retry { mut channels } => {
                let cancellation = CancellationToken::new();
                let stream = match agent.retry_last_turn(cancellation.clone()) {
                    Ok(stream) => stream,
                    Err(error) => {
                        reject_run(channels, error.into());
                        continue;
                    }
                };
                if !accept_run(&idle_tx, &mut channels) {
                    drop(stream);
                    continue;
                }
                let RunChannels {
                    events,
                    completion,
                    accepted: _,
                    sink,
                } = channels;
                let result = drive_run(
                    stream,
                    cancellation,
                    agent_snapshot_seed(&state_tx),
                    events,
                    DriveContext {
                        commands: &mut commands,
                        sinks: &mut sinks,
                        next_sink_id: &mut next_sink_id,
                        state_tx: &state_tx,
                        run_sink: sink,
                    },
                )
                .await;
                if finish_owned_run(&agent, &state_tx, &idle_tx, completion, result) {
                    return;
                }
            }
            AgentCommand::ResetTranscript { response } => {
                let result = agent.reset_transcript().map_err(TokioAgentError::from);
                if result.is_ok() {
                    let _ = state_tx.send(agent.snapshot());
                }
                let _ = response.send(result);
            }
            AgentCommand::ResetAll { response } => {
                let result = agent.reset_all().map_err(TokioAgentError::from);
                if result.is_ok() {
                    let _ = state_tx.send(agent.snapshot());
                }
                let _ = response.send(result);
            }
            AgentCommand::Subscribe { sink, response } => {
                let id = allocate_sink_id(&mut next_sink_id);
                sinks.push(RegisteredSink { id, sink });
                let _ = response.send(id);
            }
            AgentCommand::Unsubscribe { id, response } => {
                let removed = remove_sink(&mut sinks, id);
                let _ = response.send(removed);
            }
            AgentCommand::Shutdown { response } => {
                let _ = response.send(());
                let _ = idle_tx.send(true);
                return;
            }
        }
    }
}

fn accept_run(idle_tx: &watch::Sender<bool>, channels: &mut RunChannels) -> bool {
    let Some(accepted) = channels.accepted.take() else {
        return false;
    };
    if accepted.is_closed() {
        return false;
    }
    let _ = idle_tx.send(false);
    accepted.send(Ok(())).is_ok()
}

fn reject_run(mut channels: RunChannels, error: TokioAgentError) {
    if let Some(accepted) = channels.accepted.take() {
        let _ = accepted.send(Err(error));
    }
}

fn agent_snapshot_seed(state_tx: &watch::Sender<AgentSnapshot>) -> AgentSnapshot {
    state_tx.borrow().clone()
}

fn finish_owned_run(
    agent: &Agent,
    state_tx: &watch::Sender<AgentSnapshot>,
    idle_tx: &watch::Sender<bool>,
    completion: oneshot::Sender<Result<RunOutcome, TokioAgentError>>,
    result: DriveResult,
) -> bool {
    let _ = state_tx.send(agent.snapshot());
    let _ = idle_tx.send(true);
    let _ = completion.send(result.outcome);
    for response in result.shutdown_responses {
        let _ = response.send(());
    }
    result.shutdown_requested
}

async fn drive_run<'a>(
    mut stream: pi_ai::SendBoxStream<'a, AgentEvent>,
    cancellation: CancellationToken,
    mut snapshot: AgentSnapshot,
    events: mpsc::Sender<AgentEvent>,
    context: DriveContext<'_>,
) -> DriveResult {
    let mut assembler = None::<AssistantAssembler>;
    let mut outcome = None;
    let mut shutdown_responses = Vec::new();
    let mut shutdown_requested = false;
    let mut commands_open = true;
    let mut events = Some(events);

    loop {
        tokio::select! {
            event = stream.next() => {
                let Some(event) = event else {
                    break;
                };
                if let Err(error) = apply_event_to_snapshot(&mut snapshot, &mut assembler, &event) {
                    return DriveResult {
                        outcome: Err(error),
                        shutdown_responses,
                        shutdown_requested,
                    };
                }
                let _ = context.state_tx.send(snapshot.clone());
                if let AgentEvent::RunFinished { outcome: run_outcome } = &event {
                    outcome = Some(run_outcome.clone());
                }
                dispatch_event(
                    &mut events,
                    event,
                    context.sinks,
                    context.run_sink.as_ref(),
                    cancellation.clone(),
                )
                .await;
            }
            command = context.commands.recv(), if commands_open => {
                let Some(command) = command else {
                    cancellation.cancel();
                    shutdown_requested = true;
                    commands_open = false;
                    continue;
                };
                match command {
                    AgentCommand::PromptText { channels, .. }
                    | AgentCommand::PromptRecords { channels, .. }
                    | AgentCommand::Continue { channels }
                    | AgentCommand::Retry { channels } => {
                        reject_run(channels, AgentError::RunActive.into());
                    }
                    AgentCommand::ResetTranscript { response }
                    | AgentCommand::ResetAll { response } => {
                        let _ = response.send(Err(AgentError::RunActive.into()));
                    }
                    AgentCommand::Subscribe { sink, response } => {
                        let id = allocate_sink_id(context.next_sink_id);
                        context.sinks.push(RegisteredSink { id, sink });
                        let _ = response.send(id);
                    }
                    AgentCommand::Unsubscribe { id, response } => {
                        let removed = remove_sink(context.sinks, id);
                        let _ = response.send(removed);
                    }
                    AgentCommand::Shutdown { response } => {
                        cancellation.cancel();
                        shutdown_responses.push(response);
                        shutdown_requested = true;
                    }
                }
            }
        }
    }

    DriveResult {
        outcome: outcome.ok_or(TokioAgentError::MissingRunFinished),
        shutdown_responses,
        shutdown_requested,
    }
}

async fn dispatch_event(
    events: &mut Option<mpsc::Sender<AgentEvent>>,
    event: AgentEvent,
    sinks: &[RegisteredSink],
    run_sink: Option<&Arc<dyn AgentEventSink>>,
    cancellation: CancellationToken,
) {
    if let Some(sender) = events
        && sender.send(event.clone()).await.is_err()
    {
        *events = None;
    }
    for registered in sinks {
        registered
            .sink
            .on_event(event.clone(), cancellation.clone())
            .await;
    }
    if let Some(run_sink) = run_sink {
        run_sink.on_event(event, cancellation).await;
    }
}

fn apply_event_to_snapshot(
    snapshot: &mut AgentSnapshot,
    assembler: &mut Option<AssistantAssembler>,
    event: &AgentEvent,
) -> Result<(), TokioAgentError> {
    snapshot.next_sequence = snapshot.next_sequence.checked_add(1).ok_or_else(|| {
        TokioAgentError::SnapshotInvariant {
            message: "agent snapshot event sequence overflowed".into(),
        }
    })?;

    match event {
        AgentEvent::AssistantUpdate { event, .. } => {
            if let AssistantEvent::MessageStarted { message_id, .. } = event {
                let started = snapshot.streaming.as_ref().ok_or_else(|| {
                    TokioAgentError::SnapshotInvariant {
                        message: "assistant MessageStarted update lacked outer lifecycle start"
                            .into(),
                    }
                })?;
                if started.id != *message_id {
                    return Err(TokioAgentError::SnapshotInvariant {
                        message: "assistant inner and outer MessageStarted identities differ"
                            .into(),
                    });
                }
                if assembler.is_some() {
                    return Err(TokioAgentError::SnapshotInvariant {
                        message: "assistant stream emitted MessageStarted more than once".into(),
                    });
                }
                *assembler = Some(AssistantAssembler::new());
            } else if assembler.is_none() {
                let message =
                    event
                        .terminal_message()
                        .ok_or_else(|| TokioAgentError::SnapshotInvariant {
                            message: "assistant update arrived without MessageStarted".into(),
                        })?;
                let started = snapshot.streaming.as_ref().ok_or_else(|| {
                    TokioAgentError::SnapshotInvariant {
                        message: "terminal-only assistant update lacked outer lifecycle start"
                            .into(),
                    }
                })?;
                if started.id != message.id {
                    return Err(TokioAgentError::SnapshotInvariant {
                        message: "terminal-only assistant identity differs from lifecycle start"
                            .into(),
                    });
                }
                snapshot.streaming = Some(snapshot_from_terminal_message(message));
                return Ok(());
            }
            let current = assembler
                .as_mut()
                .ok_or_else(|| TokioAgentError::SnapshotInvariant {
                    message: "assistant update arrived without MessageStarted".into(),
                })?;
            current
                .apply(event)
                .map_err(|error| TokioAgentError::SnapshotInvariant {
                    message: error.to_string(),
                })?;
            snapshot.streaming = Some(current.snapshot());
        }
        AgentEvent::MessageCommitted { message } => {
            snapshot.state.transcript.push(message.clone());
            if matches!(message, AgentRecord::Llm(pi_ai::Message::Assistant(_))) {
                snapshot.streaming = None;
                *assembler = None;
            }
        }
        AgentEvent::ToolExecutionStarted { call } => {
            let mut pending = snapshot.pending_tool_calls.to_vec();
            pending.push(call.id.clone());
            snapshot.pending_tool_calls = pending.into();
        }
        AgentEvent::ToolExecutionFinished { call_id, .. } => {
            snapshot.pending_tool_calls = snapshot
                .pending_tool_calls
                .iter()
                .filter(|pending| *pending != call_id)
                .cloned()
                .collect::<Vec<_>>()
                .into();
        }
        AgentEvent::RunFinished { .. } => {
            snapshot.streaming = None;
            snapshot.pending_tool_calls = Arc::from([]);
            *assembler = None;
        }
        AgentEvent::MessageStarted {
            message_id,
            role: MessageRole::Assistant,
        } => {
            if snapshot.streaming.is_some() || assembler.is_some() {
                return Err(TokioAgentError::SnapshotInvariant {
                    message: "assistant lifecycle started while another assistant was active"
                        .into(),
                });
            }
            snapshot.streaming = Some(started_assistant_snapshot(snapshot, message_id));
        }
        AgentEvent::RunStarted { .. }
        | AgentEvent::TurnStarted { .. }
        | AgentEvent::ContextPrepared { .. }
        | AgentEvent::MessageStarted { .. }
        | AgentEvent::ToolExecutionUpdated { .. }
        | AgentEvent::TurnFinished { .. } => {}
        _ => {}
    }
    Ok(())
}

fn started_assistant_snapshot(
    snapshot: &AgentSnapshot,
    message_id: &MessageId,
) -> AssistantMessageSnapshot {
    let provider = snapshot.state.model.provider.clone();
    let requested_model = snapshot.state.model.model.clone();
    let api = ApiId::default();
    AssistantMessageSnapshot {
        id: message_id.clone(),
        provider: provider.clone(),
        api: api.clone(),
        requested_model: requested_model.clone(),
        response_model: None,
        response_id: None,
        content: Vec::new(),
        replay: ReplayEnvelope::new(ReplayScope::new(
            provider,
            api,
            requested_model.clone(),
            requested_model,
        )),
        usage: Usage::zero(UsageSource::Unknown),
        timestamp: Timestamp::default(),
        terminal_message: None,
    }
}

fn snapshot_from_terminal_message(message: &AssistantMessage) -> AssistantMessageSnapshot {
    AssistantMessageSnapshot {
        id: message.id.clone(),
        provider: message.provider.clone(),
        api: message.api.clone(),
        requested_model: message.requested_model.clone(),
        response_model: message.response_model.clone(),
        response_id: message.response_id.clone(),
        content: message.content.clone(),
        replay: message.replay.clone(),
        usage: message.usage.clone(),
        timestamp: message.timestamp,
        terminal_message: Some(message.clone()),
    }
}

fn allocate_sink_id(next_sink_id: &mut u64) -> EventSinkId {
    let id = EventSinkId(*next_sink_id);
    *next_sink_id = next_sink_id.saturating_add(1);
    id
}

fn remove_sink(sinks: &mut Vec<RegisteredSink>, id: EventSinkId) -> bool {
    let before = sinks.len();
    sinks.retain(|registered| registered.id != id);
    sinks.len() != before
}
