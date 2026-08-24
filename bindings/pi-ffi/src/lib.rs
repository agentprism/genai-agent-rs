//! Runtime-owning C and Swift binding facade for `pi-ai` and
//! `pi-agent-core` (Architecture v2 part 1 §6, part 2 §6.5 and §9.4).
//!
//! The exported surfaces use opaque objects, owned strings, integer run IDs,
//! and versioned JSON. Rust futures, streams, trait objects, references, and
//! Tokio types remain behind this crate.

#![deny(missing_docs)]

mod auth_session;
pub mod c_api;

use auth_session::{
    AuthSessionCore, RespondDisposition, cancelled_message, completed_message, failed_message,
};
use pi_agent_core::{Agent, AgentEvent, AgentState, PromptImage, PromptText, ToolRegistry};
use pi_agent_runtime_tokio::{AgentEventSink, TokioAgentError, TokioAgentHandle, TokioAgentRun};
use pi_ai::{
    AuthAnswer, AuthChallengeId, AuthError, AuthEvent, AuthHostCapabilities, AuthHtmlPage,
    AuthInteraction, AuthPrompt, AuthResolver, CancellationReason, CancellationToken, Credential,
    ModelRef, ModelRuntime, Models, OAuthCredential, OAuthDeviceCodePoll,
    OAuthDeviceCodePollOptions, OAuthDeviceCodePollResult, ProviderId, ProviderOAuthExtra,
    ProviderRegistration, PublicError, ReasoningLevel, RedirectReceiverRequest, RedirectStrategy,
    ResolvedAuth, ScriptedResponse, ScriptedRuntime, SecretString, SendBoxFuture, Timestamp,
    parse_oauth_authorization_input, poll_oauth_device_code_flow, select_first_valid,
    text_response, tool_call_response, validate_oauth_state,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::Duration;
use tokio::runtime::{Builder as RuntimeBuilder, Handle as RuntimeHandle};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

uniffi::setup_scaffolding!();

/// Current JSON schema for C and generated-language event envelopes.
pub const PI_EVENT_ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// Current accepted schema for binding configuration JSON.
pub const PI_BINDING_CONFIG_SCHEMA_VERSION: u32 = 1;

/// Capacity of each foreign-polled agent event queue.
///
/// A full queue applies backpressure through the acknowledged actor sink. No
/// lifecycle, commit, tool-result, or terminal event is dropped.
pub const PI_AGENT_EVENT_QUEUE_CAPACITY: usize = 32;

/// Stable failure returned by the generated binding facade.
#[derive(Debug, uniffi::Error)]
#[uniffi(flat_error)]
pub enum PiFfiError {
    /// Input was not valid JSON or did not match its schema.
    InvalidJson {
        /// Sanitized parser diagnostic.
        message: String,
    },
    /// A versioned input used an unsupported schema.
    UnsupportedSchema {
        /// Version supplied by the caller.
        found: u32,
    },
    /// The binding-owned runtime could not be created or entered.
    Runtime {
        /// Sanitized runtime diagnostic.
        message: String,
    },
    /// The agent facade rejected an operation.
    Agent {
        /// Sanitized agent diagnostic.
        message: String,
    },
    /// No active or queued event stream has this external run identity.
    UnknownRun {
        /// External run identity.
        run_id: u64,
    },
    /// No open or closed challenge has this identity.
    UnknownChallenge {
        /// Host-supplied challenge identity.
        challenge_id: String,
    },
    /// A response did not match the open challenge's accepted response types.
    InvalidAuthResponse {
        /// Open challenge identity.
        challenge_id: String,
    },
    /// The auth session has no further events.
    SessionClosed,
    /// A required binding invariant was violated.
    BindingInvariant {
        /// Sanitized invariant diagnostic.
        message: String,
    },
    /// A binding-owned worker thread could not be created.
    Thread {
        /// Sanitized thread diagnostic.
        message: String,
    },
    /// The opaque object has been shut down.
    Closed,
}

impl fmt::Display for PiFfiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson { message }
            | Self::Runtime { message }
            | Self::Agent { message }
            | Self::BindingInvariant { message }
            | Self::Thread { message } => formatter.write_str(message),
            Self::UnsupportedSchema { found } => {
                write!(formatter, "unsupported binding schema version {found}")
            }
            Self::UnknownRun { run_id } => write!(formatter, "unknown run {run_id}"),
            Self::UnknownChallenge { challenge_id } => {
                write!(formatter, "unknown authentication challenge {challenge_id}")
            }
            Self::InvalidAuthResponse { challenge_id } => write!(
                formatter,
                "response does not match authentication challenge {challenge_id}"
            ),
            Self::SessionClosed => formatter.write_str("authentication session is closed"),
            Self::Closed => formatter.write_str("binding object is closed"),
        }
    }
}

impl std::error::Error for PiFfiError {}

impl From<TokioAgentError> for PiFfiError {
    fn from(error: TokioAgentError) -> Self {
        Self::Agent {
            message: error.to_string(),
        }
    }
}

/// Classification returned by one blocking authentication-session `next` call.
#[derive(Clone, Copy, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum PiAuthSessionEventStatus {
    /// A challenge or informational event is available in `json`.
    Challenge,
    /// Login completed and the credential was persisted.
    Completed,
    /// Login failed with a sanitized error.
    Failed,
    /// The host cancelled login.
    Cancelled,
}

/// One versioned authentication session event for generated bindings.
#[derive(Clone, Debug, Eq, PartialEq, uniffi::Record)]
pub struct PiAuthSessionEvent {
    /// Event classification.
    pub status: PiAuthSessionEventStatus,
    /// Versioned JSON challenge or terminal payload.
    pub json: String,
}

/// Result of responding to an authentication challenge.
#[derive(Clone, Copy, Debug, Eq, PartialEq, uniffi::Enum)]
pub enum PiAuthResponseStatus {
    /// The response matched and closed the accepting challenge path.
    Accepted,
    /// Another valid response or terminal state already closed the challenge.
    ChallengeSuperseded,
}

/// Generated-binding object that owns model execution and auth control planes.
#[derive(uniffi::Object)]
pub struct PiModels {
    inner: Arc<ModelsBindingInner>,
}

struct ModelsBindingInner {
    runtime: Arc<BindingRuntime>,
    model_runtime: Arc<dyn ModelRuntime>,
    models: Arc<Models>,
}

#[uniffi::export]
impl PiModels {
    /// Creates a version-one binding control plane from JSON configuration.
    #[uniffi::constructor]
    pub fn new(config_json: String) -> Result<Arc<Self>, PiFfiError> {
        Self::from_json(&config_json)
    }

    /// Creates an agent whose concrete runtime remains hidden behind the
    /// narrow `ModelRuntime` capability.
    pub fn create_agent(&self, config_json: String) -> Result<Arc<PiAgent>, PiFfiError> {
        let config = parse_versioned::<AgentConfig>(&config_json, "agent configuration")?;
        let state = AgentState::new(config.system_prompt, config.model, config.reasoning);
        let agent = Agent::new(
            Arc::clone(&self.inner.model_runtime),
            state,
            ToolRegistry::new(),
        )
        .map_err(|error| PiFfiError::Agent {
            message: error.to_string(),
        })?;
        let handle = self
            .inner
            .runtime
            .block_on(async move { TokioAgentHandle::new(agent).map_err(PiFfiError::from) })??;
        Ok(Arc::new(PiAgent::new(
            Arc::clone(&self.inner.runtime),
            handle,
        )))
    }

    /// Begins provider login as an explicit host-driven challenge/response
    /// session. Device polling remains inside Rust.
    pub fn auth_login_begin(
        &self,
        provider_id: String,
        auth_type: String,
        host_capabilities_json: String,
    ) -> Result<Arc<PiAuthSession>, PiFfiError> {
        let capabilities = parse_host_capabilities(&host_capabilities_json)?;
        let (core, interaction, sender) = AuthSessionCore::new(capabilities, auth_type);
        let cancellation = core.cancellation();
        let provider = ProviderId::new(provider_id.clone());
        let models = Arc::clone(&self.inner.models);
        self.inner.runtime.spawn(async move {
            let result = models
                .login(provider, interaction.clone(), cancellation.clone())
                .await;
            interaction.finish_open_challenges();
            let terminal = match result {
                Ok(_) => completed_message(&provider_id),
                Err(AuthError::Cancelled) => cancelled_message(),
                Err(error) => failed_message(error.code(), &error.to_string()),
            };
            let _ = sender.send(terminal);
        });
        Ok(Arc::new(PiAuthSession {
            core,
            _runtime: Arc::clone(&self.inner.runtime),
        }))
    }
}

impl PiModels {
    /// Rust-facing constructor used by the C facade and examples.
    pub fn from_json(config_json: &str) -> Result<Arc<Self>, PiFfiError> {
        let config = parse_versioned::<ModelsConfig>(config_json, "models configuration")?;
        let runtime = BindingRuntime::new()?;
        let mut builder = Models::builder();
        for auth_provider in config.auth_providers {
            let provider_id = auth_provider.provider_id.clone();
            let flow = auth_provider.into_flow()?;
            let resolver = Arc::new(ScriptedAuthResolver { flow });
            let registration = ProviderRegistration::builder(provider_id)
                .auth(resolver)
                .build()
                .map_err(|error| PiFfiError::InvalidJson {
                    message: format!("invalid scripted auth provider: {error}"),
                })?;
            builder = builder.provider(registration);
        }
        let models = Arc::new(builder.build().map_err(|error| PiFfiError::InvalidJson {
            message: format!("invalid models configuration: {error}"),
        })?);
        let model_runtime: Arc<dyn ModelRuntime> = match config.runtime {
            RuntimeConfig::Models => Arc::new((*models).clone()),
            RuntimeConfig::Scripted { responses } => Arc::new(ScriptedRuntime::new(
                responses
                    .into_iter()
                    .map(ScriptedResponseConfig::into_response),
            )),
        };
        Ok(Arc::new(Self {
            inner: Arc::new(ModelsBindingInner {
                runtime,
                model_runtime,
                models,
            }),
        }))
    }
}

/// Generated-binding object that serializes one agent and exposes polled,
/// sequenced JSON envelopes.
#[derive(uniffi::Object)]
pub struct PiAgent {
    runtime: Arc<BindingRuntime>,
    shared: Arc<AgentBindingShared>,
    shutdown: AtomicBool,
}

#[uniffi::export]
impl PiAgent {
    /// Starts a run and returns its binding-stable integer identity.
    ///
    /// Polled delivery uses a bounded queue. The acknowledged actor sink
    /// applies backpressure when the host stops calling [`Self::next_event`];
    /// no commit or terminal envelope is discarded.
    pub fn run(&self, input_json: String) -> Result<u64, PiFfiError> {
        self.start_polled(&input_json)
    }

    /// Blocks until the next envelope for `run_id`, returning `None` after the
    /// terminal envelope has been drained. Hosts must keep polling active runs
    /// so bounded delivery can make progress.
    pub fn next_event(&self, run_id: u64) -> Result<Option<String>, PiFfiError> {
        self.next_polled_event(run_id)
    }

    /// Cancels the active run with this binding identity.
    pub fn cancel(&self, run_id: u64) -> Result<(), PiFfiError> {
        self.cancel_run(run_id)
    }

    /// Waits until every active run has emitted and delivered its final event.
    pub fn wait_for_idle(&self) -> Result<(), PiFfiError> {
        self.wait_idle();
        Ok(())
    }

    /// Cancels active work, waits for delivery, and closes the actor owner task.
    pub fn shutdown(&self) -> Result<(), PiFfiError> {
        self.shutdown_inner()
    }
}

impl PiAgent {
    fn new(runtime: Arc<BindingRuntime>, handle: TokioAgentHandle) -> Self {
        Self {
            runtime,
            shared: Arc::new(AgentBindingShared {
                handle,
                state: Mutex::new(AgentBindingState::default()),
                idle: Condvar::new(),
                next_run_id: AtomicU64::new(1),
                next_sequence: AtomicU64::new(1),
            }),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Starts a run whose envelopes are delivered through the C callback.
    pub(crate) fn start_callback(
        &self,
        input_json: &str,
        callback: RawEventCallback,
    ) -> Result<u64, PiFfiError> {
        self.start_run(input_json, PendingEventDestination::Callback(callback))
    }

    /// Starts a run whose envelopes are read with [`Self::next_polled_event`].
    pub fn start_polled(&self, input_json: &str) -> Result<u64, PiFfiError> {
        let (sender, receiver) = tokio_mpsc::channel(PI_AGENT_EVENT_QUEUE_CAPACITY);
        let queue = Arc::new(RunEventQueue {
            receiver: Mutex::new(receiver),
        });
        self.start_run(
            input_json,
            PendingEventDestination::Polled {
                sender,
                queue: Arc::clone(&queue),
            },
        )
    }

    fn start_run(
        &self,
        input_json: &str,
        destination: PendingEventDestination,
    ) -> Result<u64, PiFfiError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(PiFfiError::Closed);
        }
        let prompt = parse_run_input(input_json)?;
        let external_run_id = self
            .shared
            .next_run_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map_err(|_| PiFfiError::BindingInvariant {
                message: "binding run identity overflowed".into(),
            })?;
        let mut state = lock_unpoisoned(&self.shared.state);
        if state.closed {
            return Err(PiFfiError::Closed);
        }
        let destination = match destination {
            PendingEventDestination::Polled { sender, queue } => {
                state.queues.insert(external_run_id, queue);
                EventDestination::Polled(sender)
            }
            PendingEventDestination::Callback(callback) => EventDestination::Callback(callback),
        };
        state.active.insert(
            external_run_id,
            BindingRunState::Pending {
                cancellation_requested: false,
            },
        );
        drop(state);

        let destination = Arc::new(destination);
        let sink = Arc::new(BindingEventSink {
            shared: Arc::clone(&self.shared),
            external_run_id,
            destination: Arc::clone(&destination),
            core_run_id: Mutex::new(None),
            terminal_delivered: AtomicBool::new(false),
        });
        let run = match self
            .runtime
            .block_on(self.shared.handle.prompt_text_with_sink(prompt, sink))?
        {
            Ok(run) => run,
            Err(error) => {
                self.remove_unstarted_run(external_run_id);
                return Err(error.into());
            }
        };

        let shared = Arc::clone(&self.shared);
        self.runtime.spawn(async move {
            drive_run(shared, external_run_id, run).await;
        });
        Ok(external_run_id)
    }

    fn remove_unstarted_run(&self, run_id: u64) {
        let mut state = lock_unpoisoned(&self.shared.state);
        state.active.remove(&run_id);
        state.queues.remove(&run_id);
        self.shared.idle.notify_all();
    }

    /// Reads a C/Swift envelope from a polled run queue.
    pub fn next_polled_event(&self, run_id: u64) -> Result<Option<String>, PiFfiError> {
        let queue = {
            let state = lock_unpoisoned(&self.shared.state);
            state.queues.get(&run_id).cloned()
        }
        .ok_or(PiFfiError::UnknownRun { run_id })?;
        let mut receiver = lock_unpoisoned(&queue.receiver);
        match self.runtime.block_on(receiver.recv())? {
            Some(event) => Ok(Some(event)),
            None => {
                drop(receiver);
                lock_unpoisoned(&self.shared.state).queues.remove(&run_id);
                Ok(None)
            }
        }
    }

    /// Cancels through the core's independent `AgentControl` capability.
    pub fn cancel_run(&self, run_id: u64) -> Result<(), PiFfiError> {
        let core_run_id = {
            let mut state = lock_unpoisoned(&self.shared.state);
            match state.active.get_mut(&run_id) {
                Some(BindingRunState::Pending {
                    cancellation_requested,
                }) => {
                    *cancellation_requested = true;
                    return Ok(());
                }
                Some(BindingRunState::Active { core_run_id }) => core_run_id.clone(),
                None => return Err(PiFfiError::UnknownRun { run_id }),
            }
        };
        self.shared
            .handle
            .cancel(core_run_id)
            .map_err(|error| PiFfiError::Agent {
                message: error.to_string(),
            })
    }

    fn wait_idle(&self) {
        let mut state = lock_unpoisoned(&self.shared.state);
        while !state.active.is_empty() {
            state = self
                .shared
                .idle
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn shutdown_inner(&self) -> Result<(), PiFfiError> {
        if self.shutdown.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let active = {
            let mut state = lock_unpoisoned(&self.shared.state);
            state.closed = true;
            state
                .active
                .values_mut()
                .filter_map(|run| match run {
                    BindingRunState::Pending {
                        cancellation_requested,
                    } => {
                        *cancellation_requested = true;
                        None
                    }
                    BindingRunState::Active { core_run_id } => Some(core_run_id.clone()),
                })
                .collect::<Vec<_>>()
        };
        for run_id in active {
            let _ = self.shared.handle.cancel(run_id);
        }
        self.wait_idle();
        self.runtime.block_on(self.shared.handle.shutdown())??;
        Ok(())
    }
}

impl Drop for PiAgent {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

/// Generated-binding object for the part 2 §6.5 auth session protocol.
#[derive(uniffi::Object)]
pub struct PiAuthSession {
    core: Arc<AuthSessionCore>,
    _runtime: Arc<BindingRuntime>,
}

#[uniffi::export]
impl PiAuthSession {
    /// Blocks until the next challenge, progress update, or terminal status.
    pub fn next(&self) -> Result<PiAuthSessionEvent, PiFfiError> {
        self.core.next()
    }

    /// Responds to one open challenge. A late losing response returns
    /// `ChallengeSuperseded` without failing the session.
    pub fn respond(
        &self,
        challenge_id: String,
        response_json: String,
    ) -> Result<PiAuthResponseStatus, PiFfiError> {
        match self.core.respond(&challenge_id, &response_json)? {
            RespondDisposition::Accepted => Ok(PiAuthResponseStatus::Accepted),
            RespondDisposition::Superseded => Ok(PiAuthResponseStatus::ChallengeSuperseded),
        }
    }

    /// Cancels provider work and every pending challenge waiter.
    pub fn cancel(&self) {
        self.core.cancel();
    }
}

impl Drop for PiAuthSession {
    fn drop(&mut self) {
        self.core.cancel();
    }
}

/// C callback and host-data pair retained only for one active run.
#[derive(Clone, Copy)]
pub(crate) struct RawEventCallback {
    pub(crate) callback: unsafe extern "C" fn(*const std::ffi::c_char, *mut std::ffi::c_void),
    pub(crate) user_data: usize,
}

struct AgentBindingShared {
    handle: TokioAgentHandle,
    state: Mutex<AgentBindingState>,
    idle: Condvar,
    next_run_id: AtomicU64,
    next_sequence: AtomicU64,
}

#[derive(Default)]
struct AgentBindingState {
    closed: bool,
    active: HashMap<u64, BindingRunState>,
    queues: HashMap<u64, Arc<RunEventQueue>>,
}

enum BindingRunState {
    Pending { cancellation_requested: bool },
    Active { core_run_id: pi_ai::RunId },
}

struct RunEventQueue {
    receiver: Mutex<tokio_mpsc::Receiver<String>>,
}

enum EventDestination {
    Callback(RawEventCallback),
    Polled(tokio_mpsc::Sender<String>),
}

struct BindingEventSink {
    shared: Arc<AgentBindingShared>,
    external_run_id: u64,
    destination: Arc<EventDestination>,
    core_run_id: Mutex<Option<pi_ai::RunId>>,
    terminal_delivered: AtomicBool,
}

impl AgentEventSink for BindingEventSink {
    fn on_event(
        &self,
        event: AgentEvent,
        _cancellation: CancellationToken,
    ) -> SendBoxFuture<'static, ()> {
        if self.terminal_delivered.load(Ordering::Acquire) {
            return Box::pin(async {});
        }

        match &event {
            AgentEvent::RunStarted { run_id } => {
                let mut claimed_run_id = lock_unpoisoned(&self.core_run_id);
                if let Some(claimed_run_id) = claimed_run_id.as_ref() {
                    if claimed_run_id != run_id {
                        return Box::pin(async {});
                    }
                } else {
                    *claimed_run_id = Some(run_id.clone());
                }

                let cancellation_requested = {
                    let mut state = lock_unpoisoned(&self.shared.state);
                    let Some(active) = state.active.get_mut(&self.external_run_id) else {
                        return Box::pin(async {});
                    };
                    let cancellation_requested = matches!(
                        active,
                        BindingRunState::Pending {
                            cancellation_requested: true
                        }
                    );
                    *active = BindingRunState::Active {
                        core_run_id: run_id.clone(),
                    };
                    cancellation_requested
                };
                if cancellation_requested {
                    let _ = self.shared.handle.cancel(run_id.clone());
                }
            }
            AgentEvent::TurnStarted { run_id, .. }
                if lock_unpoisoned(&self.core_run_id).as_ref() != Some(run_id) =>
            {
                return Box::pin(async {});
            }
            _ if lock_unpoisoned(&self.core_run_id).is_none() => {
                return Box::pin(async {});
            }
            _ => {}
        }

        if matches!(event, AgentEvent::RunFinished { .. }) {
            self.terminal_delivered.store(true, Ordering::Release);
        }
        deliver_agent_event(
            Arc::clone(&self.shared),
            self.external_run_id,
            Arc::clone(&self.destination),
            event,
        )
    }
}

enum PendingEventDestination {
    Callback(RawEventCallback),
    Polled {
        sender: tokio_mpsc::Sender<String>,
        queue: Arc<RunEventQueue>,
    },
}

async fn drive_run(shared: Arc<AgentBindingShared>, external_run_id: u64, mut run: TokioAgentRun) {
    while run.next_event().await.is_some() {}
    let _ = run.outcome().await;
    let mut state = lock_unpoisoned(&shared.state);
    state.active.remove(&external_run_id);
    shared.idle.notify_all();
}

fn deliver_agent_event(
    shared: Arc<AgentBindingShared>,
    external_run_id: u64,
    destination: Arc<EventDestination>,
    event: AgentEvent,
) -> SendBoxFuture<'static, ()> {
    let sequence = next_event_sequence(&shared);
    let envelope = event_envelope_json(external_run_id, sequence, &event).unwrap_or_else(|error| {
        binding_error_envelope(external_run_id, sequence, &error.to_string())
    });
    Box::pin(async move {
        deliver_envelope(destination.as_ref(), envelope).await;
    })
}

async fn deliver_envelope(destination: &EventDestination, envelope: String) {
    match destination {
        EventDestination::Polled(sender) => {
            let _ = sender.send(envelope).await;
        }
        EventDestination::Callback(callback) => {
            if let Ok(envelope) = std::ffi::CString::new(envelope) {
                unsafe {
                    (callback.callback)(
                        envelope.as_ptr(),
                        callback.user_data as *mut std::ffi::c_void,
                    );
                }
            }
        }
    }
}

fn next_event_sequence(shared: &AgentBindingShared) -> u64 {
    shared.next_sequence.fetch_add(1, Ordering::AcqRel)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventEnvelope<'a> {
    schema_version: u32,
    sequence: u64,
    run_id: String,
    #[serde(rename = "type")]
    event_type: &'a str,
    data: Value,
}

fn event_envelope_json(
    external_run_id: u64,
    sequence: u64,
    event: &AgentEvent,
) -> Result<String, PiFfiError> {
    let mut event = serde_json::to_value(event).map_err(|error| PiFfiError::BindingInvariant {
        message: format!("failed to serialize agent event: {error}"),
    })?;
    let object = event
        .as_object_mut()
        .ok_or_else(|| PiFfiError::BindingInvariant {
            message: "serialized agent event was not an object".into(),
        })?;
    let mut event_type = object
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .ok_or_else(|| PiFfiError::BindingInvariant {
            message: "serialized agent event lacked a type".into(),
        })?;
    object.remove("run_id");
    if event_type == "assistant_update"
        && let Some(Value::Object(mut assistant)) = object.remove("event")
    {
        let assistant_type = assistant
            .remove("type")
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| PiFfiError::BindingInvariant {
                message: "serialized assistant event lacked a type".into(),
            })?;
        event_type = format!("assistant_{assistant_type}");
        object.extend(assistant);
    }
    let data = wire_schema_json(Value::Object(object.clone()));
    serde_json::to_string(&EventEnvelope {
        schema_version: PI_EVENT_ENVELOPE_SCHEMA_VERSION,
        sequence,
        run_id: external_run_id.to_string(),
        event_type: &event_type,
        data,
    })
    .map_err(|error| PiFfiError::BindingInvariant {
        message: format!("failed to serialize event envelope: {error}"),
    })
}

fn binding_error_envelope(run_id: u64, sequence: u64, message: &str) -> String {
    serde_json::json!({
        "schemaVersion": PI_EVENT_ENVELOPE_SCHEMA_VERSION,
        "sequence": sequence,
        "runId": run_id.to_string(),
        "type": "binding_error",
        "data": {
            "code": "binding_invariant",
            "message": message,
        },
    })
    .to_string()
}

/// Renames binding and canonical schema fields without crossing into JSON
/// owned by tools, applications, or provider extensions.
///
/// `serde_json::Value` is part of several canonical contracts, so a generic
/// recursive key transform is lossy: a tool argument named `inner_key` is
/// data, not a Rust schema field. The containing schema still uses camel case
/// at the FFI boundary, while these explicitly opaque children retain every
/// key exactly as supplied.
fn wire_schema_json(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(wire_schema_json).collect()),
        Value::Object(values) => wire_schema_object(values),
        value => value,
    }
}

fn wire_schema_object(values: Map<String, Value>) -> Value {
    let is_custom_record = values.get("kind").and_then(Value::as_str) == Some("custom");
    let is_tool_output_or_update = values.contains_key("content")
        && values.contains_key("added_tool_names")
        && values.contains_key("terminate");
    let is_tool_result_message = values.get("role").and_then(Value::as_str) == Some("tool_result");

    Value::Object(
        values
            .into_iter()
            .map(|(key, value)| {
                let wire_value = match key.as_str() {
                    // Provider/model tool arguments and JSON Schema documents
                    // are open JSON, not fields in the FFI schema.
                    "arguments" | "parameters" | "metadata" => value,

                    // Tool execution updates and outputs carry an unversioned
                    // RawValue. Canonical tool-result messages wrap the same
                    // value in VersionedExtension, whose wrapper is schema.
                    "details" if is_tool_output_or_update => value,
                    "details" if is_tool_result_message => wire_versioned_extension(value),

                    // Application-defined custom records own their payload.
                    "payload" if is_custom_record => value,

                    // Extension IDs are open map keys. Each map value has a
                    // canonical versioned wrapper around owner-defined JSON.
                    "extensions" => wire_extension_map(value),

                    _ => wire_schema_json(value),
                };
                (snake_to_camel(&key), wire_value)
            })
            .collect(),
    )
}

fn wire_extension_map(value: Value) -> Value {
    match value {
        Value::Object(extensions) => Value::Object(
            extensions
                .into_iter()
                .map(|(extension_id, extension)| {
                    (extension_id, wire_versioned_extension(extension))
                })
                .collect(),
        ),
        value => value,
    }
}

fn wire_versioned_extension(value: Value) -> Value {
    match value {
        Value::Object(extension) => Value::Object(
            extension
                .into_iter()
                .map(|(key, value)| {
                    let value = if key == "value" {
                        value
                    } else {
                        wire_schema_json(value)
                    };
                    (snake_to_camel(&key), value)
                })
                .collect(),
        ),
        value => value,
    }
}

fn snake_to_camel(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut uppercase = false;
    for character in value.chars() {
        if character == '_' {
            uppercase = true;
        } else if uppercase {
            output.extend(character.to_uppercase());
            uppercase = false;
        } else {
            output.push(character);
        }
    }
    output
}

struct BindingRuntime {
    handle: RuntimeHandle,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}

impl BindingRuntime {
    fn new() -> Result<Arc<Self>, PiFfiError> {
        let (handle_sender, handle_receiver) = mpsc::sync_channel(1);
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let thread = thread::Builder::new()
            .name("pi-ffi-runtime".into())
            .spawn(move || {
                let runtime = RuntimeBuilder::new_multi_thread()
                    .worker_threads(1)
                    .thread_name("pi-ffi-worker")
                    .build();
                match runtime {
                    Ok(runtime) => {
                        if handle_sender.send(Ok(runtime.handle().clone())).is_ok() {
                            runtime.block_on(async {
                                let _ = shutdown_receiver.await;
                            });
                        }
                    }
                    Err(error) => {
                        let _ = handle_sender.send(Err(error.to_string()));
                    }
                }
            })
            .map_err(|error| PiFfiError::Thread {
                message: format!("failed to start binding runtime thread: {error}"),
            })?;
        let handle = handle_receiver
            .recv()
            .map_err(|_| PiFfiError::Runtime {
                message: "binding runtime exited during startup".into(),
            })?
            .map_err(|message| PiFfiError::Runtime { message })?;
        Ok(Arc::new(Self {
            handle,
            shutdown: Mutex::new(Some(shutdown)),
            thread: Mutex::new(Some(thread)),
        }))
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.handle.spawn(future);
    }

    fn block_on<F>(&self, future: F) -> Result<F::Output, PiFfiError>
    where
        F: Future,
    {
        catch_unwind(AssertUnwindSafe(|| self.handle.block_on(future))).map_err(|_| {
            PiFfiError::Runtime {
                message: "binding runtime could not synchronously drive the operation".into(),
            }
        })
    }
}

impl Drop for BindingRuntime {
    fn drop(&mut self) {
        if let Some(shutdown) = lock_unpoisoned(&self.shutdown).take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = lock_unpoisoned(&self.thread).take() {
            let _ = thread.join();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelsConfig {
    schema_version: u32,
    #[serde(default)]
    runtime: RuntimeConfig,
    #[serde(default)]
    auth_providers: Vec<ScriptedAuthProviderConfig>,
}

#[derive(Default, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RuntimeConfig {
    #[default]
    Models,
    Scripted {
        #[serde(default)]
        responses: Vec<ScriptedResponseConfig>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ScriptedResponseConfig {
    Text { text: String },
    ToolCall { name: String, arguments: Value },
    Failure { code: String, message: String },
    Cancellation { code: String, message: String },
}

impl ScriptedResponseConfig {
    fn into_response(self) -> ScriptedResponse {
        match self {
            Self::Text { text } => text_response(text),
            Self::ToolCall { name, arguments } => tool_call_response(name, arguments),
            Self::Failure { code, message } => ScriptedResponse::failure(PublicError {
                code,
                message,
                retryable: false,
                provider_code: None,
                status: None,
                request_id: None,
            }),
            Self::Cancellation { code, message } => ScriptedResponse::cancellation(
                CancellationReason::new(format!("{code}: {message}")),
            ),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptedAuthProviderConfig {
    provider_id: String,
    #[serde(default)]
    device_code: Option<ScriptedDeviceCodeConfig>,
    #[serde(default)]
    callback_manual: Option<ScriptedCallbackManualConfig>,
}

impl ScriptedAuthProviderConfig {
    fn into_flow(self) -> Result<ScriptedAuthFlow, PiFfiError> {
        match (self.device_code, self.callback_manual) {
            (Some(config), None) => Ok(ScriptedAuthFlow::DeviceCode(config)),
            (None, Some(config)) => Ok(ScriptedAuthFlow::CallbackManual(config)),
            (Some(_), Some(_)) => Err(PiFfiError::InvalidJson {
                message: format!(
                    "scripted auth provider {} must configure exactly one auth flow",
                    self.provider_id
                ),
            }),
            (None, None) => Err(PiFfiError::InvalidJson {
                message: format!(
                    "scripted auth provider {} has no deviceCode or callbackManual flow",
                    self.provider_id
                ),
            }),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptedDeviceCodeConfig {
    challenge_id: String,
    user_code: String,
    verification_uri: url::Url,
    #[serde(default = "default_device_interval_seconds")]
    interval_seconds: u64,
    #[serde(default = "default_device_expiry_seconds")]
    expires_in_seconds: u64,
    #[serde(default)]
    pending_polls: u32,
    access_token: String,
    refresh_token: String,
    expires_at: i64,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScriptedCallbackManualConfig {
    challenge_id: String,
    authorization_url: url::Url,
    redirect_scheme: String,
    redirect_path: String,
    expected_state: String,
    authorization_code: String,
    access_token: String,
    refresh_token: String,
    expires_at: i64,
}

fn default_device_interval_seconds() -> u64 {
    5
}

fn default_device_expiry_seconds() -> u64 {
    900
}

enum ScriptedAuthFlow {
    DeviceCode(ScriptedDeviceCodeConfig),
    CallbackManual(ScriptedCallbackManualConfig),
}

struct ScriptedAuthResolver {
    flow: ScriptedAuthFlow,
}

impl AuthResolver for ScriptedAuthResolver {
    fn resolve(
        &self,
        _request: pi_ai::ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            Ok(None)
        })
    }

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<Credential, AuthError>> {
        Box::pin(async move {
            match &self.flow {
                ScriptedAuthFlow::DeviceCode(config) => {
                    login_scripted_device_code(config.clone(), interaction, cancellation).await
                }
                ScriptedAuthFlow::CallbackManual(config) => {
                    login_scripted_callback_manual(config.clone(), interaction, cancellation).await
                }
            }
        })
    }
}

async fn login_scripted_device_code(
    config: ScriptedDeviceCodeConfig,
    interaction: Arc<dyn AuthInteraction>,
    cancellation: CancellationToken,
) -> Result<Credential, AuthError> {
    let credential =
        scripted_oauth_credential(config.access_token, config.refresh_token, config.expires_at);
    interaction.notify(AuthEvent::DeviceCode {
        challenge_id: AuthChallengeId::new(config.challenge_id),
        user_code: config.user_code,
        verification_uri: config.verification_uri,
        interval: Some(Duration::from_secs(config.interval_seconds)),
        expires_in: Some(Duration::from_secs(config.expires_in_seconds)),
    })?;
    let mut options = OAuthDeviceCodePollOptions::new(
        Box::new(ScriptedCredentialPoll {
            pending: config.pending_polls,
            credential: Some(credential),
        }),
        cancellation,
    );
    options.interval = Some(Duration::from_secs(config.interval_seconds));
    options.expires_in = Some(Duration::from_secs(config.expires_in_seconds));
    let credential = poll_oauth_device_code_flow(options).await?;
    interaction.notify(AuthEvent::Progress {
        message: "Device authorization completed".into(),
    })?;
    Ok(Credential::OAuth(credential))
}

async fn login_scripted_callback_manual(
    config: ScriptedCallbackManualConfig,
    interaction: Arc<dyn AuthInteraction>,
    cancellation: CancellationToken,
) -> Result<Credential, AuthError> {
    let challenge_id = AuthChallengeId::new(config.challenge_id.clone());
    let receiver = interaction
        .create_redirect_receiver(
            RedirectReceiverRequest {
                challenge_id: challenge_id.clone(),
                preferred: vec![RedirectStrategy::CustomScheme {
                    scheme: config.redirect_scheme.clone(),
                    path: config.redirect_path.clone(),
                }],
                expected_path: Some(config.redirect_path.clone()),
                success_page: AuthHtmlPage {
                    html: "Authentication completed".into(),
                },
                failure_page: AuthHtmlPage {
                    html: "Authentication failed".into(),
                },
            },
            cancellation.child(),
        )
        .await?;
    interaction.notify(AuthEvent::OpenUrl {
        challenge_id: challenge_id.clone(),
        url: config.authorization_url.clone(),
        instructions: Some(
            "Complete login in the browser or provide the authorization code manually".into(),
        ),
    })?;

    let redirect_config = config.clone();
    let manual_config = config.clone();
    let manual_interaction = Arc::clone(&interaction);
    let manual_challenge_id = challenge_id.clone();
    let _authorization_code = select_first_valid(
        move |candidate_cancellation| async move {
            let arrival = receiver.receive(candidate_cancellation).await?;
            validate_scripted_authorization_input(arrival.url.as_ref(), &redirect_config)
        },
        move |candidate_cancellation| async move {
            let answer = manual_interaction
                .prompt(
                    AuthPrompt::ManualCode {
                        message: "Complete login in the browser, or provide the authorization code or redirect URL"
                            .into(),
                        placeholder: Some(receiver_placeholder(&manual_config)),
                        challenge_id: manual_challenge_id,
                    },
                    candidate_cancellation,
                )
                .await?;
            let AuthAnswer::Text(input) = answer else {
                return Err(AuthError::new(
                    "invalid_auth_response",
                    "manual authorization did not return text",
                ));
            };
            validate_scripted_authorization_input(&input, &manual_config)
        },
        cancellation,
    )
    .await?;

    interaction.notify(AuthEvent::Progress {
        message: "Browser authorization completed".into(),
    })?;
    Ok(Credential::OAuth(scripted_oauth_credential(
        config.access_token,
        config.refresh_token,
        config.expires_at,
    )))
}

fn validate_scripted_authorization_input(
    input: &str,
    config: &ScriptedCallbackManualConfig,
) -> Result<String, AuthError> {
    let parsed = parse_oauth_authorization_input(input);
    if let Some(state) = parsed.state.as_deref() {
        validate_oauth_state(&config.expected_state, state)?;
    }
    let code = parsed
        .code
        .filter(|code| !code.trim().is_empty())
        .ok_or_else(|| {
            AuthError::new(
                "missing_authorization_code",
                "authorization response did not contain a code",
            )
        })?;
    if code != config.authorization_code {
        return Err(AuthError::new(
            "invalid_authorization_code",
            "authorization response contained an invalid code",
        ));
    }
    Ok(code)
}

fn receiver_placeholder(config: &ScriptedCallbackManualConfig) -> String {
    format!(
        "{}://callback{}",
        config.redirect_scheme,
        if config.redirect_path.starts_with('/') {
            config.redirect_path.clone()
        } else {
            format!("/{}", config.redirect_path)
        }
    )
}

fn scripted_oauth_credential(
    access_token: String,
    refresh_token: String,
    expires_at: i64,
) -> OAuthCredential {
    OAuthCredential {
        access: SecretString::new(access_token),
        refresh: SecretString::new(refresh_token),
        expires_at: Timestamp::from_unix_millis(expires_at),
        extra: ProviderOAuthExtra::None,
    }
}

struct ScriptedCredentialPoll {
    pending: u32,
    credential: Option<OAuthCredential>,
}

impl OAuthDeviceCodePoll<OAuthCredential> for ScriptedCredentialPoll {
    fn poll(
        &mut self,
        cancellation: CancellationToken,
    ) -> SendBoxFuture<'_, Result<OAuthDeviceCodePollResult<OAuthCredential>, AuthError>> {
        Box::pin(async move {
            cancellation.check().map_err(|_| AuthError::Cancelled)?;
            if self.pending > 0 {
                self.pending -= 1;
                return Ok(OAuthDeviceCodePollResult::Pending);
            }
            self.credential
                .take()
                .map(OAuthDeviceCodePollResult::Complete)
                .ok_or_else(|| AuthError::new("scripted_device_code", "credential already emitted"))
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AgentConfig {
    schema_version: u32,
    model: ModelRef,
    #[serde(default)]
    system_prompt: String,
    #[serde(default = "default_reasoning")]
    reasoning: ReasoningLevel,
}

fn default_reasoning() -> ReasoningLevel {
    ReasoningLevel::Off
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RunInput {
    Text(String),
    Prompt(RunPrompt),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunPrompt {
    text: String,
    #[serde(default)]
    images: Vec<RunImage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunImage {
    data: String,
    mime_type: String,
}

fn parse_run_input(input_json: &str) -> Result<PromptText, PiFfiError> {
    let input =
        serde_json::from_str::<RunInput>(input_json).map_err(|error| PiFfiError::InvalidJson {
            message: format!("invalid agent input JSON: {error}"),
        })?;
    Ok(match input {
        RunInput::Text(text) => PromptText {
            text,
            images: Vec::new(),
        },
        RunInput::Prompt(prompt) => PromptText {
            text: prompt.text,
            images: prompt
                .images
                .into_iter()
                .map(|image| PromptImage {
                    data: image.data,
                    mime_type: image.mime_type,
                })
                .collect(),
        },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostCapabilitiesConfig {
    #[serde(default)]
    external_browser: bool,
    #[serde(default)]
    loopback_http: bool,
    #[serde(default)]
    custom_url_scheme: bool,
    #[serde(default)]
    universal_links: bool,
    #[serde(default)]
    manual_paste: bool,
    #[serde(default)]
    clipboard: bool,
}

fn parse_host_capabilities(input: &str) -> Result<AuthHostCapabilities, PiFfiError> {
    let value = serde_json::from_str::<HostCapabilitiesConfig>(input).map_err(|error| {
        PiFfiError::InvalidJson {
            message: format!("invalid host capabilities JSON: {error}"),
        }
    })?;
    Ok(AuthHostCapabilities {
        external_browser: value.external_browser,
        loopback_http: value.loopback_http,
        custom_url_scheme: value.custom_url_scheme,
        universal_links: value.universal_links,
        manual_paste: value.manual_paste,
        clipboard: value.clipboard,
    })
}

trait VersionedConfig {
    fn schema_version(&self) -> u32;
}

impl VersionedConfig for ModelsConfig {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

impl VersionedConfig for AgentConfig {
    fn schema_version(&self) -> u32 {
        self.schema_version
    }
}

fn parse_versioned<T>(input: &str, label: &str) -> Result<T, PiFfiError>
where
    T: for<'de> Deserialize<'de> + VersionedConfig,
{
    let value = serde_json::from_str::<T>(input).map_err(|error| PiFfiError::InvalidJson {
        message: format!("invalid {label} JSON: {error}"),
    })?;
    if value.schema_version() != PI_BINDING_CONFIG_SCHEMA_VERSION {
        return Err(PiFfiError::UnsupportedSchema {
            found: value.schema_version(),
        });
    }
    Ok(value)
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{event_envelope_json, wire_schema_json};
    use pi_agent_core::{AgentEvent, AgentRecord, ToolOutput};
    use pi_ai::{
        Message, MessageId, Timestamp, ToolCall, ToolCallId, ToolResultMessage, VersionedExtension,
    };
    use serde_json::{Value, json, value::RawValue};

    fn raw(value: Value) -> Box<RawValue> {
        RawValue::from_string(value.to_string()).expect("fixture is valid raw JSON")
    }

    fn envelope_data(event: &AgentEvent) -> Value {
        let envelope = event_envelope_json(7, 11, event).expect("event envelope serializes");
        serde_json::from_str::<Value>(&envelope).expect("event envelope is valid JSON")["data"]
            .clone()
    }

    /// Architecture v2 part 1 §6 and part 2's closing lossless-boundary
    /// invariant: schema names are camel-cased, but tool/application/extension
    /// JSON remains exactly owner-shaped at the FFI event boundary.
    #[test]
    fn ffi_event_envelope_preserves_opaque_details_custom_payloads_and_extensions() {
        let tool_details = json!({
            "schema_version": "tool-owned",
            "snake_case_detail": {
                "inner_key": "detail-value"
            }
        });
        let tool_event = AgentEvent::ToolExecutionFinished {
            call_id: ToolCallId::new("call-opaque"),
            result: ToolOutput {
                content: Vec::new(),
                details: Some(raw(tool_details.clone())),
                usage: None,
                added_tool_names: vec!["next_tool".into()],
                terminate: false,
            },
            is_error: false,
        };
        let tool_data = envelope_data(&tool_event);
        assert_eq!(tool_data["result"]["details"], tool_details);
        assert_eq!(tool_data["result"]["addedToolNames"], json!(["next_tool"]));

        let custom_payload = json!({
            "schema_version": "application-owned",
            "snake_case_payload": {
                "inner_key": "custom-value"
            }
        });
        let custom_event = AgentEvent::MessageCommitted {
            message: AgentRecord::Custom {
                type_name: "fixture_record".into(),
                payload: raw(custom_payload.clone()),
            },
        };
        let custom_data = envelope_data(&custom_event);
        assert_eq!(custom_data["message"]["payload"], custom_payload);
        assert_eq!(custom_data["message"]["typeName"], "fixture_record");

        let extension_value = json!({
            "schema_version": "extension-owned",
            "snake_case_extension": {
                "inner_key": "extension-value"
            }
        });
        let extension_event = AgentEvent::MessageCommitted {
            message: AgentRecord::Llm(Message::ToolResult(ToolResultMessage {
                id: MessageId::new("tool-result-opaque"),
                tool_call_id: ToolCallId::new("call-opaque"),
                tool_name: "opaque_tool".into(),
                content: Vec::new(),
                details: Some(VersionedExtension {
                    schema_version: 9,
                    value: raw(extension_value.clone()),
                }),
                usage: None,
                added_tool_names: Vec::new(),
                is_error: false,
                timestamp: Timestamp::from_unix_millis(1),
            })),
        };
        let extension_data = envelope_data(&extension_event);
        assert_eq!(extension_data["message"]["details"]["schemaVersion"], 9);
        assert_eq!(
            extension_data["message"]["details"]["value"],
            extension_value
        );

        let extension_map = wire_schema_json(json!({
            "extensions": {
                "provider_extension": {
                    "schema_version": 4,
                    "value": {
                        "schema_version": "extension-map-owned",
                        "snake_case_extension": {
                            "inner_key": "map-value"
                        }
                    }
                }
            }
        }));
        assert_eq!(
            extension_map["extensions"]["provider_extension"]["schemaVersion"],
            4
        );
        assert_eq!(
            extension_map["extensions"]["provider_extension"]["value"],
            json!({
                "schema_version": "extension-map-owned",
                "snake_case_extension": {
                    "inner_key": "map-value"
                }
            })
        );

        let arguments = json!({
            "snake_case_arg": {
                "inner_key": "argument-value"
            }
        });
        let argument_event = AgentEvent::ToolExecutionStarted {
            call: ToolCall {
                id: ToolCallId::new("call-opaque"),
                name: "opaque_tool".into(),
                arguments: arguments.clone(),
            },
        };
        assert_eq!(
            envelope_data(&argument_event)["call"]["arguments"],
            arguments
        );
    }
}
