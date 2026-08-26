# Rust API inventory for Swift bindings

## Scope and method

This is a Rust-side inventory only. It makes no claim about what BoltFFI can
represent, generate, require, or forbid. Those questions belong to a later
documentation-backed phase.

The inventory follows the API an ordinary embedding application uses to build
and run an agent: `pi-agent-core`, the `pi-ai` contracts visible through it,
the app-facing Tokio actor, and the adjacent durable-session traits. The core
crate publicly re-exports every public item in its modules
(`crates/pi-agent-core/src/lib.rs:20`), so visibility alone is broader than the
ordinary embedding surface.

The names below are the current crate names. The planned rename is
`pi-ai` -> `agentprism-ai`, `pi-agent-core` -> `agentprism-core`, and the same
`agentprism-*` convention for related crates; item names are inventoried as
they exist today.

Classification:

- **core**: needed for the normal create/configure/run/observe/control flow.
- **extended**: a real application-facing capability, but optional for a basic
  embedding (custom policy, restoration, catalogs/auth, sessions, and so on).
- **exclude**: a Local-only twin for a Tokio/Swift embedding, a test fixture,
  or a low-level implementation seam that would leak internals rather than the
  normal application API.

Family:

- **shared**: owned data or synchronous API used by both execution families.
- **Send**: thread-safe native family, normally using `Arc`, `SendBoxFuture`,
  or `SendBoxStream`.
- **Local**: single-threaded family, normally using `Rc`, `LocalBoxFuture`, or
  `LocalBoxStream`.
- **Tokio/Send**: Tokio actor facade over the Send `Agent` family.

## Observed ordinary-consumer flow

The existing binding implementation shows the actual assembly order. It
builds `Models`, obtains an `Arc<dyn ModelRuntime>` from either `Models` or
`ScriptedRuntime`, constructs `Agent`, and wraps it in `TokioAgentHandle`
(`bindings/pi-ffi/src/lib.rs:256`, `bindings/pi-ffi/src/lib.rs:272`,
`bindings/pi-ffi/src/lib.rs:201`, `bindings/pi-ffi/src/lib.rs:212`). This is the
same shape exercised directly by the Tokio tests: create state and records,
implement or inject `ModelRuntime`, construct `Agent`, create the handle, start
a run, await its outcome, inspect a snapshot, and shut down
(`crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:22`,
`crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:58`,
`crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:164`).

The existing C example is evidence for required operations, not the desired
API shape. It creates Models and Agent handles, starts a run with an event
callback, cancels re-entrantly after `run_started`, waits for the terminal
event, drives login challenges, and destroys the handles
(`bindings/pi-ffi/examples/scripted_host.rs:21`,
`bindings/pi-ffi/examples/scripted_host.rs:89`,
`bindings/pi-ffi/examples/scripted_host.rs:100`,
`bindings/pi-ffi/examples/scripted_host.rs:133`,
`bindings/pi-ffi/examples/scripted_host.rs:189`). Its JSON envelopes are an
existing hand-written binding layer and are not substituted for the native
Rust items in this inventory.

## `pi_agent_core`: primary agent surface

### Construction, state, and restoration

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_core::Agent` | `crates/pi-agent-core/src/restore.rs:128` | struct owning trait objects and state | Send | core | The mutable low-level agent state machine. |
| `Agent::new` | `crates/pi-agent-core/src/run.rs:140` | sync constructor | Send | core | Accepts `Arc<dyn ModelRuntime>`, `AgentState`, and `ToolRegistry`. |
| `Agent::restore` | `crates/pi-agent-core/src/restore.rs:153` | sync constructor | Send | extended | Restores a snapshot after model, tool, and custom-kind validation. The restoration test begins at `crates/pi-agent-core/tests/m2_1_state.rs:416` and calls this exact dependency set at `crates/pi-agent-core/tests/m2_1_state.rs:433`. |
| `Agent::{state,runtime,tools,snapshot}` | `crates/pi-agent-core/src/restore.rs:194`, `crates/pi-agent-core/src/restore.rs:199`, `crates/pi-agent-core/src/restore.rs:204`, `crates/pi-agent-core/src/restore.rs:209` | borrowed getters / owned snapshot | Send | core | State observation and persistence; `runtime` and `tools` expose Rust trait-object containers. |
| `Agent::{state_mut,options,options_mut}` | `crates/pi-agent-core/src/run.rs:227`, `crates/pi-agent-core/src/run.rs:233`, `crates/pi-agent-core/src/run.rs:238` | borrowed getters | Send | extended | Idle-only configuration; borrowed mutable returns are part of the native surface. |
| `pi_agent_core::AgentState` / `AgentState::new` | `crates/pi-agent-core/src/state.rs:23`, `crates/pi-agent-core/src/state.rs:38` | data struct / generic constructor | shared | core | System prompt, `ModelRef`, reasoning, and durable transcript. |
| `pi_agent_core::AgentRecord` | `crates/pi-agent-core/src/state.rs:62` | large data enum | shared | core | Either canonical `Message` or custom `{ type_name, Box<RawValue> }`. |
| `AgentRecord::{message_id,custom_type_name}` | `crates/pi-agent-core/src/state.rs:158`, `crates/pi-agent-core/src/state.rs:166` | borrowed getters | shared | extended | Variant inspection helpers. |
| `pi_agent_core::AgentSnapshot` / `AgentSnapshot::new` | `crates/pi-agent-core/src/state.rs:180`, `crates/pi-agent-core/src/state.rs:195` | data struct / constructor | shared | core | Durable state plus next sequence, partial assistant snapshot, and pending tool-call IDs. |
| `pi_agent_core::{AGENT_STATE_SCHEMA_VERSION,AGENT_SNAPSHOT_SCHEMA_VERSION,AGENT_INITIAL_SEQUENCE}` | `crates/pi-agent-core/src/state.rs:10`, `crates/pi-agent-core/src/state.rs:13`, `crates/pi-agent-core/src/state.rs:16` | constants | shared | extended | Persistence/version checks. |
| `pi_agent_core::ModelRefResolver` | `crates/pi-agent-core/src/restore.rs:21` | trait host implements | shared | extended | Synchronous model lookup used only during restore. |
| `pi_agent_core::CustomRecordKindRegistry` | `crates/pi-agent-core/src/restore.rs:36` | trait host implements | shared | extended | Validates custom durable record kinds during restore. |
| `pi_agent_core::CustomRecordKinds` / `{new,register}` | `crates/pi-agent-core/src/restore.rs:52`, `crates/pi-agent-core/src/restore.rs:58`, `crates/pi-agent-core/src/restore.rs:63` | struct / generic method | shared | extended | Built-in registry implementation. |
| `pi_agent_core::migrate_agent_snapshot` | `crates/pi-agent-core/src/restore.rs:87` | sync function | shared | extended | Public snapshot migration/validation entry point. |
| `pi_agent_core::AgentError` | `crates/pi-agent-core/src/error.rs:14` | non-exhaustive error enum | shared | core | Construction, configuration, restoration, and state-machine invariant failures. |

`AgentState::new`, `CustomRecordKinds::register`, and several other constructors
accept `impl Into<...>` rather than one concrete argument type. That is a
generic Rust call shape even where the returned item is concrete.

### Runs, event streams, and lifecycle values

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_core::AgentInput` / `AgentInput::records` | `crates/pi-agent-core/src/run.rs:68`, `crates/pi-agent-core/src/run.rs:75` | data struct / generic constructor | shared | core | Identified record batch for a prompt run. |
| `pi_agent_core::PromptImage` | `crates/pi-agent-core/src/run.rs:84` | data struct | shared | core | Base64 image data plus MIME type. |
| `pi_agent_core::PromptText` | `crates/pi-agent-core/src/run.rs:93` | data struct | shared | core | Text plus ordered images convenience input. |
| `Agent::run` | `crates/pi-agent-core/src/run.rs:283` | stream-returning fn | Send | core | Returns borrowed `SendBoxStream<'a, AgentEvent>`. |
| `Agent::prompt_text` | `crates/pi-agent-core/src/run.rs:292` | stream-returning fn | Send | core | Same borrowed stream, with text/image record construction. |
| `Agent::prompt_records` | `crates/pi-agent-core/src/run.rs:302` | generic stream-returning fn | Send | core | Accepts `impl IntoIterator<Item = AgentRecord>`. |
| `Agent::continue_run` | `crates/pi-agent-core/src/run.rs:312` | fallible stream-returning fn | Send | core | Returns `Result<SendBoxStream<'a, AgentEvent>, AgentError>`. |
| `Agent::retry_last_turn` | `crates/pi-agent-core/src/run.rs:336` | fallible stream-returning fn | Send | core | Same borrowed stream family. |
| `Agent::{reset_transcript,reset_all}` | `crates/pi-agent-core/src/run.rs:350`, `crates/pi-agent-core/src/run.rs:363` | sync methods | Send | core | Idle-only reset operations. |
| `pi_agent_core::AgentPhase` | `crates/pi-agent-core/src/run.rs:30` | data enum | shared | extended | Detailed transient state-machine phase observation. |
| `Agent::{active_run_id,phase,last_error}` | `crates/pi-agent-core/src/run.rs:212`, `crates/pi-agent-core/src/run.rs:217`, `crates/pi-agent-core/src/run.rs:222` | borrowed/sync getters | Send | extended | Low-level live run diagnostics. |
| `pi_agent_core::MessageRole` | `crates/pi-agent-core/src/events.rs:15` | data enum | shared | core | Role in message lifecycle events. |
| `pi_agent_core::TurnOutcome` | `crates/pi-agent-core/src/events.rs:31` | data struct | shared | core | Committed message IDs, finish reason, usage, and cost for a turn. |
| `pi_agent_core::RunOutcome` | `crates/pi-agent-core/src/events.rs:51` | large data enum | shared | core | Completed, failed, or cancelled terminal result. |
| `pi_agent_core::AgentEvent` | `crates/pi-agent-core/src/events.rs:81` | non-exhaustive large data enum | shared | core | Eleven lifecycle variants, including nested `AssistantEvent`, records, calls, updates, outputs, and outcomes. |
| `pi_agent_core::AgentEventEnvelope` | `crates/pi-agent-core/src/events.rs:329` | data struct | shared | extended | Sequenced persistence/FFI envelope around `AgentEvent`; the bare and Tokio run APIs emit `AgentEvent`, not this wrapper. |

The low-level stream borrows `&mut Agent` for its lifetime. The Tokio facade
below exists to move ownership into one task and expose a cloneable handle.

### Concurrent control

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_core::AgentControl` / `Agent::control` | `crates/pi-agent-core/src/control.rs:107`, `crates/pi-agent-core/src/run.rs:172` | cloneable capability / getter | Send | core | Concurrent steering, follow-up, cancellation, queue configuration, and clearing. |
| `AgentControl::{steer,follow_up}` | `crates/pi-agent-core/src/control.rs:154`, `crates/pi-agent-core/src/control.rs:159` | async fns | Send | core | Accept `AgentRecord`, return `QueueReceipt` or `ControlError`. |
| `AgentControl::cancel` | `crates/pi-agent-core/src/control.rs:164` | sync fn | Send | core | Cancels only a matching `RunId`. |
| `AgentControl::{clear_steering,clear_follow_up,clear_all}` | `crates/pi-agent-core/src/control.rs:180`, `crates/pi-agent-core/src/control.rs:188`, `crates/pi-agent-core/src/control.rs:196` | sync fns | Send | extended | Optional queue maintenance. |
| `AgentControl::{set_steering_mode,steering_mode,set_follow_up_mode,follow_up_mode}` | `crates/pi-agent-core/src/control.rs:205`, `crates/pi-agent-core/src/control.rs:210`, `crates/pi-agent-core/src/control.rs:215`, `crates/pi-agent-core/src/control.rs:220` | sync setters/getters | Send | extended | Optional independent queue-drain configuration. |
| `Agent::{set_steering_mode,steering_mode,set_follow_up_mode,follow_up_mode,clear_steering_queue,clear_follow_up_queue,clear_all_queues}` | `crates/pi-agent-core/src/run.rs:177`, `crates/pi-agent-core/src/run.rs:182`, `crates/pi-agent-core/src/run.rs:187`, `crates/pi-agent-core/src/run.rs:192`, `crates/pi-agent-core/src/run.rs:197`, `crates/pi-agent-core/src/run.rs:202`, `crates/pi-agent-core/src/run.rs:207` | sync convenience methods | Send | extended | Delegates to `AgentControl`. |
| `pi_agent_core::{QueueSequence,QueueKind,QueueReceipt}` | `crates/pi-agent-core/src/control.rs:19`, `crates/pi-agent-core/src/control.rs:27`, `crates/pi-agent-core/src/control.rs:58` | newtype/enum/data struct | shared | core | Direct result graph of the ordinary `steer` and `follow_up` control operations. |
| `pi_agent_core::QueueDrainMode` | `crates/pi-agent-core/src/control.rs:37` | data enum | shared | extended | Direct input/output of the optional queue-drain configuration methods above. |
| `pi_agent_core::QueueCommand` | `crates/pi-agent-core/src/control.rs:47` | data struct | shared | exclude | Public because the module is glob-re-exported, but ordinary callers neither pass nor receive it: private `AgentControl::enqueue` creates it and crate-private drains consume it (`crates/pi-agent-core/src/control.rs:224`, `crates/pi-agent-core/src/control.rs:294`). Exposing it would leak the queue implementation. |
| `pi_agent_core::ControlError` | `crates/pi-agent-core/src/control.rs:68` | non-exhaustive error enum | shared | core | Closed/full/unknown-run/overflow control failures. |
| `pi_agent_core::DEFAULT_QUEUE_CAPACITY` | `crates/pi-agent-core/src/control.rs:14` | constant | shared | extended | Bare-agent bounded queue default. |

### Tools and host callbacks

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::ToolSpec` | `crates/pi-ai/src/messages.rs:311` | data struct | shared | core | Model-facing name, description, JSON Schema, and optional constrained-sampling contract. |
| `pi_agent_core::Tool` | `crates/pi-agent-core/src/tools.rs:201` | trait host implements | Send | core | `spec`, scheduling mode, and async `execute`; crosses into the library as `Arc<dyn Tool>`. A concrete implementation is exercised at `crates/pi-agent-core/tests/m2_3_tools.rs:60`. |
| `pi_agent_core::ToolCallContext` | `crates/pi-agent-core/src/tools.rs:32` | data struct | shared | core | Assistant ID, canonical call, and transient normalized JSON arguments. |
| `pi_agent_core::ToolOutput` / `ToolOutput::new` | `crates/pi-agent-core/src/tools.rs:47`, `crates/pi-agent-core/src/tools.rs:72` | data struct / constructor | shared | core | Final content, optional raw JSON details and usage, added tools, termination hint. |
| `pi_agent_core::ToolUpdate` | `crates/pi-agent-core/src/tools.rs:89` | data struct | shared | core | Partial result with the same broad shape as `ToolOutput`. |
| `pi_agent_core::ToolError` / `ToolError::new` | `crates/pi-agent-core/src/tools.rs:134`, `crates/pi-agent-core/src/tools.rs:143` | error struct / generic constructor | shared | core | Host tool failure. |
| `pi_agent_core::ToolUpdateError` / `ToolUpdateError::new` | `crates/pi-agent-core/src/tools.rs:161`, `crates/pi-agent-core/src/tools.rs:168` | error struct / generic constructor | shared | core | Failure to accept a transient update. |
| `pi_agent_core::ToolUpdateSink` | `crates/pi-agent-core/src/tools.rs:184` | trait library implements | Send | core | Library callback capability passed to host `Tool::execute` as `Arc<dyn ToolUpdateSink>`. |
| `pi_agent_core::ToolArgumentPreparer` | `crates/pi-agent-core/src/tools.rs:242` | trait host implements | Send | extended | Optional pre-validation JSON transformation. |
| `pi_agent_core::ToolExecutionMode` | `crates/pi-agent-core/src/tools.rs:21` | data enum | shared | core | Parallel or sequential scheduling requirement. |
| `pi_agent_core::TypedTool<I,F>` | `crates/pi-agent-core/src/tools.rs:277` | generic struct | Send | core | Typed host-tool adapter over input `I` and closure `F`. |
| `TypedTool::{new,from_spec,with_execution_mode}` | `crates/pi-agent-core/src/tools.rs:290`, `crates/pi-agent-core/src/tools.rs:312`, `crates/pi-agent-core/src/tools.rs:326` | generic constructors/builder | Send | core | `new` derives JSON Schema from `I`; closure `F` returns `SendBoxFuture<'static, Result<ToolOutput, ToolError>>`. Real usage is at `crates/pi-agent-core/tests/m2_3_tools.rs:607`. |
| `pi_agent_core::ToolRegistry` | `crates/pi-agent-core/src/tools.rs:491` | trait-object registry struct | Send | core | Heterogeneous `Arc<dyn Tool>` registry passed into `Agent`. |
| `ToolRegistry::{new,register,register_with_argument_preparer,get,is_empty,len}` | `crates/pi-agent-core/src/tools.rs:497`, `crates/pi-agent-core/src/tools.rs:502`, `crates/pi-agent-core/src/tools.rs:507`, `crates/pi-agent-core/src/tools.rs:538`, `crates/pi-agent-core/src/tools.rs:543`, `crates/pi-agent-core/src/tools.rs:548` | constructor/methods | Send | core | Tool binding and inspection. |

The optional constrained-sampling branch of `ToolSpec` is also part of its
exact public shape:

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::{ConstrainedSampling,ConstrainedSamplingConfig,JsonSchemaStrictMode,GrammarFormat,GrammarVariants}` | `crates/pi-ai/src/messages.rs:334`, `crates/pi-ai/src/messages.rs:379`, `crates/pi-ai/src/messages.rs:397`, `crates/pi-ai/src/messages.rs:407`, `crates/pi-ai/src/messages.rs:418` | data enums / ordered-map alias | shared | extended | Optional `ToolSpec::constrained_sampling` graph; basic tools normally leave it unset. |

### Optional policies and low-level scheduler

| Item path | Source | Kind | Family | Relevance | Reason |
|---|---|---|---|---|---|
| `ContextPolicy`, `MessageProjector`, `ToolPolicy`, `TurnPolicy` | `crates/pi-agent-core/src/policy.rs:116`, `crates/pi-agent-core/src/policy.rs:183`, `crates/pi-agent-core/src/policy.rs:335`, `crates/pi-agent-core/src/policy.rs:482` | async traits host implements | Send | extended | Agent customization seams set through `Agent::{set_context_policy,set_message_projector,set_tool_policy,set_turn_policy}` at `crates/pi-agent-core/src/run.rs:244`, `crates/pi-agent-core/src/run.rs:251`, `crates/pi-agent-core/src/run.rs:275`, and `crates/pi-agent-core/src/run.rs:261`. |
| `Agent::set_tool_execution_mode` | `crates/pi-agent-core/src/run.rs:268` | sync setter | Send | extended | Selects the default batch scheduling mode while idle. |
| `AgentStateView`, `PreparedAgentRecords`, `PreparedContext`, `ContextError` | `crates/pi-agent-core/src/policy.rs:16`, `crates/pi-agent-core/src/policy.rs:59`, `crates/pi-agent-core/src/policy.rs:74`, `crates/pi-agent-core/src/policy.rs:90` | borrowed/data structs and error enum | shared | extended | Context-policy inputs and outputs. |
| `AgentRunContext<Tools>`, `AgentContext` | `crates/pi-agent-core/src/policy.rs:37`, `crates/pi-agent-core/src/policy.rs:47` | generic struct / concrete alias | Send | extended | Complete run-local policy context, including executable tool registry. |
| `BeforeToolCall<'a,Tools>`, `AfterToolCall<'a,Tools>`, `ToolAuthorization`, `ToolOutputPatch` | `crates/pi-agent-core/src/policy.rs:253`, `crates/pi-agent-core/src/policy.rs:268`, `crates/pi-agent-core/src/policy.rs:238`, `crates/pi-agent-core/src/policy.rs:296` | borrowed generic structs / data | Send | extended | Tool-policy callback graph. |
| `CompletedTurn<'a,Tools>`, `NextTurn<Tools>`, `TurnPolicyError` | `crates/pi-agent-core/src/policy.rs:413`, `crates/pi-agent-core/src/policy.rs:431`, `crates/pi-agent-core/src/policy.rs:459` | borrowed generic structs / error | Send | extended | Turn-policy callback graph. |
| `DefaultContextPolicy`, `DefaultMessageProjector`, `DefaultToolPolicy`, `DefaultTurnPolicy` | `crates/pi-agent-core/src/policy.rs:137`, `crates/pi-agent-core/src/policy.rs:206`, `crates/pi-agent-core/src/policy.rs:374`, `crates/pi-agent-core/src/policy.rs:517` | library implementations | shared | exclude | Agent installs these defaults itself; separately exporting them leaks policy assembly into a basic embedding API. |
| `ToolScheduler`, `ToolBatchRequest<'a,Tools>`, `ToolBatchStreamEvent`, `ToolBatchOutcome`, `ToolCallOutcome`, `PreflightIndex`, `CompletionIndex`, `SourceIndex`, and `ToolExecutionPlan` | `crates/pi-agent-core/src/scheduler.rs:141`, `crates/pi-agent-core/src/scheduler.rs:126`, `crates/pi-agent-core/src/scheduler.rs:89`, `crates/pi-agent-core/src/scheduler.rs:72`, `crates/pi-agent-core/src/scheduler.rs:52`, `crates/pi-agent-core/src/scheduler.rs:28`, `crates/pi-agent-core/src/scheduler.rs:32`, `crates/pi-agent-core/src/scheduler.rs:36`, `crates/pi-agent-core/src/scheduler.rs:40` | low-level scheduler/generic inputs/stream/data | Send/shared | exclude | Public conformance and composition surface below `Agent`; ordinary embedding receives the corresponding `AgentEvent` tool lifecycle instead. |

### Event replay helpers

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_core::CommittedEventReplay` / `{new,apply,state,next_sequence,into_state}` | `crates/pi-agent-core/src/replay.rs:16`, `crates/pi-agent-core/src/replay.rs:24`, `crates/pi-agent-core/src/replay.rs:48`, `crates/pi-agent-core/src/replay.rs:95`, `crates/pi-agent-core/src/replay.rs:100`, `crates/pi-agent-core/src/replay.rs:105` | reducer struct/methods | shared | extended | Rebuilds durable agent state from sequenced event envelopes. |
| `pi_agent_core::replay_committed_events` | `crates/pi-agent-core/src/replay.rs:112` | lifetime-generic function | shared | extended | Batch reducer over borrowed envelopes. |
| `pi_agent_core::committed_record` | `crates/pi-agent-core/src/replay.rs:125` | sync function | shared | extended | Extracts committed records from events. |

### Local family

The Local family is real public Rust API and the restoration test verifies the
Send and Local runtime families in the named test at
`crates/pi-agent-core/tests/m2_1_state.rs:464`; its Local restore call is at
`crates/pi-agent-core/tests/m2_1_state.rs:467` and its Send restore call is at
`crates/pi-agent-core/tests/m2_1_state.rs:477`.
For the Swift design centered on `TokioAgentHandle`, it is classified **exclude**
as an alternate executor family, not as hidden implementation:

| Item paths | Source | Kind | Family | Relevance |
|---|---|---|---|---|
| `LocalAgent` | `crates/pi-agent-core/src/restore.rs:221` | agent struct | Local | exclude |
| `LocalAgent::restore` | `crates/pi-agent-core/src/restore.rs:246` | sync constructor over Local trait objects | Local | exclude |
| `LocalAgent::{state,runtime,tools,snapshot}` | `crates/pi-agent-core/src/restore.rs:287`, `crates/pi-agent-core/src/restore.rs:292`, `crates/pi-agent-core/src/restore.rs:297`, `crates/pi-agent-core/src/restore.rs:302` | borrowed getters / owned snapshot | Local | exclude |
| `LocalAgent::{new,control}` | `crates/pi-agent-core/src/run.rs:470`, `crates/pi-agent-core/src/run.rs:502` | sync constructor / cloneable control getter | Local | exclude |
| `LocalAgent::{set_steering_mode,steering_mode,set_follow_up_mode,follow_up_mode,clear_steering_queue,clear_follow_up_queue,clear_all_queues}` | `crates/pi-agent-core/src/run.rs:507`, `crates/pi-agent-core/src/run.rs:512`, `crates/pi-agent-core/src/run.rs:517`, `crates/pi-agent-core/src/run.rs:522`, `crates/pi-agent-core/src/run.rs:527`, `crates/pi-agent-core/src/run.rs:532`, `crates/pi-agent-core/src/run.rs:537` | sync queue configuration/control methods | Local | exclude |
| `LocalAgent::{active_run_id,phase,last_error,state_mut,options,options_mut}` | `crates/pi-agent-core/src/run.rs:542`, `crates/pi-agent-core/src/run.rs:547`, `crates/pi-agent-core/src/run.rs:552`, `crates/pi-agent-core/src/run.rs:557`, `crates/pi-agent-core/src/run.rs:563`, `crates/pi-agent-core/src/run.rs:568` | borrowed/sync observation and configuration methods | Local | exclude |
| `LocalAgent::{set_tool_execution_mode,set_tool_policy,set_context_policy,set_message_projector,set_turn_policy}` | `crates/pi-agent-core/src/run.rs:574`, `crates/pi-agent-core/src/run.rs:581`, `crates/pi-agent-core/src/run.rs:588`, `crates/pi-agent-core/src/run.rs:598`, `crates/pi-agent-core/src/run.rs:608` | Local trait-object policy setters | Local | exclude |
| `LocalAgent::{run,prompt_text,prompt_records,continue_run,retry_last_turn}` | `crates/pi-agent-core/src/run.rs:615`, `crates/pi-agent-core/src/run.rs:624`, `crates/pi-agent-core/src/run.rs:634`, `crates/pi-agent-core/src/run.rs:643`, `crates/pi-agent-core/src/run.rs:666` | Local borrowed-stream methods, including one iterator-generic method | Local | exclude |
| `LocalAgent::{reset_transcript,reset_all}` | `crates/pi-agent-core/src/run.rs:679`, `crates/pi-agent-core/src/run.rs:691` | fallible sync lifecycle methods | Local | exclude |
| `LocalTool`, `LocalToolUpdateSink`, `LocalToolArgumentPreparer`, `LocalTypedTool<I,F>`, `LocalToolRegistry` | `crates/pi-agent-core/src/tools.rs:220`, `crates/pi-agent-core/src/tools.rs:194`, `crates/pi-agent-core/src/tools.rs:257`, `crates/pi-agent-core/src/tools.rs:382`, `crates/pi-agent-core/src/tools.rs:595` | Local traits/generic adapter/registry | Local | exclude |
| `LocalContextPolicy`, `LocalMessageProjector`, `LocalToolPolicy`, `LocalTurnPolicy`, `LocalAgentContext`, `LocalBeforeToolCall`, `LocalAfterToolCall`, `LocalCompletedTurn`, and `LocalNextTurn` | `crates/pi-agent-core/src/policy.rs:126`, `crates/pi-agent-core/src/policy.rs:192`, `crates/pi-agent-core/src/policy.rs:356`, `crates/pi-agent-core/src/policy.rs:499`, `crates/pi-agent-core/src/policy.rs:50`, `crates/pi-agent-core/src/policy.rs:285`, `crates/pi-agent-core/src/policy.rs:288`, `crates/pi-agent-core/src/policy.rs:452`, `crates/pi-agent-core/src/policy.rs:455` | Local async traits/data aliases | Local | exclude |
| `LocalToolScheduler` and Local scheduler stream | `crates/pi-agent-core/src/scheduler.rs:197`, `crates/pi-agent-core/src/scheduler.rs:234` | low-level scheduler/stream | Local | exclude |

## `pi_agent_runtime_tokio`: natural app-facing handle

### Actor, runs, events, and control

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_runtime_tokio::TokioAgentHandle` | `crates/pi-agent-runtime-tokio/src/lib.rs:158` | cloneable actor handle struct | Tokio/Send | core | Owns a bounded command sender, snapshot/idle watches, direct control, and event capacity. |
| `TokioAgentHandle::{new,spawn,with_capacities}` | `crates/pi-agent-runtime-tokio/src/lib.rs:168`, `crates/pi-agent-runtime-tokio/src/lib.rs:173`, `crates/pi-agent-runtime-tokio/src/lib.rs:178` | sync constructors | Tokio/Send | core | Start the owner task; requires an active Tokio runtime. |
| `TokioAgentHandle::{prompt_text,prompt_text_with_sink,prompt_records}` | `crates/pi-agent-runtime-tokio/src/lib.rs:203`, `crates/pi-agent-runtime-tokio/src/lib.rs:217`, `crates/pi-agent-runtime-tokio/src/lib.rs:230` | async fns | Tokio/Send | core | Accept a run and return `TokioAgentRun`; `prompt_records` has an `impl IntoIterator` input. |
| `TokioAgentHandle::{continue_run,retry_last_turn}` | `crates/pi-agent-runtime-tokio/src/lib.rs:243`, `crates/pi-agent-runtime-tokio/src/lib.rs:249` | async fns | Tokio/Send | core | Start follow-on runs. |
| `TokioAgentHandle::{steer,follow_up,cancel}` | `crates/pi-agent-runtime-tokio/src/lib.rs:255`, `crates/pi-agent-runtime-tokio/src/lib.rs:270`, `crates/pi-agent-runtime-tokio/src/lib.rs:285` | async fns | Tokio/Send | core | Serialized mailbox control. |
| `TokioAgentHandle::cancel_now` | `crates/pi-agent-runtime-tokio/src/lib.rs:301` | sync fn | Tokio/Send | core | Direct re-entrant cancellation intended for foreign callbacks. |
| `TokioAgentHandle::{subscribe,unsubscribe}` | `crates/pi-agent-runtime-tokio/src/lib.rs:306`, `crates/pi-agent-runtime-tokio/src/lib.rs:319` | async fns | Tokio/Send | core | Register/remove acknowledged event sinks. |
| `TokioAgentHandle::{reset_transcript,reset_all}` | `crates/pi-agent-runtime-tokio/src/lib.rs:329`, `crates/pi-agent-runtime-tokio/src/lib.rs:339` | async fns | Tokio/Send | core | Actor-serialized reset. |
| `TokioAgentHandle::{snapshot,latest_snapshot,snapshots}` | `crates/pi-agent-runtime-tokio/src/lib.rs:349`, `crates/pi-agent-runtime-tokio/src/lib.rs:364`, `crates/pi-agent-runtime-tokio/src/lib.rs:369` | async getter / sync getter / receiver-returning fn | Tokio/Send | core (`snapshot`, `latest_snapshot`); extended (`snapshots`) | `snapshots` exposes `tokio::sync::watch::Receiver<AgentSnapshot>` directly. |
| `TokioAgentHandle::{wait_for_idle,shutdown}` | `crates/pi-agent-runtime-tokio/src/lib.rs:374`, `crates/pi-agent-runtime-tokio/src/lib.rs:385` | async fns | Tokio/Send | core | Lifecycle barriers and teardown. Tests exercise all command categories at `crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:442`. |
| `pi_agent_runtime_tokio::TokioAgentRun` | `crates/pi-agent-runtime-tokio/src/lib.rs:126` | run handle struct | Tokio/Send | core | Owns event receiver and completion receiver. |
| `TokioAgentRun::events` | `crates/pi-agent-runtime-tokio/src/lib.rs:133` | borrowed Tokio receiver getter | Tokio/Send | core | Exposes `&mut tokio::sync::mpsc::Receiver<AgentEvent>`; it is not declared as a `futures_core::Stream` wrapper. |
| `TokioAgentRun::next_event` | `crates/pi-agent-runtime-tokio/src/lib.rs:138` | async fn | Tokio/Send | core | Pulls one ordered observational event. |
| `TokioAgentRun::outcome` | `crates/pi-agent-runtime-tokio/src/lib.rs:143` | consuming async fn | Tokio/Send | core | Waits for terminal result and sink barriers. |
| `pi_agent_runtime_tokio::AgentEventSink` | `crates/pi-agent-runtime-tokio/src/lib.rs:45` | trait host implements | Tokio/Send | core | Receives owned `AgentEvent` and `CancellationToken`, returns `SendBoxFuture<'static, ()>`. The current binding implements it at `bindings/pi-ffi/src/lib.rs:617`. |
| `pi_agent_runtime_tokio::EventSinkId` | `crates/pi-agent-runtime-tokio/src/lib.rs:37` | opaque newtype | Tokio/Send | core | Subscription identity; inner integer is private. |
| `pi_agent_runtime_tokio::TokioAgentError` | `crates/pi-agent-runtime-tokio/src/lib.rs:70` | non-exhaustive error enum | Tokio/Send | core | No-runtime, closed, nested Agent error, missing terminal, or snapshot invariant. |
| `DEFAULT_COMMAND_CAPACITY`, `DEFAULT_EVENT_CAPACITY` | `crates/pi-agent-runtime-tokio/src/lib.rs:30`, `crates/pi-agent-runtime-tokio/src/lib.rs:33` | constants | Tokio/Send | extended | Actor defaults. |
| `TokioAgentFileSystem`, `TokioClock`, `TokioTemporaryArtifactStore`, `TokioProcessSpawner`, `TokioAgentEnvironment` | `crates/pi-agent-runtime-tokio/src/environment.rs:38`, `crates/pi-agent-runtime-tokio/src/environment.rs:357`, `crates/pi-agent-runtime-tokio/src/environment.rs:385`, `crates/pi-agent-runtime-tokio/src/environment.rs:450`, `crates/pi-agent-runtime-tokio/src/environment.rs:868` | library implementation structs | Tokio/Send | exclude | Coding-harness filesystem/clock/process/environment capabilities rather than the ordinary agent handle; they need a separate inventory when `pi-coding-agent` enters scope. |

Those environment items are re-exported by the runtime crate at
`crates/pi-agent-runtime-tokio/src/lib.rs:11`.

## `pi_ai`: contracts visible through the agent

### Runtime, futures, and assistant streams

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::ModelRequest` | `crates/pi-ai/src/runtime.rs:13` | data struct | shared | core | `ModelRef`, canonical `Context`, and `SimpleGenerationOptions`. |
| `pi_ai::ModelRuntime` | `crates/pi-ai/src/runtime.rs:87` | object-safe async trait, normally library implemented | Send | core | One `stream` method returns `SendBoxFuture<Result<AssistantStream, RequestStartError>>`; `Agent::new` consumes `Arc<dyn ModelRuntime>`. Tests also show native Rust hosts can implement it at `crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:58`. |
| `pi_ai::RequestStartErrorKind` / `RequestStartError` | `crates/pi-ai/src/runtime.rs:25`, `crates/pi-ai/src/runtime.rs:48` | non-exhaustive enum / error struct | shared | core | Failure before an assistant stream exists. |
| `RequestStartError::{new,with_model}` | `crates/pi-ai/src/runtime.rs:61`, `crates/pi-ai/src/runtime.rs:71` | generic constructor/builder | shared | extended | Used by custom runtimes. |
| `pi_ai::SendBoxFuture<'a,T>` | `crates/pi-ai/src/async_types.rs:11` | generic boxed-future alias | Send | core | Return carrier for runtime, tool, sink, policy, and session traits. |
| `pi_ai::SendBoxStream<'a,T>` | `crates/pi-ai/src/async_types.rs:17` | generic boxed-stream alias | Send | core | Carrier returned by bare Agent runs and low-level schedulers. |
| `pi_ai::AssistantStream` / `{new,from_boxed,is_terminated}` | `crates/pi-ai/src/streaming.rs:1900`, `crates/pi-ai/src/streaming.rs:1907`, `crates/pi-ai/src/streaming.rs:1915`, `crates/pi-ai/src/streaming.rs:1920` | owned stream wrapper / generic constructor | Send | core | Owns a `SendBoxStream<'static, AssistantEvent>` and fuses at terminal/EOF. |
| `pi_ai::LocalModelRuntime` | `crates/pi-ai/src/runtime.rs:100` | object-safe async trait | Local | exclude | Alternate single-threaded runtime family. |
| `pi_ai::{LocalBoxFuture,LocalBoxStream}` | `crates/pi-ai/src/async_types.rs:8`, `crates/pi-ai/src/async_types.rs:14` | generic aliases | Local | exclude | Local carrier family. |
| `pi_ai::LocalAssistantStream` | `crates/pi-ai/src/streaming.rs:1965` | owned Local stream wrapper | Local | exclude | Local counterpart to `AssistantStream`. |

There are two **executor/carrier** families: `SendBoxStream` and
`LocalBoxStream` (`crates/pi-ai/src/async_types.rs:17`,
`crates/pi-ai/src/async_types.rs:14`). They must not be
confused with the two **semantic event** families. Model execution emits
`AssistantEvent` through owned, fused, `'static`
`AssistantStream`/`LocalAssistantStream` wrappers
(`crates/pi-ai/src/streaming.rs:1900`,
`crates/pi-ai/src/streaming.rs:1965`). Agent execution emits the broader
`AgentEvent` lifecycle through a boxed stream borrowing the bare agent
(`crates/pi-agent-core/src/run.rs:283`,
`crates/pi-agent-core/src/run.rs:615`), or through the Tokio actor's bounded
receiver and `next_event` (`crates/pi-agent-runtime-tokio/src/lib.rs:126`).
`HttpBody` is a separate non-event use of `SendBoxStream`, carrying byte chunks
or `TransportError` at the provider transport seam
(`crates/pi-ai/src/middleware.rs:69`); it is extended provider plumbing rather
than either public event protocol.

### `Models` construction and execution bridge

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::Models` | `crates/pi-ai/src/models.rs:49` | cloneable control-plane struct | Send | core | Provider/model/auth/catalog owner; implements `ModelRuntime` at `crates/pi-ai/src/models.rs:1399` and `DeferredModelRuntime` at `crates/pi-ai/src/models.rs:1409`. |
| `Models::default` | `crates/pi-ai/src/models.rs:99` | default constructor | Send | extended | Constructs an empty valid registry. |
| `Models::builder` / `ModelsBuilder` | `crates/pi-ai/src/models.rs:109`, `crates/pi-ai/src/models.rs:1445` | constructor / builder struct holding trait objects | Send | core | Normal construction entry. |
| `ModelsBuilder::{provider,build}` | `crates/pi-ai/src/models.rs:1501`, `crates/pi-ai/src/models.rs:1541` | builder methods | Send | core | Register provider compositions and validate/build Models. The current binding uses this route at `bindings/pi-ffi/src/lib.rs:256`. |
| `ModelsBuilder::{credential_store,auth_context,models_store,model_override_store}` | `crates/pi-ai/src/models.rs:1476`, `crates/pi-ai/src/models.rs:1482`, `crates/pi-ai/src/models.rs:1488`, `crates/pi-ai/src/models.rs:1494` | trait-object builder methods | Send | extended | Accept `Arc<dyn CredentialStore>`, `Arc<dyn AuthContext>`, `Arc<dyn ModelsStore>`, and `Arc<dyn ModelOverrideStore>` respectively. |
| `ModelsBuilder::{header_transform,payload_transform,erased_payload_transform,response_observer,attempt_middleware}` | `crates/pi-ai/src/models.rs:1507`, `crates/pi-ai/src/models.rs:1513`, `crates/pi-ai/src/models.rs:1523`, `crates/pi-ai/src/models.rs:1529`, `crates/pi-ai/src/models.rs:1535` | trait-object/generic builder methods | Send | extended | Accept the five callback families enumerated below; `payload_transform<A>` is generic over `ApiFamily`. |
| `pi_ai::ProviderRegistration` / `ProviderRegistration::builder` | `crates/pi-ai/src/provider.rs:2320`, `crates/pi-ai/src/provider.rs:2355` | trait-object aggregate / generic constructor | Send | core | Complete provider composition accepted by `ModelsBuilder::provider`. Its public fields are descriptor, auth resolver, catalog, model filter, API dispatch map, retry policy, and retry classifier. |
| `pi_ai::ProviderRegistrationBuilder` / `{new,display_name,base_url,headers,auth,catalog,catalog_source,models,filter_models,api,retry_policy,retry_classifier,build}` | `crates/pi-ai/src/provider.rs:2380`, `crates/pi-ai/src/provider.rs:2394`, `crates/pi-ai/src/provider.rs:2409`, `crates/pi-ai/src/provider.rs:2415`, `crates/pi-ai/src/provider.rs:2421`, `crates/pi-ai/src/provider.rs:2427`, `crates/pi-ai/src/provider.rs:2433`, `crates/pi-ai/src/provider.rs:2442`, `crates/pi-ai/src/provider.rs:2449`, `crates/pi-ai/src/provider.rs:2458`, `crates/pi-ai/src/provider.rs:2464`, `crates/pi-ai/src/provider.rs:2470`, `crates/pi-ai/src/provider.rs:2476`, `crates/pi-ai/src/provider.rs:2482` | builder with generic inputs and many trait objects | Send | extended | Custom provider composition. |
| `pi_ai::ProviderRegistrationError` | `crates/pi-ai/src/provider.rs:2529` | error enum | shared | core | Build/set-provider validation error. |
| `Models::{stream_simple,stream_simple_with_auth}` | `crates/pi-ai/src/models.rs:762`, `crates/pi-ai/src/models.rs:773` | future-returning fns | Send | core (`stream_simple`); extended (`with_auth`) | Direct model call; `ModelRuntime::stream` delegates to `stream_simple`. |
| `Models::{stream_api,stream_api_with_request_options}` | `crates/pi-ai/src/models.rs:785`, `crates/pi-ai/src/models.rs:803` | generic future-returning fns | Send | extended | Generic over `A: ApiFamily`; accept `A::FullOptions`, with the second method also accepting `ApiRequestOptions`. |
| `Models::{fetch_deferred,fetch_deferred_with_auth,cancel_deferred,cancel_deferred_with_auth}` | `crates/pi-ai/src/models.rs:827`, `crates/pi-ai/src/models.rs:846`, `crates/pi-ai/src/models.rs:884`, `crates/pi-ai/src/models.rs:902` | future-returning fns | Send | extended | Redeem/cancel durable deferred responses. |
| `Models::{providers,provider,models,filter_models,model}` | `crates/pi-ai/src/models.rs:115`, `crates/pi-ai/src/models.rs:125`, `crates/pi-ai/src/models.rs:132`, `crates/pi-ai/src/models.rs:150`, `crates/pi-ai/src/models.rs:248` | snapshot/query methods | Send | extended | Synchronous provider/model catalog inspection. |
| `Models::{check_auth,get_available,credential_store,credential_info,resolve_auth,login,logout}` | `crates/pi-ai/src/models.rs:167`, `crates/pi-ai/src/models.rs:197`, `crates/pi-ai/src/models.rs:259`, `crates/pi-ai/src/models.rs:264`, `crates/pi-ai/src/models.rs:280`, `crates/pi-ai/src/models.rs:313`, `crates/pi-ai/src/models.rs:350` | trait-object getter and future-returning methods | Send | extended | Direct auth/availability control plane; its complete signature graph is enumerated next. `Models::login` is called by the existing binding at `bindings/pi-ffi/src/lib.rs:233`. |
| `Models::{catalog_snapshot,catalog_layers,set_provider,remove_provider,clear_providers,refresh_host_overrides,set_runtime_overrides,clear_runtime_overrides,refresh}` | `crates/pi-ai/src/models.rs:370`, `crates/pi-ai/src/models.rs:391`, `crates/pi-ai/src/models.rs:399`, `crates/pi-ai/src/models.rs:456`, `crates/pi-ai/src/models.rs:478`, `crates/pi-ai/src/models.rs:493`, `crates/pi-ai/src/models.rs:514`, `crates/pi-ai/src/models.rs:534`, `crates/pi-ai/src/models.rs:544` | sync and async control-plane methods | Send | extended | Live provider/catalog mutation and refresh. |
| `pi_ai::{ProviderSnapshot,ModelSnapshot}` | `crates/pi-ai/src/models.rs:42`, `crates/pi-ai/src/models.rs:45` | `Arc` slice aliases | Send | extended | Immutable catalog snapshots. |
| `pi_ai::LocalModels`, `LocalModelsBuilder`, `LocalProviderRegistration`, `LocalProviderRegistrationBuilder` | `crates/pi-ai/src/models.rs:1610`, `crates/pi-ai/src/models.rs:2947`, `crates/pi-ai/src/provider.rs:2584`, `crates/pi-ai/src/provider.rs:2643` | Local control plane/builders | Local | exclude | Complete Local twin of the Models/provider surface. |

#### Scripted fixture surface observed at reviewed call sites

These public items are deliberately itemized even though they are fixtures.
Agent tests use them pervasively, and the existing binding selects them from
its fixture configuration (`crates/pi-agent-core/tests/m2_2_run.rs:56`,
`crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:397`,
`bindings/pi-ffi/src/lib.rs:272`). They are **exclude** because they are a
deterministic fake runtime, not the production application surface represented
by `Models`.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::ScriptedRuntime` | `crates/pi-ai/src/scripted.rs:323` | library fixture implementing `ModelRuntime`, `LocalModelRuntime`, and both deferred-runtime traits | Send + Local | exclude | Queue-backed deterministic model fake. `ScriptedRuntime::new` is called by the binding at `bindings/pi-ffi/src/lib.rs:274` and by core/Tokio tests at `crates/pi-agent-core/tests/m2_2_run.rs:58` and `crates/pi-agent-runtime-tokio/tests/m2_2_handle.rs:397`. |
| `ScriptedRuntime::{new,builder,remaining,deferred_fetch_count,cancelled_deferred}` | `crates/pi-ai/src/scripted.rs:346`, `crates/pi-ai/src/scripted.rs:359`, `crates/pi-ai/src/scripted.rs:364`, `crates/pi-ai/src/scripted.rs:369`, `crates/pi-ai/src/scripted.rs:375` | iterator-generic constructor, builder entry, and fixture inspection methods | Send + Local | exclude | Creates and inspects scripted queues and deferred fixture activity. |
| `ScriptedRuntime::{fetch_deferred,cancel_deferred}` | `crates/pi-ai/src/scripted.rs:380`, `crates/pi-ai/src/scripted.rs:408` | future-returning fixture methods | Send | exclude | Direct fixture helpers mirroring the deferred runtime capability. |
| `pi_ai::ScriptedRuntimeBuilder` | `crates/pi-ai/src/scripted.rs:279` | fixture builder struct | shared | exclude | Accumulates scripted responses. |
| `ScriptedRuntimeBuilder::{response,failure,cancellation,scripted_events,deferred,usage,build}` | `crates/pi-ai/src/scripted.rs:285`, `crates/pi-ai/src/scripted.rs:291`, `crates/pi-ai/src/scripted.rs:296`, `crates/pi-ai/src/scripted.rs:301`, `crates/pi-ai/src/scripted.rs:306`, `crates/pi-ai/src/scripted.rs:311`, `crates/pi-ai/src/scripted.rs:316` | generic builder methods | shared | exclude | Exact response/event/deferred fixture assembly; restoration tests use `builder().build()` at `crates/pi-agent-core/tests/m2_1_state.rs:429`. |
| `pi_ai::ScriptedResponse` | `crates/pi-ai/src/scripted.rs:57` | fixture data struct with private variant storage | shared | exclude | One exact or generated response consumed by `ScriptedRuntime`. |
| `ScriptedResponse::{events,completed_events,failure,cancellation,deferred}` | `crates/pi-ai/src/scripted.rs:101`, `crates/pi-ai/src/scripted.rs:110`, `crates/pi-ai/src/scripted.rs:125`, `crates/pi-ai/src/scripted.rs:136`, `crates/pi-ai/src/scripted.rs:149` | iterator-generic/fallible fixture constructors | shared | exclude | Builds exact assistant-event, terminal-error/cancellation, and deferred response fixtures. The binding calls `failure` and `cancellation` at `bindings/pi-ffi/src/lib.rs:1014` and `bindings/pi-ffi/src/lib.rs:1022`. |
| `ScriptedResponse::{with_pending_fetches,with_api,with_response_metadata,with_usage,with_replay_item,failing,cancelling,with_timestamp}` | `crates/pi-ai/src/scripted.rs:163`, `crates/pi-ai/src/scripted.rs:175`, `crates/pi-ai/src/scripted.rs:181`, `crates/pi-ai/src/scripted.rs:192`, `crates/pi-ai/src/scripted.rs:198`, `crates/pi-ai/src/scripted.rs:205`, `crates/pi-ai/src/scripted.rs:214`, `crates/pi-ai/src/scripted.rs:222` | fixture builder methods, including generic API input | shared | exclude | Adds polling, response metadata, usage, replay, terminal, and timestamp fixture behavior. |
| `pi_ai::{text_response,tool_call_response,deferred_response}` | `crates/pi-ai/src/scripted.rs:245`, `crates/pi-ai/src/scripted.rs:256`, `crates/pi-ai/src/scripted.rs:270` | generic/free fixture constructors | shared | exclude | Convenience text, tool-call, and deferred fixture creation. The binding uses the first two at `bindings/pi-ffi/src/lib.rs:1012`. |
| `pi_ai::ScriptedReplayItem` | `crates/pi-ai/src/scripted.rs:40` | replay fixture data struct | shared | exclude | Replay ID, ordinal, target, kind, applicability, and opaque payload used by `with_replay_item`. |
| `pi_ai::ScriptedReplayTarget` | `crates/pi-ai/src/scripted.rs:24` | data enum | shared | exclude | Message, generated content-block/tool-call index, or provider output index used only to manufacture fixture replay targets. |

The binding-specific `RuntimeConfig`, `ScriptedResponseConfig`,
`ScriptedAuthProviderConfig`, `ScriptedDeviceCodeConfig`,
`ScriptedCallbackManualConfig`, `ScriptedAuthFlow`, and
`ScriptedAuthResolver` are private hand-written fixture/configuration items
(`bindings/pi-ffi/src/lib.rs:991`, `bindings/pi-ffi/src/lib.rs:1002`,
`bindings/pi-ffi/src/lib.rs:1031`, `bindings/pi-ffi/src/lib.rs:1062`,
`bindings/pi-ffi/src/lib.rs:1079`, `bindings/pi-ffi/src/lib.rs:1099`,
`bindings/pi-ffi/src/lib.rs:1104`). They are **exclude** and are not substituted
for any native public item in this R1 inventory.

#### Auth and login graph

These are direct argument, result, or callback types of the `Models` auth
methods and the provider registration accepted by `ModelsBuilder`:

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::{AuthCheck,AuthSource,ResolvedAuth,AuthResolutionPurpose,ResolveAuthRequest}` | `crates/pi-ai/src/provider.rs:211`, `crates/pi-ai/src/provider.rs:85`, `crates/pi-ai/src/provider.rs:96`, `crates/pi-ai/src/provider.rs:135`, `crates/pi-ai/src/provider.rs:146` | auth result/data structs and enum, including trait objects in `ResolveAuthRequest` | Send/shared | extended | `check_auth` returns `AuthCheck`; `resolve_auth` returns `ResolvedAuth`; custom `AuthResolver` implementations receive `ResolveAuthRequest`. |
| `pi_ai::{SecretString,ApiKeyCredential,OAuthCredential,ProviderOAuthExtra,Credential,CredentialType,CredentialInfo}` | `crates/pi-ai/src/auth.rs:27`, `crates/pi-ai/src/auth.rs:55`, `crates/pi-ai/src/auth.rs:78`, `crates/pi-ai/src/auth.rs:104`, `crates/pi-ai/src/auth.rs:210`, `crates/pi-ai/src/auth.rs:230`, `crates/pi-ai/src/auth.rs:239` | secret wrapper and credential data graph, including a large enum and opaque JSON | shared | extended | `login` returns `Credential`; `filter_models`, stores, status, auth resolution, and OAuth provider data use the rest of this graph. |
| `pi_ai::{CredentialStore,CredentialLease}` | `crates/pi-ai/src/auth.rs:259`, `crates/pi-ai/src/auth.rs:247` | async traits host implements; nested returned trait object | Send | extended | Builder-injected credential storage. `CredentialStore::acquire_lease` returns `Box<dyn CredentialLease>`, whose consuming `commit` returns a `'static` boxed future. |
| `CredentialStore::{read,list,acquire_lease}` / `CredentialLease::{current,replace,commit}` | `crates/pi-ai/src/auth.rs:261`, `crates/pi-ai/src/auth.rs:268`, `crates/pi-ai/src/auth.rs:274`, `crates/pi-ai/src/auth.rs:249`, `crates/pi-ai/src/auth.rs:252`, `crates/pi-ai/src/auth.rs:255` | async trait methods / borrowed getter / mutation / consuming async method | Send | extended | Complete credential-store callback surface used by request resolution, login, logout, and OAuth refresh. |
| `pi_ai::AuthContext` | `crates/pi-ai/src/auth.rs:682` | async trait host implements | Send | extended | Builder-injected ambient environment/file access; `read_file` returns `SecretString`. |
| `pi_ai::AuthResolutionOverrides` | `crates/pi-ai/src/auth.rs:1347` | secret-bearing data struct | Send/shared | extended | Direct input to `resolve_auth` and every `*_with_auth` method; contains an optional secret, environment map, and duration. |
| `pi_ai::AuthResolver` | `crates/pi-ai/src/provider.rs:248` | async trait provider/host implements | Send | extended | Direct `ProviderRegistrationBuilder::auth` callback. `login` receives `Arc<dyn AuthInteraction>` and returns `Credential`; `resolve` returns `ResolvedAuth`. The current binding injects a concrete resolver at `bindings/pi-ffi/src/lib.rs:260`. |
| `pi_ai::AuthInteraction` | `crates/pi-ai/src/auth.rs:1085` | async/sync trait host implements | Send | extended | Direct `Models::login` callback. It reports capabilities, prompts for answers, emits auth events, and returns a host-owned `Box<dyn RedirectReceiver>`. The existing binding implements it at `bindings/pi-ffi/src/auth_session.rs:370`. |
| `pi_ai::RedirectReceiver` | `crates/pi-ai/src/auth.rs:1207` | consuming async trait host implements | Send | extended | Nested callback returned by `AuthInteraction::create_redirect_receiver`; exposes a borrowed redirect URI and consumes the box to await `RedirectArrival`. The existing binding implements it at `bindings/pi-ffi/src/auth_session.rs:703`. |
| `pi_ai::{AuthHostCapabilities,AuthPrompt,AuthAnswer,AuthSelectOption}` | `crates/pi-ai/src/auth.rs:922`, `crates/pi-ai/src/auth.rs:950`, `crates/pi-ai/src/auth.rs:985`, `crates/pi-ai/src/auth.rs:939` | data struct and large data enums | shared | extended | Capability and challenge/response values used by `AuthInteraction`. `AuthPrompt` includes text, secret, selection, and manual-code forms. |
| `pi_ai::{AuthEvent,AuthInfoLink,AuthChallengeId}` | `crates/pi-ai/src/auth.rs:1004`, `crates/pi-ai/src/auth.rs:995`, `crates/pi-ai/src/ids.rs:99` | large event enum, link struct, open string newtype | shared | extended | Notification graph: info, open-URL, device-code, and progress events. Challenge IDs also correlate manual prompts, redirect requests, and superseded responses. |
| `pi_ai::{RedirectReceiverRequest,RedirectStrategy,AuthHtmlPage,RedirectArrival,RedirectStrategyDescription}` | `crates/pi-ai/src/auth.rs:1177`, `crates/pi-ai/src/auth.rs:1140`, `crates/pi-ai/src/auth.rs:1132`, `crates/pi-ai/src/auth.rs:1199`, `crates/pi-ai/src/auth.rs:1192` | redirect request/value graph and large enum | shared | extended | Complete redirect-reception graph and the unsupported-strategy error payload. |
| `pi_ai::{AuthInteractionError,AuthError,StoreError}` | `crates/pi-ai/src/auth.rs:1043`, `crates/pi-ai/src/auth.rs:1233`, `crates/pi-ai/src/catalog.rs:1481` | callback error enum, non-exhaustive auth error enum, store error struct | shared | extended | Errors across host interaction, provider auth, and credential leases. `AuthError::code` is used by the binding at `bindings/pi-ffi/src/lib.rs:240`. |
| `pi_ai::{ProviderAuthResolver,ApiKeyAuth,ApiKeyResolveRequest,AuthClock}` | `crates/pi-ai/src/auth.rs:1728`, `crates/pi-ai/src/auth.rs:1384`, `crates/pi-ai/src/auth.rs:1372`, `crates/pi-ai/src/auth.rs:1695` | library resolver composition, callback traits, and callback request struct | Send | extended | Standard provider-auth composition and expiry clock. `ApiKeyResolveRequest` carries a nested `Arc<dyn AuthContext>`; the OAuth branch is itemized below. |
| `ProviderAuthResolver::{new,with_clock}` / `EnvironmentApiKeyAuth::new` | `crates/pi-ai/src/auth.rs:1749`, `crates/pi-ai/src/auth.rs:1758`, `crates/pi-ai/src/auth.rs:1461` | trait-object constructor/builder / iterator-generic constructor | Send | extended | Ordinary construction of the standard resolver and API-key method. |
| `pi_ai::{EnvironmentApiKeyAuth,AnonymousAuthResolver,EmptyAuthContext,MapAuthContext,InMemoryCredentialStore,SystemAuthClock}` | `crates/pi-ai/src/auth.rs:1454`, `crates/pi-ai/src/provider.rs:314`, `crates/pi-ai/src/auth.rs:747`, `crates/pi-ai/src/auth.rs:799`, `crates/pi-ai/src/auth.rs:318`, `crates/pi-ai/src/auth.rs:1708` | library implementations | Send/shared | extended | Concrete defaults and basic implementations available to an embedding application. `MapAuthContext::{new,with_file}` and `InMemoryCredentialStore::new` are at `crates/pi-ai/src/auth.rs:817`, `crates/pi-ai/src/auth.rs:830`, and `crates/pi-ai/src/auth.rs:349`. |

##### OAuth helpers and callback surface observed at reviewed call sites

The existing binding and auth tests call these native helpers directly; they
are not inferred from the binding's JSON challenge format
(`bindings/pi-ffi/src/lib.rs:1152`, `bindings/pi-ffi/src/lib.rs:1205`,
`bindings/pi-ffi/src/lib.rs:1248`, `bindings/pi-ffi/src/auth_session.rs:362`,
`crates/pi-ai/tests/m3_3_auth.rs:1031`,
`crates/pi-ai/tests/m3_3_auth.rs:1143`).

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::OAuthAuth` | `crates/pi-ai/src/auth.rs:1643` | async trait provider/host implements | Send | extended | Provider OAuth capability: synchronous `name` plus `login`, `refresh`, and `to_auth`, each returning `SendBoxFuture`; `ProviderAuthResolver` stores it as `Arc<dyn OAuthAuth>` at `crates/pi-ai/src/auth.rs:1730`. |
| `pi_ai::LocalOAuthAuth` | `crates/pi-ai/src/auth.rs:1669` | async trait provider/host implements | Local | exclude | Local-executor twin of `OAuthAuth`; excluded with the rest of the Local family for the Tokio/Swift embedding target. |
| `pi_ai::PkcePair` | `crates/pi-ai/src/oauth.rs:22` | data struct | shared | extended | PKCE verifier/challenge pair. |
| `pi_ai::generate_pkce` | `crates/pi-ai/src/oauth.rs:30` | fallible sync function | shared | extended | Production random PKCE construction. |
| `pi_ai::pkce_from_random_bytes` | `crates/pi-ai/src/oauth.rs:44` | sync fixture/helper function | shared | exclude | Deterministic captured-fixture constructor; the auth test calls it at `crates/pi-ai/tests/m3_3_auth.rs:1145`. |
| `pi_ai::generate_oauth_state` | `crates/pi-ai/src/oauth.rs:55` | fallible sync function | shared | extended | Production random state generation. |
| `pi_ai::oauth_state_from_random_bytes` | `crates/pi-ai/src/oauth.rs:67` | sync fixture/helper function | shared | exclude | Deterministic state rendering for fixtures; called at `crates/pi-ai/tests/m3_3_auth.rs:1149`. |
| `pi_ai::validate_oauth_state` | `crates/pi-ai/src/oauth.rs:78` | fallible sync function | shared | extended | State comparison used by the binding's callback/manual flow at `bindings/pi-ffi/src/lib.rs:1250`. |
| `pi_ai::OAuthAuthorizationInput` | `crates/pi-ai/src/oauth.rs:97` | data struct | shared | extended | Optional authorization code and state returned by the parser. |
| `pi_ai::parse_oauth_authorization_input` | `crates/pi-ai/src/oauth.rs:106` | sync function | shared | extended | Parses redirect URL, query, fragment, or raw code; called by the binding at `bindings/pi-ffi/src/lib.rs:1248`. |
| `pi_ai::select_first_valid<T,LeftFactory,LeftFuture,RightFactory,RightFuture>` | `crates/pi-ai/src/oauth.rs:155` | generic async function with closure/future type parameters | shared | extended | Races redirect and manual completion paths with child cancellation; called by the binding at `bindings/pi-ffi/src/lib.rs:1205`. |
| `pi_ai::OAuthDeviceCodePollResult<T>` | `crates/pi-ai/src/oauth.rs:231` | generic data enum | shared | extended | Pending, slow-down with optional `Duration`, failed message, or completed typed value. |
| `pi_ai::OAuthDeviceCodePoll<T>` | `crates/pi-ai/src/oauth.rs:250` | generic async trait host/provider implements | Send | extended | Mutable token-endpoint polling callback returning `SendBoxFuture`; the binding's private fixture implements it at `bindings/pi-ffi/src/lib.rs:1300`. |
| `pi_ai::OAuthDeviceCodeRuntime` | `crates/pi-ai/src/oauth.rs:268` | async trait host/provider implements | Send | extended | Injectable clock and cancellable timer used by polling. |
| `pi_ai::SystemOAuthDeviceCodeRuntime` | `crates/pi-ai/src/oauth.rs:295` | library implementation struct | Send + Local | extended | Default clock/timer used by `OAuthDeviceCodePollOptions::new`. |
| `pi_ai::OAuthDeviceCodePollOptions<T>` / `OAuthDeviceCodePollOptions::new` | `crates/pi-ai/src/oauth.rs:344`, `crates/pi-ai/src/oauth.rs:372` | generic trait-object configuration struct / constructor | Send | extended | Interval, expiry, first-poll behavior, `Box<dyn OAuthDeviceCodePoll<T>>`, `CancellationToken`, and `Arc<dyn OAuthDeviceCodeRuntime>`; constructed by the binding at `bindings/pi-ffi/src/lib.rs:1152`. |
| `pi_ai::poll_oauth_device_code_flow<T>` | `crates/pi-ai/src/oauth.rs:429` | generic async function | Send | extended | Consumes `OAuthDeviceCodePollOptions<T>`; called by the binding at `bindings/pi-ffi/src/lib.rs:1161`. |
| `pi_ai::LocalOAuthDeviceCodePoll<T>` | `crates/pi-ai/src/oauth.rs:259` | generic async trait host/provider implements | Local | exclude | Local polling callback returning `LocalBoxFuture`; excluded as the alternate executor family. |
| `pi_ai::LocalOAuthDeviceCodeRuntime` | `crates/pi-ai/src/oauth.rs:281` | async trait host/provider implements | Local | exclude | Local clock/timer twin. |
| `pi_ai::LocalOAuthDeviceCodePollOptions<T>` / `LocalOAuthDeviceCodePollOptions::new` | `crates/pi-ai/src/oauth.rs:385`, `crates/pi-ai/src/oauth.rs:413` | generic trait-object configuration struct / constructor | Local | exclude | Local option graph using `Box<dyn LocalOAuthDeviceCodePoll<T>>` and `Rc<dyn LocalOAuthDeviceCodeRuntime>`. |
| `pi_ai::poll_local_oauth_device_code_flow<T>` | `crates/pi-ai/src/oauth.rs:497` | generic async function | Local | exclude | Local polling state-machine entry point. |
| `pi_ai::LocalAuthClock` | `crates/pi-ai/src/auth.rs:1701` | sync trait host/provider implements | Local | exclude | Local expiry-clock twin used by the Local auth resolver family. |
| `pi_ai::redirect_strategy_supported` | `crates/pi-ai/src/auth.rs:2377` | sync function | shared | extended | Tests a `RedirectStrategy` against `AuthHostCapabilities`; the binding uses it at `bindings/pi-ffi/src/auth_session.rs:362`. |
| `pi_ai::create_supported_redirect_receiver` | `crates/pi-ai/src/auth.rs:2393` | async function returning boxed callback trait object | Send | extended | Validates host capabilities and returns `Box<dyn RedirectReceiver>`; auth tests call it at `crates/pi-ai/tests/m3_3_auth.rs:1260`. |
| `std::time::Duration` / `url::Url` | `crates/pi-ai/src/oauth.rs:13`, `crates/pi-ai/src/oauth.rs:14` | standard/external value types | shared | extended | Concrete interval/deadline and authorization/redirect leaves in the OAuth graph. |

The binding-private `ScriptedCredentialPoll` is **exclude**: it exists only to
feed deterministic credentials into `OAuthDeviceCodePoll`
(`bindings/pi-ffi/src/lib.rs:1295`). The public trait and result enum it
implements remain extended native surface.

This graph is sufficient to describe the existing scripted login without its
JSON envelope: the host supplies `AuthHostCapabilities`, `Models::login`
receives `Arc<dyn AuthInteraction>`, the provider emits `AuthEvent::DeviceCode`
and `AuthEvent::Progress`, a challenge is correlated by `AuthChallengeId`, and
the flow returns `Credential` or `AuthError` (`bindings/pi-ffi/src/lib.rs:233`,
`bindings/pi-ffi/src/lib.rs:1145`, `bindings/pi-ffi/src/lib.rs:1162`). The C
example observes the challenge loop and a superseded late response at
`bindings/pi-ffi/examples/scripted_host.rs:152` and
`bindings/pi-ffi/examples/scripted_host.rs:175`.

#### Provider, model, and catalog graph

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::{ProviderDescriptor,HeaderMapSpec}` | `crates/pi-ai/src/provider.rs:44`, `crates/pi-ai/src/model.rs:17` | data struct / map alias | shared | extended | Provider identity, URL, display name, and case-preserving logical headers with deletion markers. |
| `url::Url` / `http::HeaderMap` | `crates/pi-ai/src/provider.rs:50`, `crates/pi-ai/src/provider.rs:100` | external URL / HTTP-header data types | shared | extended | Direct provider-builder/descriptor and resolved-auth signature graph; provider execution and middleware use `HeaderMap`, while logical caller configuration uses `HeaderMapSpec`. |
| `pi_ai::{ModelCatalog,StaticModelCatalog,ProviderCatalogState}` | `crates/pi-ai/src/provider.rs:353`, `crates/pi-ai/src/provider.rs:388`, `crates/pi-ai/src/catalog.rs:538` | trait provider/host implements, library implementation, managed-state struct | Send | extended | Direct `ProviderRegistrationBuilder::catalog` graph. The trait exposes immutable snapshots and optional managed state/source. |
| `pi_ai::{ModelCatalogSource,CatalogFetchContext,CatalogCandidate}` | `crates/pi-ai/src/catalog.rs:243`, `crates/pi-ai/src/catalog.rs:231`, `crates/pi-ai/src/catalog.rs:58` | async trait provider/host implements and data structs | Send | extended | Direct `ProviderRegistrationBuilder::catalog_source` graph for dynamic catalogs. |
| `pi_ai::{ModelsStore,ModelOverrideStore}` | `crates/pi-ai/src/catalog.rs:269`, `crates/pi-ai/src/catalog.rs:319` | async and sync traits host implements | Send | extended | Direct `ModelsBuilder` catalog-persistence and host-policy callback traits. |
| `pi_ai::{CatalogSnapshot,PersistedCatalogSnapshot,ProviderCatalogLayers}` | `crates/pi-ai/src/catalog.rs:27`, `crates/pi-ai/src/catalog.rs:98`, `crates/pi-ai/src/catalog.rs:134` | catalog data structs | Send/shared | extended | Direct `Models` results and `ModelsStore` values. |
| `pi_ai::{ModelOverride,ModelOverrideAction,ModelOverridePatch}` | `crates/pi-ai/src/catalog.rs:149`, `crates/pi-ai/src/catalog.rs:190`, `crates/pi-ai/src/catalog.rs:208` | data struct, large enum, patch struct | shared | extended | Runtime/host override inputs and provenance-layer values. |
| `pi_ai::{RefreshRequest,RefreshReport,ProviderRefreshResult}` | `crates/pi-ai/src/catalog.rs:1351`, `crates/pi-ai/src/catalog.rs:1372`, `crates/pi-ai/src/catalog.rs:1383` | request/report structs and large result enum | shared | extended | Direct `Models::refresh` input/output graph. |
| `pi_ai::{CatalogError,CatalogErrorReport,StoreError,OverrideError}` | `crates/pi-ai/src/catalog.rs:1421`, `crates/pi-ai/src/catalog.rs:1412`, `crates/pi-ai/src/catalog.rs:1481`, `crates/pi-ai/src/catalog.rs:1512` | error/report structs | shared | extended | Catalog callback and live-mutation failures. |
| `pi_ai::ModelAvailabilityFilter` | `crates/pi-ai/src/provider.rs:2309` | `Arc`-wrapped `Fn` trait-object alias | Send | extended | Direct `ProviderRegistrationBuilder::filter_models` callback over model slices and an optional borrowed `Credential`. |
| `pi_ai::ChatApi` | `crates/pi-ai/src/provider.rs:877` | async trait provider/host implements | Send | extended | Direct `ProviderRegistrationBuilder::api` callback; receives owned resolved requests and returns `AssistantStream` or `AiError`. |
| `pi_ai::{ResolvedApiRequest,ResolvedDeferredRequest,AiError,AiErrorKind}` | `crates/pi-ai/src/provider.rs:559`, `crates/pi-ai/src/provider.rs:600`, `crates/pi-ai/src/provider.rs:448`, `crates/pi-ai/src/provider.rs:531` | large request structs and provider/API error graph | Send/shared | extended | Direct `ChatApi` method inputs/results. Resolved requests themselves contain middleware trait-object slices and a retry-classifier trait object. |
| `pi_ai::{RetryPolicy,RetryClassifier,AttemptFailure,RetryDecision}` | `crates/pi-ai/src/retry.rs:15`, `crates/pi-ai/src/retry.rs:273`, `crates/pi-ai/src/retry.rs:43`, `crates/pi-ai/src/retry.rs:258` | policy data, sync callback trait, large failure enum, decision enum | Send/shared | extended | Direct provider builder configuration and the complete classifier callback graph. |
| `pi_ai::{HttpChatApi,ErasedApiHandler,HttpTransport,RetrySleeper}` | `crates/pi-ai/src/provider.rs:959`, `crates/pi-ai/src/provider.rs:717`, `crates/pi-ai/src/middleware.rs:275`, `crates/pi-ai/src/retry.rs:549` | library composition plus three nested async callback traits | Send | extended | Public standard HTTP `ChatApi` composition for custom provider/API assembly. `HttpChatApi::{new,with_retry_sleeper}` accepts those callback objects at `crates/pi-ai/src/provider.rs:975` and `crates/pi-ai/src/provider.rs:984`. |
| `pi_ai::{ApiCallOptions<'a>,ApiExecutionContext<'a>,DeferredExecutionContext<'a>,ProviderResponseStream}` | `crates/pi-ai/src/provider.rs:633`, `crates/pi-ai/src/provider.rs:653`, `crates/pi-ai/src/provider.rs:684`, `crates/pi-ai/src/provider.rs:714` | borrowed enum/context structs and response alias | Send | extended | Complete direct callback graph of `ErasedApiHandler`; contexts carry borrowed model/request/auth/transport/retry/middleware capabilities. |
| `pi_ai::{HttpRequest,HttpBody,HttpResponse,TransportError}` | `crates/pi-ai/src/middleware.rs:20`, `crates/pi-ai/src/middleware.rs:69`, `crates/pi-ai/src/middleware.rs:76`, `crates/pi-ai/src/middleware.rs:196` | request struct / stream alias / streamed response / error struct | Send | extended | Complete `HttpTransport::execute` input/result graph; the response body is another `SendBoxStream`, this time of byte chunks or `TransportError`. |

##### Complete transitive model-descriptor and API-configuration graph

`Models::{models,filter_models,model}` return this graph, and provider
registration accepts it through static or dynamic catalogs
(`crates/pi-ai/src/models.rs:132`, `crates/pi-ai/src/models.rs:150`,
`crates/pi-ai/src/models.rs:248`, `crates/pi-ai/src/provider.rs:388`). The rows
below name every public Rust type reached through `ModelDescriptor`; scalar and
standard-library collection leaves are stated in the role column rather than
treated as unnamed configuration.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::ModelDescriptor` | `crates/pi-ai/src/model.rs:29` | data struct | shared | extended | Root catalog value: `common: CommonModelDescriptor`, `api: ApiModelConfig`, and `extensions: ExtensionMap`. |
| `pi_ai::CommonModelDescriptor` | `crates/pi-ai/src/model.rs:40` | data struct | shared | extended | API-independent fields: `ModelRef`, display name, `url::Url`, `ModalityCapabilities`, `ModelLimits`, `ModelPricing`, reasoning flag, and `HeaderMapSpec`. |
| `pi_ai::ModalityCapabilities` | `crates/pi-ai/src/model.rs:89` | data struct | shared | extended | Input and output are `BTreeSet<Modality>`. |
| `pi_ai::Modality` | `crates/pi-ai/src/model.rs:78` | data enum | shared | extended | Text, image, or audio set members. |
| `pi_ai::ModelLimits` | `crates/pi-ai/src/model.rs:98` | data struct | shared | extended | `context_window: u64` and `max_output_tokens: u32`. |
| `pi_ai::ModelPricing` | `crates/pi-ai/src/usage.rs:202` | data struct | shared | extended | Pricing root: default `TokenPriceRates`, ordered `Vec<RequestWidePriceTier>`, and `CacheWriteRetentionPricing`. |
| `pi_ai::TokenPriceRates` | `crates/pi-ai/src/usage.rs:155` | data struct | shared | extended | Four `MoneyRate` values for input, output, cache read, and cache write. |
| `pi_ai::MoneyRate` | `crates/pi-ai/src/usage.rs:130` | numeric newtype | shared | extended | Fixed-point micro-currency units per million tokens. |
| `pi_ai::RequestWidePriceTier` | `crates/pi-ai/src/usage.rs:169` | data struct | shared | extended | Strict input-token threshold plus replacement `TokenPriceRates`. |
| `pi_ai::CacheWriteRetentionPricing` | `crates/pi-ai/src/usage.rs:179` | data struct | shared | extended | Optional short-retention and one-hour `MoneyRate` overrides. |
| `pi_ai::HeaderMapSpec` | `crates/pi-ai/src/model.rs:17` | map alias | shared | extended | `BTreeMap<String, Option<String>>`; `None` is retained as a logical deletion marker. |
| `pi_ai::ApiModelConfig` | `crates/pi-ai/src/model.rs:112` | large data enum | shared | extended | Nine tagged variants covering OpenAI Completions, OpenAI Responses, OpenAI Codex Responses, Anthropic Messages, Gemini Developer, Google Vertex, Bedrock Converse, Mistral Conversations, and custom APIs. |
| `ApiModelConfig::api_id` | `crates/pi-ai/src/model.rs:144` | sync getter returning owned open ID | shared | extended | Returns the effective `ApiId`, including the custom variant's configured ID. |
| `pi_ai::OpenAiCompletionsModelConfig` | `crates/pi-ai/src/model.rs:162` | data struct | shared | extended | Contains `OpenAiCompletionsCompat`, `ThinkingLevelMap<OpenAiThinkingValue>`, and `OrderedJsonObject` sampling defaults. |
| `pi_ai::OpenAiCompletionsCompat` | `crates/pi-ai/src/model.rs:199` | large data struct | shared | extended | Scalar support switches plus the named branches `MaxTokensField`, `OpenAiThinkingFormat`, `ChatTemplateValues`, `OpenRouterRouting`, `VercelGatewayRouting`, `ThinkingTokenBudgetField`, `CacheControlFormat`, `DeferredToolsMode`, `SessionAffinityFormat`, and `ExtensionMap`. |
| `pi_ai::MaxTokensField` | `crates/pi-ai/src/model.rs:258` | data enum | shared | extended | Selects `max_completion_tokens` or `max_tokens`. |
| `pi_ai::OpenAiThinkingFormat` | `crates/pi-ai/src/model.rs:516` | data enum | shared | extended | The eleven provider reasoning request conventions stored by the completions compatibility record. |
| `pi_ai::ThinkingTokenBudgetField` | `crates/pi-ai/src/model.rs:556` | data enum | shared | extended | Selects one of the three supported top-level reasoning-budget field names. |
| `pi_ai::CacheControlFormat` | `crates/pi-ai/src/model.rs:568` | data enum | shared | extended | Prompt-cache marker convention. |
| `pi_ai::DeferredToolsMode` | `crates/pi-ai/src/model.rs:576` | data enum | shared | extended | Provider-specific deferred-tool serialization mode. |
| `pi_ai::SessionAffinityFormat` | `crates/pi-ai/src/model.rs:584` | data enum | shared | extended | OpenAI, OpenAI-without-session, or OpenRouter affinity-header convention. |
| `pi_ai::ChatTemplateValues` | `crates/pi-ai/src/model.rs:13` | ordered-map alias | shared | extended | `IndexMap<String, ChatTemplateKwargValue>` used by both chat-template compatibility fields. |
| `pi_ai::ChatTemplateKwargValue` | `crates/pi-ai/src/model.rs:269` | data enum | shared | extended | String, exact `serde_json::Number`, bool, null, or `ChatTemplateVariable`. |
| `pi_ai::ChatTemplateVariable` | `crates/pi-ai/src/model.rs:285` | data struct | shared | extended | `ChatTemplateVariableName` plus an optional omit-when-off flag. |
| `pi_ai::ChatTemplateVariableName` | `crates/pi-ai/src/model.rs:301` | data enum | shared | extended | Thinking-enabled, effort, or budget substitution. |
| `pi_ai::OpenRouterRouting` | `crates/pi-ai/src/model.rs:317` | large data struct | shared | extended | Fallback/parameter/data/ZDR/distillation flags, ordered allow/deny/provider and quantization lists, `OpenRouterSort`, `OpenRouterMaxPrice`, and two `OpenRouterMetricPreference` values. |
| `pi_ai::OpenRouterDataCollection` | `crates/pi-ai/src/model.rs:363` | data enum | shared | extended | Allow or deny upstream data collection. |
| `pi_ai::OpenRouterSort` | `crates/pi-ai/src/model.rs:374` | data enum | shared | extended | A metric-name string or structured `OpenRouterSortOptions`. |
| `pi_ai::OpenRouterSortOptions` | `crates/pi-ai/src/model.rs:385` | data struct | shared | extended | Optional sort metric plus tri-state `NullableString` partition. |
| `pi_ai::NullableString` | `crates/pi-ai/src/model.rs:397` | data enum | shared | extended | Preserves absent, explicit JSON null, and string as separate states. |
| `pi_ai::OpenRouterMaxPrice` | `crates/pi-ai/src/model.rs:441` | data struct | shared | extended | Optional prompt, completion, image, audio, and request `JsonNumberOrString` ceilings. |
| `pi_ai::JsonNumberOrString` | `crates/pi-ai/src/model.rs:463` | data enum | shared | extended | Exact `serde_json::Number` or string price representation. |
| `pi_ai::OpenRouterMetricPreference` | `crates/pi-ai/src/model.rs:474` | data enum | shared | extended | Exact number shorthand or `OpenRouterPercentiles`. |
| `pi_ai::OpenRouterPercentiles` | `crates/pi-ai/src/model.rs:485` | data struct | shared | extended | Optional p50, p75, p90, and p99 exact `serde_json::Number` cutoffs. |
| `pi_ai::VercelGatewayRouting` | `crates/pi-ai/src/model.rs:504` | data struct | shared | extended | Optional exclusive and ordered provider lists. |
| `pi_ai::OpenAiResponsesModelConfig` | `crates/pi-ai/src/model.rs:173` | data struct | shared | extended | Contains `OpenAiResponsesCompat`, `ThinkingLevelMap<OpenAiThinkingValue>`, and `OrderedJsonObject` sampling defaults; both Responses variants use this type. |
| `pi_ai::OpenAiResponsesCompat` | `crates/pi-ai/src/model.rs:600` | data struct | shared | extended | Developer-role, cache, strict/schema, additional-tool, tool-search, and prompt-cache support switches plus `SessionAffinityFormat` and `ExtensionMap`. |
| `pi_ai::OpenAiThinkingValue` | `crates/pi-ai/src/model.rs:186` | data enum | shared | extended | Disabled, provider effort string, or token budget used by both OpenAI thinking maps. |
| `pi_ai::AnthropicMessagesModelConfig` | `crates/pi-ai/src/model.rs:623` | data struct | shared | extended | Contains `AnthropicMessagesCompat` and `ThinkingLevelMap<AnthropicThinkingValue>`. |
| `pi_ai::AnthropicMessagesCompat` | `crates/pi-ai/src/model.rs:666` | large data struct | shared | extended | Tool-stream/cache/session/temperature/adaptive/signature/strict/reference switches, `Vec<AnthropicFallbackModel>`, and `ExtensionMap`. |
| `pi_ai::AnthropicFallbackModel` | `crates/pi-ai/src/model.rs:694` | data struct | shared | extended | Fallback `ProviderId`, `ModelId`, and `ModelPricing`. |
| `pi_ai::AnthropicThinkingValue` | `crates/pi-ai/src/model.rs:634` | data enum | shared | extended | Off, `AnthropicEffort`, or token budget. |
| `pi_ai::AnthropicEffort` | `crates/pi-ai/src/model.rs:646` | data enum | shared | extended | Minimal through max provider-native adaptive effort. |
| `pi_ai::GoogleModelConfig` | `crates/pi-ai/src/model.rs:706` | data struct | shared | extended | `ThinkingLevelMap<String>` for both Gemini Developer and Vertex variants. |
| `pi_ai::BedrockModelConfig` | `crates/pi-ai/src/model.rs:714` | data struct | shared | extended | Contains `BedrockCompat` and `ThinkingLevelMap<String>`. |
| `pi_ai::BedrockCompat` | `crates/pi-ai/src/model.rs:724` | data struct | shared | extended | Strict-mode support switch plus `ExtensionMap`. |
| `pi_ai::MistralModelConfig` | `crates/pi-ai/src/model.rs:734` | data struct | shared | extended | `ThinkingLevelMap<String>`. |
| `pi_ai::CustomApiModelConfig` | `crates/pi-ai/src/model.rs:741` | opaque-JSON data struct | shared | extended | Open `ApiId`, schema version, and exact `Box<serde_json::value::RawValue>`. |
| `pi_ai::ThinkingLevelMap<T>` | `crates/pi-ai/src/model.rs:763` | generic data struct | shared | extended | Seven optional `LevelSupport<T>` entries from off through max. |
| `pi_ai::LevelSupport<T>` | `crates/pi-ai/src/model.rs:805` | generic data enum | shared | extended | Unsupported, explicitly disabled, or typed provider value. |
| `pi_ai::ReasoningLevelResolution<T>` | `crates/pi-ai/src/model.rs:816` | generic data struct | shared | extended | Public result of `ThinkingLevelMap::resolve`, containing requested/effective `ReasoningLevel`, optional `LevelSupport<T>`, and clamp flag. |
| `ThinkingLevelMap::{get,resolve}` | `crates/pi-ai/src/model.rs:829`, `crates/pi-ai/src/model.rs:843` | generic borrowed getter / fallible sync method | shared | extended | Accept `ReasoningLevel`; `resolve` additionally accepts `ReasoningFallback` and returns `ReasoningLevelResolution<T>` or `LoweringError`. |
| `pi_ai::ExtensionMap` | `crates/pi-ai/src/model.rs:25` | map alias | shared | extended | `BTreeMap<ExtensionId, VersionedExtension>` used at descriptor and compatibility levels. |
| `pi_ai::VersionedExtension` | `crates/pi-ai/src/model.rs:919` | opaque-JSON data struct | shared | extended | Schema version plus exact `Box<serde_json::value::RawValue>`. |
| `pi_ai::{OrderedJsonObject,OrderedJsonArray,OrderedJsonValue,OrderedJsonString}` | `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:227`, `crates/pi-ai/src/json_compat.rs:324`, `crates/pi-ai/src/json_compat.rs:24` | recursive ordered JSON graph | shared | extended | Sampling defaults and later full-option branches reach recursive object/array/value/string storage. |
| `pi_ai::{ProviderId,ModelId,ApiId,ExtensionId,ModelRef}` | `crates/pi-ai/src/ids.rs:58`, `crates/pi-ai/src/ids.rs:62`, `crates/pi-ai/src/ids.rs:66`, `crates/pi-ai/src/ids.rs:74`, `crates/pi-ai/src/ids.rs:105` | open newtypes / data struct | shared | extended | Identifier leaves of common, fallback, custom, and extension configuration. |
| `ModelPricing::{rates_for,calculate_cost,calculate_cost_with_multiplier}` | `crates/pi-ai/src/usage.rs:213`, `crates/pi-ai/src/usage.rs:223`, `crates/pi-ai/src/usage.rs:237` | borrowed getter / fallible sync methods | shared | extended | Accept usage, `Currency`, `CacheWriteRetention`, and optional rational multiplier; return `TokenPriceRates` or `Cost`/`CostArithmeticError`. |
| `pi_ai::{Currency,CacheWriteRetention,Cost,CostArithmeticError}` | `crates/pi-ai/src/usage.rs:88`, `crates/pi-ai/src/usage.rs:190`, `crates/pi-ai/src/usage.rs:119`, `crates/pi-ai/src/usage.rs:301` | newtype / data enum / data struct / error enum | shared | extended | Public input/result graph of the model-pricing methods. |
| `url::Url`, `serde_json::Number`, `serde_json::value::RawValue`, `indexmap::IndexMap`, `BTreeMap`, `BTreeSet` | `crates/pi-ai/src/model.rs:9`, `crates/pi-ai/src/model.rs:6`, `crates/pi-ai/src/model.rs:4`, `crates/pi-ai/src/model.rs:7` | external/standard container and opaque value types | shared | extended | Concrete non-`pi_ai` leaves appearing in the public model/configuration graph. |

#### Models middleware and API-family generic graph

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::{HeaderTransform,HeaderTransformContext,MiddlewareError}` | `crates/pi-ai/src/middleware.rs:337`, `crates/pi-ai/src/middleware.rs:325`, `crates/pi-ai/src/middleware.rs:298` | async trait host implements, borrowed context, error struct | Send/shared | extended | Direct `ModelsBuilder::header_transform` callback graph. |
| `pi_ai::{PayloadTransform<A>,PayloadTransformContext<'a,A>,PayloadTransformResult<T>}` | `crates/pi-ai/src/middleware.rs:368`, `crates/pi-ai/src/middleware.rs:358`, `crates/pi-ai/src/middleware.rs:389` | generic async trait host implements and generic value graph | Send | extended | Direct typed `ModelsBuilder::payload_transform<A>` callback graph. |
| `pi_ai::{ErasedPayloadTransform,ErasedPayloadContext,ProviderPayload,PayloadTransformDisposition}` | `crates/pi-ai/src/middleware.rs:552`, `crates/pi-ai/src/middleware.rs:540`, `crates/pi-ai/src/middleware.rs:440`, `crates/pi-ai/src/middleware.rs:677` | async trait host implements, borrowed context, type-erased payload, large result enum | Send | extended | Direct `ModelsBuilder::erased_payload_transform` callback graph. |
| `pi_ai::{ResponseObserver,ResponseObservationContext,ProviderResponseMetadata}` | `crates/pi-ai/src/middleware.rs:721`, `crates/pi-ai/src/middleware.rs:711`, `crates/pi-ai/src/middleware.rs:686` | async trait host implements and borrowed/owned response values | Send/shared | extended | Direct `ModelsBuilder::response_observer` callback graph. |
| `pi_ai::{AttemptMiddleware,HttpRequest}` | `crates/pi-ai/src/middleware.rs:742`, `crates/pi-ai/src/middleware.rs:20` | async trait host implements / large request struct | Send | extended | Direct `ModelsBuilder::attempt_middleware` callback graph; the callback mutably borrows each attempt-local request. |
| `pi_ai::ApiFamily` | `crates/pi-ai/src/options.rs:368` | generic trait with five associated types and three sync functions | Send | extended | Generic contract used by `Models::stream_api`, typed payload transforms, model narrowing, lowering, and encoding. |
| `pi_ai::{TypedModelDescriptor<A>,SimpleLoweringContext<'a,A>,EncodeContext<'a,A>}` | `crates/pi-ai/src/options.rs:316`, `crates/pi-ai/src/options.rs:336`, `crates/pi-ai/src/options.rs:355` | generic owned and borrowed structs | Send/shared | extended | Direct `ApiFamily` and `PayloadTransform<A>` signature graph. |
| `pi_ai::{ErasedApiFullOptions,ApiOptionsInput<A>,ApiRequestOptions,ErasedApiOptionsPatch}` | `crates/pi-ai/src/options.rs:410`, `crates/pi-ai/src/options.rs:525`, `crates/pi-ai/src/options.rs:450`, `crates/pi-ai/src/options.rs:294` | type-erased/generic options values and data structs | Send/shared | extended | Full-options dispatch, typed/erased simple patches, and request transport controls. |
| `pi_ai::{LoweringError,EncodeError}` | `crates/pi-ai/src/options.rs:654`, `crates/pi-ai/src/options.rs:705` | non-exhaustive error enums | shared | extended | `ApiFamily` resolve/lower/encode failures. |

##### Complete provider-neutral request-configuration graph

`ModelRequest::options` is a `SimpleGenerationOptions` value
(`crates/pi-ai/src/runtime.rs:19`). The following rows exhaust its named
transitive field types, then record the separate request controls and typed
patch/full-options carriers used by the public `Models` methods. Primitive
leaves are stated in the containing-record rows rather than repeated as
separate Rust items.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::SimpleGenerationOptions` | `crates/pi-ai/src/options.rs:561` | large data struct | shared | core | Provider-neutral request root. Primitive/container fields are retry count and delay, HTTP and WebSocket timeouts, output cap, temperature, top-p, ordered stop strings, seed, and optional session ID. Its named branches are `StreamTransport`, `ReasoningLevel`, `ReasoningFallback`, `ThinkingBudgets`, recursive `OrderedJsonObject`, `HeaderMapSpec`, `CacheRetention`, `ToolChoice`, `DeferredSubmission`, and `ErasedApiOptionsPatch`. |
| `pi_ai::StreamTransport` | `crates/pi-ai/src/options.rs:510` | data enum | shared | extended | Optional SSE, WebSocket, cached-WebSocket, or automatic transport preference in both simple and API-request options. |
| `pi_ai::ReasoningLevel` / `ReasoningLevel::resolve_extended` | `crates/pi-ai/src/options.rs:19`, `crates/pi-ai/src/options.rs:41` | data enum / fallible sync method | shared | core | Optional request effort from off through max; resolution additionally consumes `ReasoningFallback` and returns `LoweringError` on strict unsupported levels. |
| `pi_ai::ReasoningFallback` | `crates/pi-ai/src/options.rs:77` | data enum | shared | extended | Strict-versus-clamp policy nested directly in every simple-options value. |
| `pi_ai::ThinkingBudgets` / `ThinkingBudgets::budget_for` | `crates/pi-ai/src/options.rs:88`, `crates/pi-ai/src/options.rs:113` | data struct / sync method | shared | extended | Optional per-level `u32` budget overrides; the getter maps `ReasoningLevel` to the configured or default budget. |
| `pi_ai::CacheRetention` | `crates/pi-ai/src/options.rs:136` | data enum | shared | extended | Optional none, short, or long prompt-cache preference. |
| `pi_ai::ToolChoice` | `crates/pi-ai/src/options.rs:149` | data enum | shared | extended | Optional provider-neutral auto-or-none tool selection. |
| `pi_ai::DeferredSubmission` / `DeferredSubmission::is_enabled` | `crates/pi-ai/src/deferred.rs:116`, `crates/pi-ai/src/deferred.rs:130` | data enum / sync method | shared | extended | Optional deferred-execution request: disabled, enabled with provider defaults, or a `DeferredWindow` selection. |
| `pi_ai::DeferredWindow` | `crates/pi-ai/src/deferred.rs:102` | data enum | shared | extended | Fifteen-minute, one-hour, or twenty-four-hour branch nested in `DeferredSubmission::Window`. |
| `pi_ai::{OrderedJsonObject,OrderedJsonArray,OrderedJsonValue,OrderedJsonString}` | `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:227`, `crates/pi-ai/src/json_compat.rs:324`, `crates/pi-ai/src/json_compat.rs:24` | recursive ordered JSON graph | shared | extended | Complete recursive graph reached from `SimpleGenerationOptions::sampling`; the string type preserves exact UTF-16 code units. |
| `pi_ai::HeaderMapSpec` | `crates/pi-ai/src/model.rs:17` | map alias | shared | extended | `BTreeMap<String, Option<String>>` reached by both simple and API request options; `None` records an explicit logical-header deletion. |
| `pi_ai::ErasedApiOptionsPatch` | `crates/pi-ai/src/options.rs:294` | opaque-JSON data struct | shared | extended | Optional dynamic simple-options patch containing `ApiId`, a schema version, and exact `Box<serde_json::value::RawValue>`. |
| `pi_ai::ApiRequestOptions` / `ApiRequestOptions::from` | `crates/pi-ai/src/options.rs:450`, `crates/pi-ai/src/options.rs:469` | transport-options data struct / conversion | shared | extended | Separate controls accepted by `stream_api_with_request_options`: retry count/delay, HTTP and WebSocket timeouts, `StreamTransport`, optional session ID, and `HeaderMapSpec`. Conversion copies that subset from simple options. |
| `pi_ai::ApiOptionsInput<A>` / `ApiOptionsInput::from_sources` | `crates/pi-ai/src/options.rs:525`, `crates/pi-ai/src/options.rs:536` | generic data enum / generic fallible constructor | Send/shared | extended | Resolves none, `A::OptionsPatch`, or `ErasedApiOptionsPatch`; the result/error graph includes `ApiId` and `LoweringError`. |
| `pi_ai::ErasedApiFullOptions` / `ErasedApiFullOptions::{new,downcast_ref}` | `crates/pi-ai/src/options.rs:410`, `crates/pi-ai/src/options.rs:418`, `crates/pi-ai/src/options.rs:426` | type-erased struct / generic methods | Send | extended | Carries one `A::FullOptions` as `Arc<dyn Any + Send + Sync>` through dynamic provider dispatch, then conditionally returns a borrowed concrete full-options value. |
| `pi_ai::{SamplingPlan,CommonSimplePlan,ThinkingBudgetPlan}` | `crates/pi-ai/src/options.rs:159`, `crates/pi-ai/src/options.rs:174`, `crates/pi-ai/src/options.rs:251` | lowering-plan data structs | shared | exclude | Public intermediate records produced inside provider-neutral lowering, not request configuration supplied by an ordinary embedding app. |
| `pi_ai::{plan_common,plan_thinking_budget}` | `crates/pi-ai/src/options.rs:190`, `crates/pi-ai/src/options.rs:223` | fallible sync lowering helpers | shared | exclude | Provider-family implementation helpers. `plan_common` also exposes the internal `TokenEstimator` lowering seam; neither is an ordinary model/agent call. |
| `pi_ai::{CONTEXT_SAFETY_TOKENS,MIN_ANSWER_TOKENS}` | `crates/pi-ai/src/options.rs:128`, `crates/pi-ai/src/options.rs:131` | constants | shared | exclude | Internal common-planning thresholds rather than caller-settable configuration. |

##### Complete concrete `ApiFamily` configuration graphs

These are the concrete associated types selected by `Models::stream_api<A>`
and the simple-patch types used by `ApiFamily::lower_simple`
(`crates/pi-ai/src/models.rs:785`, `crates/pi-ai/src/options.rs:368`). The
full-options records are distinct from the common transport controls in
`ApiRequestOptions` (`crates/pi-ai/src/options.rs:443`). A repository-wide
search finds ten non-test public implementations: eight re-exported by
`pi_ai`, one re-exported by `pi_ai_openai`, and one re-exported by
`pi_ai_pi_messages` (`crates/pi-ai/src/lib.rs:46`,
`crates/pi-ai/src/lib.rs:49`, `crates/pi-ai/src/lib.rs:56`,
`crates/pi-ai/src/lib.rs:62`, `crates/pi-ai/src/lib.rs:66`,
`crates/pi-ai/src/lib.rs:67`,
`providers/pi-ai-openai/src/lib.rs:12`,
`providers/pi-ai-pi-messages/src/lib.rs:11`). Their complete associated-type
matrix is:

| Marker and implementation | `Compat` | `ModelConfig` | `FullOptions` | `OptionsPatch` | `WireRequest` |
|---|---|---|---|---|---|
| `pi_ai::OpenAiCompletions` (`crates/pi-ai/src/openai_completions.rs:215`) | `OpenAiCompletionsCompat` | `OpenAiCompletionsModelConfig` | `OpenAiCompletionsOptions` | `OpenAiCompletionsSimplePatch` | `OrderedJsonObject` |
| `pi_ai::OpenAiResponses` (`crates/pi-ai/src/openai_responses.rs:269`) | `OpenAiResponsesCompat` | `OpenAiResponsesModelConfig` | `OpenAiResponsesOptions` | `OpenAiResponsesSimplePatch` | `OrderedJsonObject` |
| `pi_ai::OpenAiCodexResponses` (`crates/pi-ai/src/openai_responses.rs:304`) | `OpenAiResponsesCompat` | `OpenAiResponsesModelConfig` | `OpenAiCodexResponsesOptions` | `OpenAiCodexResponsesSimplePatch` | `OrderedJsonObject` |
| `pi_ai::AnthropicMessages` (`crates/pi-ai/src/anthropic_messages.rs:113`) | `AnthropicMessagesCompat` | `AnthropicMessagesModelConfig` | `AnthropicOptions` | `AnthropicSimplePatch` | `OrderedJsonObject` |
| `pi_ai::GoogleGenerativeAi` (`crates/pi-ai/src/google.rs:132`) | `GoogleCompat` | `GoogleModelConfig` | `GoogleOptions` | `GoogleSimplePatch` | `OrderedJsonObject` |
| `pi_ai::GoogleVertex` (`crates/pi-ai/src/google.rs:175`) | `GoogleCompat` | `GoogleModelConfig` | `GoogleVertexOptions` | `GoogleSimplePatch` | `OrderedJsonObject` |
| `pi_ai::BedrockConverseStream` (`crates/pi-ai/src/bedrock.rs:195`) | `BedrockCompat` | `BedrockModelConfig` | `BedrockOptions` | `BedrockSimplePatch` | `OrderedJsonObject` |
| `pi_ai::MistralConversations` (`crates/pi-ai/src/mistral.rs:151`) | `MistralCompat` | `MistralModelConfig` | `MistralOptions` | `MistralSimplePatch` | `OrderedJsonObject` |
| `pi_ai_openai::AzureOpenAiResponses` (`providers/pi-ai-openai/src/azure.rs:54`) | `OpenAiResponsesCompat` | `CustomApiModelConfig` | `AzureOpenAiResponsesOptions` | `AzureOpenAiResponsesSimplePatch` | `OrderedJsonObject` |
| `pi_ai_pi_messages::PiMessages` (`providers/pi-ai-pi-messages/src/wire.rs:68`) | `PiMessagesCompat` | `CustomApiModelConfig` | `PiMessagesOptions` | `PiMessagesSimplePatch` | `OrderedJsonObject` |

The detailed rows below name the nested public configuration types. The
model-config rows in the preceding model-descriptor section complete the
shared `Compat` and `ModelConfig` branches; the provider-crate families that
use the open `CustomApiModelConfig` seam are expanded explicitly here.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::OpenAiCompletions` | `crates/pi-ai/src/openai_completions.rs:36` | API-family marker / generic type argument | Send/shared | extended | Selects completions compat, model config, full options, patch, and ordered-JSON wire associated types at `crates/pi-ai/src/openai_completions.rs:215`. |
| `pi_ai::OpenAiCompletionsOptions` | `crates/pi-ai/src/openai_completions.rs:41` | data struct (`ApiFamily::FullOptions`) | shared | extended | `max_tokens`, `MaxTokensField`, `OpenAiReasoningPlan`, temperature, `OrderedJsonObject` sampling, optional `OpenAiCompletionsToolChoice`, `CacheRetention`, and session ID. |
| `pi_ai::OpenAiCompletionsToolChoice` | `crates/pi-ai/src/openai_completions.rs:68` | large data enum | shared | extended | Auto, none, required, named function/custom tool, or allowed-tools with `OpenAiAllowedToolsMode` and `OrderedJsonArray`. |
| `pi_ai::OpenAiAllowedToolsMode` | `crates/pi-ai/src/openai_completions.rs:106` | data enum | shared | extended | Auto or required mode nested in allowed-tools choice. |
| `pi_ai::OpenAiReasoningPlan` | `crates/pi-ai/src/openai_completions.rs:119` | data struct | shared | extended | Product of `OpenAiReasoningMode` and optional `OpenAiReasoningTokenBudget`. |
| `pi_ai::OpenAiReasoningMode` | `crates/pi-ai/src/openai_completions.rs:138` | large data enum | shared | extended | Disabled, reasoning effort with provenance, enabled, OpenRouter, DeepSeek, chat-template, or string-thinking shape; chat-template carries `OrderedJsonObject`. |
| `pi_ai::OpenAiReasoningEffortProvenance` | `crates/pi-ai/src/openai_completions.rs:190` | data enum | shared | extended | Distinguishes a requested level from an explicit model mapping. |
| `pi_ai::OpenAiReasoningTokenBudget` | `crates/pi-ai/src/openai_completions.rs:200` | data struct | shared | extended | `ThinkingTokenBudgetField` plus `u32` budget. |
| `pi_ai::OpenAiCompletionsSimplePatch` | `crates/pi-ai/src/openai_completions.rs:210` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | One ordered API-specific sampling overlay. |
| `pi_ai::OpenAiResponses` | `crates/pi-ai/src/openai_responses.rs:263` | API-family marker / generic type argument | Send/shared | extended | Selects standard Responses associated types at `crates/pi-ai/src/openai_responses.rs:269`. |
| `pi_ai::OpenAiResponsesOptions` | `crates/pi-ai/src/openai_responses.rs:151` | data struct (`ApiFamily::FullOptions`) | shared | extended | Output cap, temperature, `OrderedJsonObject` sampling, effort, tri-state `OpenAiResponsesReasoningSummary`, service tier, optional `OrderedJsonValue` tool choice, `CacheRetention`, and session ID. |
| `pi_ai::OpenAiResponsesReasoningSummary` | `crates/pi-ai/src/openai_responses.rs:102` | data enum | shared | extended | Auto, detailed, or concise summary nested under a double `Option`. |
| `pi_ai::OpenAiResponsesSimplePatch` | `crates/pi-ai/src/openai_responses.rs:176` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Optional summary and service-tier override. |
| `pi_ai::OpenAiCodexResponses` | `crates/pi-ai/src/openai_responses.rs:267` | API-family marker / generic type argument | Send/shared | extended | Selects Codex Responses associated types at `crates/pi-ai/src/openai_responses.rs:304`. |
| `pi_ai::OpenAiCodexResponsesOptions` | `crates/pi-ai/src/openai_responses.rs:185` | data struct (`ApiFamily::FullOptions`) | shared | extended | Temperature, effort, tri-state `OpenAiCodexReasoningSummary`, service tier, `OpenAiTextVerbosity`, `OpenAiCodexToolChoice`, `CacheRetention`, and session ID. |
| `pi_ai::OpenAiCodexReasoningSummary` | `crates/pi-ai/src/openai_responses.rs:124` | data enum | shared | extended | Auto, concise, detailed, off, or on summary state. |
| `pi_ai::OpenAiTextVerbosity` | `crates/pi-ai/src/openai_responses.rs:219` | data enum | shared | extended | Low, medium, or high Codex output verbosity. |
| `pi_ai::OpenAiCodexToolChoice` | `crates/pi-ai/src/openai_responses.rs:241` | data enum | shared | extended | Auto, none, or required Codex tool selection. |
| `pi_ai::OpenAiCodexResponsesSimplePatch` | `crates/pi-ai/src/openai_responses.rs:207` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Optional summary, service tier, and verbosity overrides. |
| `pi_ai::AnthropicMessages` | `crates/pi-ai/src/anthropic_messages.rs:32` | API-family marker / generic type argument | Send/shared | extended | Selects Anthropic associated types at `crates/pi-ai/src/anthropic_messages.rs:113`. |
| `pi_ai::AnthropicOptions` | `crates/pi-ai/src/anthropic_messages.rs:82` | data struct (`ApiFamily::FullOptions`) | shared | extended | Required max tokens, temperature, `AnthropicThinking`, `AnthropicThinkingDisplay`, optional `AnthropicToolChoice`, `CacheRetention`, metadata user ID, and interleaved-thinking flag. |
| `pi_ai::AnthropicThinking` | `crates/pi-ai/src/anthropic_messages.rs:47` | data enum | shared | extended | Omitted, disabled, adaptive with optional `AnthropicEffort`, or fixed token budget. |
| `pi_ai::AnthropicThinkingDisplay` | `crates/pi-ai/src/anthropic_messages.rs:37` | data enum | shared | extended | Summarized or omitted display policy. |
| `pi_ai::AnthropicToolChoice` | `crates/pi-ai/src/anthropic_messages.rs:66` | data enum | shared | extended | Auto, any, none, or one named tool. |
| `pi_ai::AnthropicSimplePatch` | `crates/pi-ai/src/anthropic_messages.rs:104` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Optional thinking display, metadata user ID, and interleaved-thinking overrides. |
| `pi_ai::GoogleGenerativeAi` | `crates/pi-ai/src/google.rs:23` | API-family marker / generic type argument | Send/shared | extended | Selects Gemini Developer associated types at `crates/pi-ai/src/google.rs:132`. |
| `pi_ai::GoogleVertex` | `crates/pi-ai/src/google.rs:27` | API-family marker / generic type argument | Send/shared | extended | Selects Vertex associated types at `crates/pi-ai/src/google.rs:175`. |
| `pi_ai::GoogleOptions` | `crates/pi-ai/src/google.rs:86` | data struct (`ApiFamily::FullOptions`) | shared | extended | Output cap, temperature, optional `GoogleThinkingOptions`, and optional `GoogleToolChoice`. |
| `pi_ai::GoogleVertexOptions` | `crates/pi-ai/src/google.rs:103` | data struct (`ApiFamily::FullOptions`) | shared | extended | The `GoogleOptions` fields plus request-scoped project and location. |
| `pi_ai::GoogleThinkingOptions` | `crates/pi-ai/src/google.rs:63` | data struct | shared | extended | Enabled flag, optional signed token budget, and optional `GoogleThinkingLevel`. |
| `pi_ai::GoogleThinkingLevel` | `crates/pi-ai/src/google.rs:32` | data enum | shared | extended | Unspecified, minimal, low, medium, or high provider-native level. |
| `pi_ai::GoogleToolChoice` | `crates/pi-ai/src/google.rs:75` | data enum | shared | extended | Auto, none, or any function-call mode. |
| `pi_ai::GoogleSimplePatch` | `crates/pi-ai/src/google.rs:121` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Optional native Google tool-choice override shared by both Google families. |
| `pi_ai::GoogleCompat` | `crates/pi-ai/src/google.rs:130` | empty compatibility struct | shared | extended | Concrete `ApiFamily::Compat` for both Google families. |
| `pi_ai::BedrockConverseStream` | `crates/pi-ai/src/bedrock.rs:50` | API-family marker / generic type argument | Send/shared | extended | Selects Bedrock associated types at `crates/pi-ai/src/bedrock.rs:195`. |
| `pi_ai::BedrockOptions` | `crates/pi-ai/src/bedrock.rs:134` | data struct (`ApiFamily::FullOptions`) | shared | extended | Region, profile, `SecretString` bearer token, output cap, temperature, optional `BedrockToolChoice`, optional `ReasoningLevel` and `ThinkingBudgets`, interleaving/display/cache controls, `BedrockProviderEnvironment`, and optional ordered request metadata. |
| `pi_ai::BedrockToolChoice` | `crates/pi-ai/src/bedrock.rs:65` | data enum | shared | extended | Auto, any, none, or one named tool. |
| `pi_ai::BedrockThinkingDisplay` | `crates/pi-ai/src/bedrock.rs:55` | data enum | shared | extended | Summarized or omitted reasoning display. |
| `pi_ai::BedrockProviderEnvironment` | `crates/pi-ai/src/bedrock.rs:172` | public, doc-hidden scratch struct | shared | exclude | Direct public field of `BedrockOptions`, but described by the source as provider-leaf scratch request state. Exposing it as app configuration would leak provider assembly internals. |
| `pi_ai::BedrockSimplePatch` | `crates/pi-ai/src/bedrock.rs:184` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Optional native tool choice, interleaved-thinking, display, and ordered request-metadata overrides. |
| `pi_ai::MistralConversations` | `crates/pi-ai/src/mistral.rs:21` | API-family marker / generic type argument | Send/shared | extended | Selects Mistral associated types at `crates/pi-ai/src/mistral.rs:151`. |
| `pi_ai::MistralOptions` | `crates/pi-ai/src/mistral.rs:126` | data struct (`ApiFamily::FullOptions`) | shared | extended | Temperature, output cap, optional `MistralToolChoice`, prompt mode, reasoning effort, optional `CacheRetention`, and session ID. |
| `pi_ai::MistralToolChoice` | `crates/pi-ai/src/mistral.rs:107` | data enum | shared | extended | Auto, none, any, required, or one named function. |
| `pi_ai::MistralSimplePatch` | `crates/pi-ai/src/mistral.rs:146` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Optional native Mistral tool-choice override. |
| `pi_ai::MistralCompat` | `crates/pi-ai/src/mistral.rs:102` | empty compatibility struct | shared | extended | Concrete `ApiFamily::Compat` for Mistral. |
| `pi_ai_openai::AzureOpenAiResponses` | `providers/pi-ai-openai/src/azure.rs:14`, `providers/pi-ai-openai/src/azure.rs:54` | API-family marker / generic type argument | Send/shared | extended | Public provider-crate family. Its five associated types are `OpenAiResponsesCompat`, `CustomApiModelConfig`, `AzureOpenAiResponsesOptions`, `AzureOpenAiResponsesSimplePatch`, and `OrderedJsonObject` (`providers/pi-ai-openai/src/azure.rs:57`, `providers/pi-ai-openai/src/azure.rs:58`, `providers/pi-ai-openai/src/azure.rs:59`, `providers/pi-ai-openai/src/azure.rs:60`, `providers/pi-ai-openai/src/azure.rs:61`). |
| `pi_ai_openai::AzureOpenAiResponsesModelConfig` / `pi_ai_openai::azure_model_config` | `providers/pi-ai-openai/src/azure.rs:18`, `providers/pi-ai-openai/src/azure.rs:20`, `providers/pi-ai-openai/src/azure.rs:195`, `providers/pi-ai-openai/src/azure.rs:210` | data struct nested in opaque model config / fallible decoder | shared | extended | The associated `ModelConfig` is `CustomApiModelConfig`; its raw JSON is decoded as this public struct, whose only field is `OpenAiResponsesModelConfig`. That shared type reaches `OpenAiResponsesCompat`, `ThinkingLevelMap<OpenAiThinkingValue>`, and `OrderedJsonObject` (`crates/pi-ai/src/model.rs:173`). |
| `pi_ai_openai::AzureOpenAiResponsesOptions` | `providers/pi-ai-openai/src/azure.rs:25` | data struct (`ApiFamily::FullOptions`) | shared | extended | Contains nested `OpenAiResponsesOptions`, optional `url::Url`, optional resource name, deployment name, and API-version strings. `OpenAiResponsesOptions` in turn reaches `OrderedJsonObject`, `OrderedJsonValue`, `OpenAiResponsesReasoningSummary`, and `CacheRetention` (`crates/pi-ai/src/openai_responses.rs:151`). |
| `pi_ai_openai::AzureOpenAiResponsesSimplePatch` | `providers/pi-ai-openai/src/azure.rs:41` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Contains optional `OpenAiResponsesReasoningSummary`, optional `url::Url`, and optional resource, deployment, and API-version strings. |
| `pi_ai::{OpenAiResponsesCompat,SessionAffinityFormat,ExtensionMap,VersionedExtension,OrderedJsonObject,OrderedJsonString,OrderedJsonArray,OrderedJsonValue}` | `crates/pi-ai/src/model.rs:600`, `crates/pi-ai/src/model.rs:584`, `crates/pi-ai/src/model.rs:25`, `crates/pi-ai/src/model.rs:919`, `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:24`, `crates/pi-ai/src/json_compat.rs:227`, `crates/pi-ai/src/json_compat.rs:324` | Azure nested compatibility and wire data graph | shared | extended | The Azure `Compat` branch reaches `SessionAffinityFormat` and `ExtensionMap`, whose `VersionedExtension` values carry raw JSON. The wire branch reaches exact `OrderedJsonString` keys/strings, `OrderedJsonArray`, and recursive `OrderedJsonValue`. |
| `pi_ai_pi_messages::PiMessages` | `providers/pi-ai-pi-messages/src/wire.rs:15`, `providers/pi-ai-pi-messages/src/wire.rs:68` | API-family marker / generic type argument | Send/shared | extended | Public provider-crate family. Its five associated types are `PiMessagesCompat`, `CustomApiModelConfig`, `PiMessagesOptions`, `PiMessagesSimplePatch`, and `OrderedJsonObject` (`providers/pi-ai-pi-messages/src/wire.rs:71`, `providers/pi-ai-pi-messages/src/wire.rs:72`, `providers/pi-ai-pi-messages/src/wire.rs:73`, `providers/pi-ai-pi-messages/src/wire.rs:74`, `providers/pi-ai-pi-messages/src/wire.rs:75`). |
| `pi_ai_pi_messages::PiMessagesCompat` | `providers/pi-ai-pi-messages/src/wire.rs:19` | empty data struct (`ApiFamily::Compat`) | shared | extended | No nested fields. |
| `pi_ai::CustomApiModelConfig` as `PiMessages::ModelConfig` | `crates/pi-ai/src/model.rs:741`, `crates/pi-ai/src/model.rs:743`, `crates/pi-ai/src/model.rs:745`, `crates/pi-ai/src/model.rs:747`, `providers/pi-ai-pi-messages/src/handler.rs:375`, `providers/pi-ai-pi-messages/src/handler.rs:378`, `providers/pi-ai-pi-messages/src/handler.rs:383` | opaque-JSON data struct (`ApiFamily::ModelConfig`) | shared | extended | Contains `ApiId`, schema version, and exact `Box<RawValue>`. The provider's typed-model conversion validates the API ID and otherwise retains the whole `CustomApiModelConfig`; no additional named model-config type is decoded. |
| `pi_ai_pi_messages::PiMessagesOptions` | `providers/pi-ai-pi-messages/src/wire.rs:41` | data struct (`ApiFamily::FullOptions`) | shared | extended | Primitive fields plus nested `ReasoningLevel`, `CacheRetention`, and optional `PiMessagesToolChoice`. |
| `pi_ai_pi_messages::PiMessagesSimplePatch` | `providers/pi-ai-pi-messages/src/wire.rs:61` | data struct (`ApiFamily::OptionsPatch`) | shared | extended | Optional `PiMessagesToolChoice` plus a boolean debug flag. |
| `pi_ai_pi_messages::PiMessagesToolChoice` | `providers/pi-ai-pi-messages/src/wire.rs:24` | data enum | shared | extended | Auto, none, required, or a named function; nested by both Pi Messages option records. |
| `pi_ai::{OrderedJsonObject,OrderedJsonString,OrderedJsonArray,OrderedJsonValue}` as `PiMessages::WireRequest` graph | `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:24`, `crates/pi-ai/src/json_compat.rs:227`, `crates/pi-ai/src/json_compat.rs:324` | recursive ordered JSON graph (`ApiFamily::WireRequest`) | shared | extended | `OrderedJsonObject` transitively contains exact UTF-16 keys/strings, ordered arrays, and recursive `OrderedJsonValue`. |
| `pi_ai::ApiRequestOptions` | `crates/pi-ai/src/options.rs:450` | transport-options data struct | shared | extended | Retry count/delay, timeout, optional `StreamTransport`, WebSocket connect timeout, session ID, and `HeaderMapSpec`; passed separately by `stream_api_with_request_options`. |
| `pi_ai::StreamTransport` | `crates/pi-ai/src/options.rs:510` | data enum | shared | extended | SSE, WebSocket, cached WebSocket, or auto transport preference. |
| `pi_ai::{CacheRetention,ReasoningLevel,ThinkingBudgets}` | `crates/pi-ai/src/options.rs:136`, `crates/pi-ai/src/options.rs:19`, `crates/pi-ai/src/options.rs:88` | shared option enums/data | shared | extended | Named nested leaves reused by multiple full-options records. |
| `pi_ai::{OrderedJsonObject,OrderedJsonArray,OrderedJsonValue}` | `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:227`, `crates/pi-ai/src/json_compat.rs:324` | recursive ordered JSON graph | shared | extended | Sampling, native tool-choice, and allowed-tool values in the OpenAI families. |
| `pi_ai::SecretString` | `crates/pi-ai/src/auth.rs:27` | secret wrapper | shared | extended | Nested Bedrock bearer-token carrier. |
| `indexmap::IndexMap<String,String>` | `crates/pi-ai/src/bedrock.rs:166` | external ordered map | shared | extended | Bedrock request metadata in full options and the simple patch. |

This table exhausts the repository-defined non-test public `ApiFamily`
markers, including the two implementations defined outside `pi-ai`.
`ApiModelConfig::Custom` stores only an open API ID, schema version, and opaque
configuration (`crates/pi-ai/src/model.rs:137`); `ApiFamily` leaves
`FullOptions`, `OptionsPatch`, and `WireRequest` as implementation-chosen
associated types (`crates/pi-ai/src/options.rs:368`). No additional concrete
custom-family full-options type was observed in this repository.

##### Public provider-family siblings outside the options graphs

The provider-family modules also expose the following replay/handoff helpers.
They are not fields or associated types in either configuration graph above,
and observed non-test uses are in provider implementations rather than in the
agent application's model-call surface.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::OpenAiResponsesReplay` | `crates/pi-ai/src/openai_responses.rs:38` | provider replay data enum | shared | exclude | Typed internal view used while encoding/decoding the three OpenAI Responses opaque replay records; canonical app-visible replay remains `ReplayItem`/`OpaquePayload`. |
| `pi_ai::{OpenAiMessagePhase,OpenAiToolItemType}` | `crates/pi-ai/src/openai_responses.rs:73`, `crates/pi-ai/src/openai_responses.rs:92` | provider replay data enums | shared | exclude | Nested typed leaves of `OpenAiResponsesReplay`; the OpenAI provider decoder consumes them at `providers/pi-ai-openai/src/responses_decoder.rs:8`. |
| `pi_ai::{OpenAiCompletionsHandoff,OpenAiResponsesHandoff,AnthropicMessagesHandoff,GoogleHandoff,BedrockHandoff,MistralConversationsHandoff}` | `crates/pi-ai/src/openai_completions.rs:2151`, `crates/pi-ai/src/openai_responses.rs:1761`, `crates/pi-ai/src/anthropic_messages.rs:1211`, `crates/pi-ai/src/google.rs:1049`, `crates/pi-ai/src/bedrock.rs:1004`, `crates/pi-ai/src/mistral.rs:78` | provider-family `ApiFamilyHandoff` implementations | shared | exclude | Provider-side canonical-context projection hooks. Observed production uses are inside provider handlers, for example `providers/pi-ai-openai/src/handler.rs:271`, `providers/pi-ai-google/src/handler.rs:476`, and `providers/pi-ai-mistral/src/handler.rs:307`. |
| `pi_ai::{AnthropicToolCallIdPolicy,GoogleToolCallIdPolicy,BedrockToolCallIdPolicy,MistralToolCallIdPolicy}` | `crates/pi-ai/src/anthropic_messages.rs:1184`, `crates/pi-ai/src/google.rs:1013`, `crates/pi-ai/src/bedrock.rs:971`, `crates/pi-ai/src/mistral.rs:29` | provider-family `ToolCallIdPolicy` implementations | Send/shared | exclude | Provider-specific target-ID normalizers selected by the handoff hooks; Mistral's implementation contains request-order state behind a mutex. OpenAI uses its handoff structs themselves as the policies (`crates/pi-ai/src/openai_completions.rs:2174`, `crates/pi-ai/src/openai_responses.rs:1786`). |

### Cancellation and deferred values

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::CancellationToken` / `CancellationToken::new` | `crates/pi-ai/src/cancellation.rs:28`, `crates/pi-ai/src/cancellation.rs:47` | cloneable capability / constructor | shared, thread-safe | core | Propagates cancellation into model, tool, policy, and sink calls; the session-storage traits do not accept it. |
| `CancellationToken::{cancel,is_cancelled,check,child}` | `crates/pi-ai/src/cancellation.rs:62`, `crates/pi-ai/src/cancellation.rs:67`, `crates/pi-ai/src/cancellation.rs:72`, `crates/pi-ai/src/cancellation.rs:90` | sync methods | shared, thread-safe | core | State/control operations. |
| `CancellationToken::cancelled` / `pi_ai::Cancelled<'a>` | `crates/pi-ai/src/cancellation.rs:81`, `crates/pi-ai/src/cancellation.rs:115` | borrowed future constructor / future struct | shared, thread-safe | extended | Awaitable cancellation view. |
| `pi_ai::CancellationError` | `crates/pi-ai/src/cancellation.rs:12` | error unit struct | shared | core | Result from `check`. |
| `pi_ai::DeferredHandle` / `{new,model_ref}` | `crates/pi-ai/src/deferred.rs:19`, `crates/pi-ai/src/deferred.rs:44`, `crates/pi-ai/src/deferred.rs:63` | durable data struct / generic constructor / getter | shared | extended | Provider token, model/API identity, expiry, polling hint, and provider JSON data. Also nested in `AssistantMessage`. |
| `pi_ai::{DeferredCapabilities,DeferredWindow,DeferredSubmission,DeferredFetchOptions,DeferredCancelOptions}` | `crates/pi-ai/src/deferred.rs:73`, `crates/pi-ai/src/deferred.rs:102`, `crates/pi-ai/src/deferred.rs:116`, `crates/pi-ai/src/deferred.rs:178`, `crates/pi-ai/src/deferred.rs:188` | data structs/enums/type alias | shared | extended | Deferred request/capability graph. |
| `pi_ai::DeferredModelRuntime` | `crates/pi-ai/src/deferred.rs:195` | object-safe async supertrait | Send | extended | Optional library-implemented fetch/cancel capability; `Models` implements it. |
| `pi_ai::LocalDeferredModelRuntime` | `crates/pi-ai/src/deferred.rs:218` | Local async supertrait | Local | exclude | Local counterpart. |

### Canonical message and event graph

Every item in this table is owned data unless the row says otherwise.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_ai::Message` / `Message::id` | `crates/pi-ai/src/messages.rs:32`, `crates/pi-ai/src/messages.rs:105` | large data enum / borrowed getter | shared | core | User, Assistant, and ToolResult variants. |
| `pi_ai::UserMessage` | `crates/pi-ai/src/messages.rs:116` | data struct | shared | core | ID, ordered `ContentBlock` values, timestamp. |
| `pi_ai::AssistantMessage` | `crates/pi-ai/src/messages.rs:128` | large data struct | shared | core | Provider/API/model identity, diagnostics, content, replay, usage/cost, finish, timestamp, optional `DeferredHandle`. |
| `pi_ai::ToolResultMessage` | `crates/pi-ai/src/messages.rs:222` | data struct | shared | core | Call identity, content, details, usage, tool additions, error flag, timestamp. |
| `pi_ai::ContentBlock` / `ContentBlock::id` | `crates/pi-ai/src/messages.rs:247`, `crates/pi-ai/src/messages.rs:288` | large data enum / borrowed getter | shared | core | Text, Image, Thinking, or ToolCall; variants carry heterogeneous fields. |
| `pi_ai::ToolCall` | `crates/pi-ai/src/messages.rs:300` | data struct | shared | core | Stable ID, name, `serde_json::Value` arguments. |
| `pi_ai::ToolResultContent` | `crates/pi-ai/src/messages.rs:423` | data enum | shared | core | Text or image tool output. |
| `pi_ai::Conversation` / `Conversation::new` | `crates/pi-ai/src/messages.rs:444`, `crates/pi-ai/src/messages.rs:455` | data struct / constructor | shared | extended | Versioned standalone durable conversation. Agent state stores `AgentRecord` instead. |
| `pi_ai::Context` / `Context::new` | `crates/pi-ai/src/messages.rs:467`, `crates/pi-ai/src/messages.rs:480` | data struct / constructor | shared | core | Canonical system prompt, messages, and `ToolSpec` list sent via `ModelRequest`. |
| `pi_ai::AssistantFinish` / `AssistantFinishReason` | `crates/pi-ai/src/messages.rs:492`, `crates/pi-ai/src/messages.rs:504` | data struct / enum | shared | core | Stop, length, tool use, deferred, error, or aborted terminal metadata. |
| `pi_ai::PublicError` | `crates/pi-ai/src/messages.rs:521` | error data struct | shared | core | Sanitized operational failure stored in messages/outcomes. |
| `DiagnosticErrorCode`, `DiagnosticErrorInfo`, `AssistantMessageDiagnostic` | `crates/pi-ai/src/messages.rs:178`, `crates/pi-ai/src/messages.rs:187`, `crates/pi-ai/src/messages.rs:204` | data enum/structs | shared | extended | Persisted provider/runtime diagnostics nested in assistant data and events. |
| `pi_ai::ContentBlockKind` | `crates/pi-ai/src/streaming.rs:347` | data enum | shared | core | Text, Thinking, ToolCall streaming kind. |
| `pi_ai::AssistantEvent` | `crates/pi-ai/src/streaming.rs:360` | non-exhaustive large data enum | shared | core | Twenty-one event forms: message start, response metadata, content start/deltas/replacements, diagnostics, tool metadata/arguments, replay lifecycle/data, usage, and three terminal variants. |
| `AssistantEvent::{is_terminal,terminal_message}` | `crates/pi-ai/src/streaming.rs:542`, `crates/pi-ai/src/streaming.rs:550` | sync inspection methods | shared | core | Terminal stream handling. |
| `pi_ai::ReplayDataOperation` | `crates/pi-ai/src/streaming.rs:563` | data enum | shared | core | UTF-8, byte, or JSON-byte replace/append operations. |
| `pi_ai::CancellationReason` / `{new,with_request_id}` | `crates/pi-ai/src/streaming.rs:579`, `crates/pi-ai/src/streaming.rs:588`, `crates/pi-ai/src/streaming.rs:596` | data struct / generic builders | shared | core | Portable terminal cancellation reason. |
| `pi_ai::AssistantMessageSnapshot` | `crates/pi-ai/src/streaming.rs:1674` | large data struct | shared | core | Scratch-free partial/terminal assistant observation nested in `AgentSnapshot`. |
| `pi_ai::AssistantAssembler` / `AssemblyError` | `crates/pi-ai/src/streaming.rs:800`, `crates/pi-ai/src/streaming.rs:1710` | mutable protocol reducer / large error enum | shared | exclude | Library stream-assembly machinery used by the agent/runtime, not ordinary embedding control. |

Transitive value types that must remain available with the above graph:

| Item paths | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `ProviderId`, `ModelId`, `ApiId`, `MessageId`, `ContentBlockId`, `ToolCallId`, `ReplayItemId`, `ReplayKind`, `ExtensionId`, `RunId` | `crates/pi-ai/src/ids.rs:58`, `crates/pi-ai/src/ids.rs:62`, `crates/pi-ai/src/ids.rs:66`, `crates/pi-ai/src/ids.rs:78`, `crates/pi-ai/src/ids.rs:82`, `crates/pi-ai/src/ids.rs:86`, `crates/pi-ai/src/ids.rs:90`, `crates/pi-ai/src/ids.rs:70`, `crates/pi-ai/src/ids.rs:74`, `crates/pi-ai/src/ids.rs:94` | open string newtypes with generic `new`, borrowed `as_str`, consuming `into_inner` | shared | core | Stable identities throughout requests, events, messages, replay, tools, and extensions. |
| `ModelRef` / `ModelRef::new` | `crates/pi-ai/src/ids.rs:105`, `crates/pi-ai/src/ids.rs:114` | data struct / generic constructor | shared | core | Provider/model lookup identity. |
| `Timestamp` / `{from_unix_millis,unix_millis}` | `crates/pi-ai/src/ids.rs:133`, `crates/pi-ai/src/ids.rs:140`, `crates/pi-ai/src/ids.rs:145` | numeric newtype / methods | shared | core | Unix milliseconds in messages, deferred handles, and sessions. |
| `UsageSource`, `Usage` / `Usage::{zero,request_input_tokens,total_tokens}` | `crates/pi-ai/src/usage.rs:13`, `crates/pi-ai/src/usage.rs:26`, `crates/pi-ai/src/usage.rs:51`, `crates/pi-ai/src/usage.rs:67`, `crates/pi-ai/src/usage.rs:77` | enum / data struct / methods | shared | core | Cumulative token totals, provenance, construction, and inspection. |
| `Currency`, `Cost` / `Currency::{new,as_str,usd}` | `crates/pi-ai/src/usage.rs:88`, `crates/pi-ai/src/usage.rs:119`, `crates/pi-ai/src/usage.rs:95`, `crates/pi-ai/src/usage.rs:100`, `crates/pi-ai/src/usage.rs:105` | newtype / data struct / methods | shared | core | Fixed-point monetary result nested in messages/outcomes. |
| `ReplayEnvelope`, `ReplayScope`, `ReplayItem`, `ReplayTarget`, `ReplayApplicability`, `ReplayCompleteness`, `OpaquePayload`, `OpaquePayloadEncodingError` | `crates/pi-ai/src/replay.rs:13`, `crates/pi-ai/src/replay.rs:88`, `crates/pi-ai/src/replay.rs:135`, `crates/pi-ai/src/replay.rs:179`, `crates/pi-ai/src/replay.rs:254`, `crates/pi-ai/src/replay.rs:266`, `crates/pi-ai/src/replay.rs:276`, `crates/pi-ai/src/replay.rs:371` | replay data/error graph including large enums and bytes | shared | core | Lossless provider replay nested in assistant messages/events; `ReplayItem::json_bytes` and `OpaquePayload::json_bytes` expose the encoding error. |
| `ModelFingerprint`, `ReplayDropReason`, `HandoffChange`, `HandoffReport` | `crates/pi-ai/src/handoff.rs:17`, `crates/pi-ai/src/handoff.rs:44`, `crates/pi-ai/src/handoff.rs:65`, `crates/pi-ai/src/handoff.rs:144` | data structs/open string newtype/large enum | shared | core | Complete `AgentEvent::ContextPrepared` graph. `HandoffChange::OpaqueReplayDropped` carries `ReplayDropReason`. |
| `ReasoningLevel` | `crates/pi-ai/src/options.rs:19` | data enum | shared | core | Agent-state and request-level reasoning choice. |
| `SimpleGenerationOptions` | `crates/pi-ai/src/options.rs:561` | large data struct | shared | core | Agent configuration and `ModelRequest` option carrier. |
| `ReasoningFallback`, `ThinkingBudgets`, `CacheRetention`, `ToolChoice`, `StreamTransport`, `ApiRequestOptions` | `crates/pi-ai/src/options.rs:77`, `crates/pi-ai/src/options.rs:88`, `crates/pi-ai/src/options.rs:136`, `crates/pi-ai/src/options.rs:149`, `crates/pi-ai/src/options.rs:510`, `crates/pi-ai/src/options.rs:450` | option data graph | shared | extended | Exact typed fields nested in simple/deferred options; an app may leave them at defaults but they are part of the native request shape. |
| `OrderedJsonObject`, `OrderedJsonString`, `OrderedJsonValue`, `OrderedJsonArray` | `crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:24`, `crates/pi-ai/src/json_compat.rs:324`, `crates/pi-ai/src/json_compat.rs:227` | ordered map/string/large recursive enum/array data graph with generic mutators and iterators | shared | extended | Complete graph of `SimpleGenerationOptions::sampling` (`crates/pi-ai/src/options.rs:598`); values can recursively contain ordered objects and arrays. |
| `HeaderMapSpec` | `crates/pi-ai/src/model.rs:17` | map alias `BTreeMap<String, Option<String>>` | shared | extended | Complete graph of `SimpleGenerationOptions::headers` (`crates/pi-ai/src/options.rs:602`) and `ApiRequestOptions::headers`; `None` is an explicit deletion marker. |
| `ErasedApiOptionsPatch` | `crates/pi-ai/src/options.rs:294` | data struct containing raw JSON | shared | extended | Sole dynamic API-family patch nested in `SimpleGenerationOptions`. |
| `VersionedExtension` | `crates/pi-ai/src/model.rs:919` | data struct containing JSON | shared | extended | Custom/session metadata and tool-result details. |

## `pi_agent_session`: durable storage boundary

Session storage is adjacent rather than a constructor parameter of the current
`Agent` or `TokioAgentHandle`: their constructors take runtime/state/tools and
Agent respectively (`crates/pi-agent-core/src/run.rs:140`,
`crates/pi-agent-runtime-tokio/src/lib.rs:168`). It is nevertheless an explicit
host boundary requested for this inventory.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_session::SessionStorage` | `crates/pi-agent-session/src/storage.rs:18` | async trait host implements | Send | extended | Object-safe storage capability; every operation returns `SendBoxFuture` and the complete method/value graph is itemized below. |
| `pi_agent_session::SessionStorage::metadata` | `crates/pi-agent-session/src/storage.rs:20` | async trait method host implements | Send | extended | Returns `SessionMetadata` in `SendBoxFuture`. |
| `pi_agent_session::SessionStorage::load_state` | `crates/pi-agent-session/src/storage.rs:23` | async trait method host implements | Send | extended | Returns complete derived `SessionState`. |
| `pi_agent_session::SessionStorage::append` | `crates/pi-agent-session/src/storage.rs:26` | async trait method host implements | Send | extended | Accepts expected `Sequence` plus `Vec<SessionMutation>` and returns `AppendReceipt`. |
| `pi_agent_session::SessionStorage::log` | `crates/pi-agent-session/src/storage.rs:33` | async trait method host implements | Send | extended | Returns accepted `SessionMutation` values after an optional sequence bound and limit. |
| `pi_agent_session::SessionStorage::repair_tail` | `crates/pi-agent-session/src/storage.rs:40` | async trait method host implements | Send | extended | Returns `TailRepairReport`. |
| `pi_agent_session::SessionRepository` | `crates/pi-agent-session/src/storage.rs:70` | async trait host implements | Send | extended | Object-safe session factory/index capability; methods return storage trait objects and are itemized below. |
| `pi_agent_session::SessionRepository::create` | `crates/pi-agent-session/src/storage.rs:72` | async trait method host implements | Send | extended | Accepts `CreateSessionRequest`, returns `Arc<dyn SessionStorage>`. |
| `pi_agent_session::SessionRepository::open` | `crates/pi-agent-session/src/storage.rs:78` | async trait method host implements | Send | extended | Accepts borrowed `SessionId`, returns `Arc<dyn SessionStorage>`. |
| `pi_agent_session::SessionRepository::fork` | `crates/pi-agent-session/src/storage.rs:84` | async trait method host implements | Send | extended | Accepts source ID and `ForkRequest`, returns `Arc<dyn SessionStorage>`. |
| `pi_agent_session::SessionRepository::list` | `crates/pi-agent-session/src/storage.rs:91` | async trait method host implements | Send | extended | Accepts `SessionQuery`, returns `Vec<SessionMetadata>`. |
| `pi_agent_session::InMemorySessionStorage` / `{new,state_snapshot,append_batch,metadata_snapshot,log_snapshot}` | `crates/pi-agent-session/src/storage.rs:126`, `crates/pi-agent-session/src/storage.rs:133`, `crates/pi-agent-session/src/storage.rs:150`, `crates/pi-agent-session/src/storage.rs:155`, `crates/pi-agent-session/src/storage.rs:192`, `crates/pi-agent-session/src/storage.rs:205` | library backend / sync methods | Send + Local implementation | extended | Built-in process-local backend. |
| `pi_agent_session::InMemorySessionRepository` / `new` | `crates/pi-agent-session/src/storage.rs:307`, `crates/pi-agent-session/src/storage.rs:313` | library repository / constructor | Send | extended | Process-local repository returning trait objects. |
| `LocalSessionStorage`, `LocalSessionRepository`, `LocalInMemorySessionRepository` | `crates/pi-agent-session/src/storage.rs:44`, `crates/pi-agent-session/src/storage.rs:98`, `crates/pi-agent-session/src/storage.rs:446` | Local traits/backend | Local | exclude | Alternate Local family. The in-memory storage implements both families, verified at `crates/pi-agent-session/tests/m7_1_session.rs:1373`. |
| `SessionErrorKind`, `SessionError`, `SessionReductionError` | `crates/pi-agent-session/src/error.rs:10`, `crates/pi-agent-session/src/error.rs:31`, `crates/pi-agent-session/src/error.rs:77` | error enum/struct/non-exhaustive reducer error enum | shared | extended | Storage, repository, optimistic-sequence, and exact reducer-integrity failures. |
| `SessionId`, `EntryId`, `LaneName`, `OperationRecordId`, `Sequence` | `crates/pi-agent-session/src/ids.rs:58`, `crates/pi-agent-session/src/ids.rs:59`, `crates/pi-agent-session/src/ids.rs:60`, `crates/pi-agent-session/src/ids.rs:61`, `crates/pi-agent-session/src/ids.rs:68` | open string/numeric newtypes | shared | extended | Trait input/output identities and optimistic sequence. |
| `SessionMetadata`, `AppendReceipt`, `TailRepairReport` | `crates/pi-agent-session/src/types.rs:879`, `crates/pi-agent-session/src/types.rs:896`, `crates/pi-agent-session/src/types.rs:909` | data structs | shared | extended | Direct `SessionStorage` results. |
| `SessionHeader`, `SessionEnvironmentMetadata` | `crates/pi-agent-session/src/types.rs:52`, `crates/pi-agent-session/src/types.rs:30` | data structs | shared | extended | In-memory/backend creation and metadata. |
| `CreateSessionRequest`, `ForkRequest`, `SessionQuery`, `ForkPosition` | `crates/pi-agent-session/src/types.rs:922`, `crates/pi-agent-session/src/types.rs:962`, `crates/pi-agent-session/src/types.rs:975`, `crates/pi-agent-session/src/types.rs:826` | request structs / enum | shared | extended | `SessionRepository` inputs. |

### Session state API

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_session::SessionReducer::{apply,state}` | `crates/pi-agent-session/src/reducer.rs:13`, `crates/pi-agent-session/src/reducer.rs:15`, `crates/pi-agent-session/src/reducer.rs:18` | sync reducer trait implemented by library state | shared | extended | Applies authoritative mutations and exposes derived state. |
| `pi_agent_session::SessionState` / `SessionState::new` | `crates/pi-agent-session/src/reducer.rs:23`, `crates/pi-agent-session/src/reducer.rs:72` | large derived-state struct / constructor | shared | extended | Direct `load_state` result containing entries, operational records, lanes, facts, statistics, recovery indexes, and the accepted log. |
| `SessionState::replay` | `crates/pi-agent-session/src/reducer.rs:77` | iterator-generic sync constructor | shared | extended | Rebuilds state from owned `SessionMutation` values. |
| `SessionState::{sequence,next_sequence}` | `crates/pi-agent-session/src/reducer.rs:88`, `crates/pi-agent-session/src/reducer.rs:93` | sync getters | shared | extended | Current and required-next global sequence. |
| `SessionState::{entry,entries_in_sequence_order,records_in_sequence_order}` | `crates/pi-agent-session/src/reducer.rs:100`, `crates/pi-agent-session/src/reducer.rs:105`, `crates/pi-agent-session/src/reducer.rs:113` | borrowed/collection getters | shared | extended | Entry and operation-record projection APIs. |
| `SessionState::{lanes,lane_leaf}` | `crates/pi-agent-session/src/reducer.rs:118`, `crates/pi-agent-session/src/reducer.rs:131` | owned/borrowed getters | shared | extended | Durable lane state and leaf lookup. |
| `SessionState::{log,name,label,labels,stats}` | `crates/pi-agent-session/src/reducer.rs:136`, `crates/pi-agent-session/src/reducer.rs:141`, `crates/pi-agent-session/src/reducer.rs:146`, `crates/pi-agent-session/src/reducer.rs:151`, `crates/pi-agent-session/src/reducer.rs:156` | borrowed getters | shared | extended | Accepted mutation log, latest facts, and derived ledger statistics. |
| `SessionState::{scan_branch_leaf_to_root,scan_branch_root_to_leaf}` | `crates/pi-agent-session/src/reducer.rs:161`, `crates/pi-agent-session/src/reducer.rs:187` | fallible sync traversal methods returning borrowed entries | shared | extended | Immutable branch scans in either direction. |
| `SessionState::{open_operations,recovery_decision}` | `crates/pi-agent-session/src/reducer.rs:197`, `crates/pi-agent-session/src/reducer.rs:206` | borrowed collection/result getter | shared | extended | Crash-recovery view for a selected lane. |
| `SessionState::create_fork_mutations` | `crates/pi-agent-session/src/reducer.rs:254` | fallible sync method | shared | extended | Produces re-sequenced entry/lane/fact mutations for `ForkPosition`. |

### Authoritative mutation and state value graph

Every row is a named public part of `SessionStorage::{append,log}` or
`SessionStorage::load_state`; none is represented only by a source range.

| Item path | Source | Kind | Family | Relevance | Observed role |
|---|---|---|---|---|---|
| `pi_agent_session::SessionMutation` / `SessionMutation::sequence` | `crates/pi-agent-session/src/types.rs:772`, `crates/pi-agent-session/src/types.rs:805` | large data enum / getter | shared | extended | Authoritative log item with `Entry`, `Record`, `Lane`, and `Fact` variants. |
| `pi_agent_session::{EntryBase,SessionEntry}` / `SessionEntry::{base,id,sequence,parent_id,with_base}` | `crates/pi-agent-session/src/types.rs:84`, `crates/pi-agent-session/src/types.rs:102`, `crates/pi-agent-session/src/types.rs:174`, `crates/pi-agent-session/src/types.rs:187`, `crates/pi-agent-session/src/types.rs:192`, `crates/pi-agent-session/src/types.rs:197`, `crates/pi-agent-session/src/types.rs:202` | data struct, large entry enum, methods | shared | extended | Immutable entry graph: message, model/reasoning/tool-set changes, compaction, branch summary, and custom entry. |
| `pi_agent_session::ProvisionedEntry` / `ProvisionedEntry::{id,materialize}` | `crates/pi-agent-session/src/types.rs:223`, `crates/pi-agent-session/src/types.rs:295`, `crates/pi-agent-session/src/types.rs:308` | large data enum / methods | shared | extended | Entry content before lane parent, sequence, and timestamp assignment; nested in operation intent and queue/deferred-write records. |
| `pi_agent_session::OperationRecordBase` | `crates/pi-agent-session/src/types.rs:374` | data struct | shared | extended | Stable ID, sequence, lane, and timestamp shared by operational records. |
| `pi_agent_session::OperationIntent` | `crates/pi-agent-session/src/types.rs:388` | large data enum | shared | extended | Resumable run, compaction, or navigation intent stored in `OperationRecord::Started`. |
| `pi_agent_session::{OperationOutcome,OperationStep,CompactionReason}` | `crates/pi-agent-session/src/types.rs:426`, `crates/pi-agent-session/src/types.rs:440`, `crates/pi-agent-session/src/types.rs:452` | data enums | shared | extended | Terminal classification and step-attempt graph. |
| `pi_agent_session::{ToolCallIdentity,ToolReplayPolicy}` | `crates/pi-agent-session/src/types.rs:463`, `crates/pi-agent-session/src/types.rs:473` | data struct / enum | shared | extended | Stable tool identity and crash-replay rule nested in tool-start records. |
| `pi_agent_session::QueueKind` | `crates/pi-agent-session/src/types.rs:483` | data enum | shared | extended | Durable session input queue (`Steer`, `FollowUp`, `NextRun`); distinct from `pi_agent_core::QueueKind`. |
| `pi_agent_session::{UsageAttribution,SignedUsageAdjustment}` / `UsageAttribution::run_id` | `crates/pi-agent-session/src/types.rs:495`, `crates/pi-agent-session/src/types.rs:569`, `crates/pi-agent-session/src/types.rs:738` | large data enum, adjustment struct, getter | shared | extended | Complete usage-ledger attribution and correction graph. |
| `pi_agent_session::OperationRecord` / `OperationRecord::{base,sequence,lane,run_id}` | `crates/pi-agent-session/src/types.rs:587`, `crates/pi-agent-session/src/types.rs:695`, `crates/pi-agent-session/src/types.rs:710`, `crates/pi-agent-session/src/types.rs:715`, `crates/pi-agent-session/src/types.rs:720` | large data enum / methods | shared | extended | Started, abort-requested, finished, step-attempt, tool-started, queue-enqueued/cancelled, write-deferred, and usage records. |
| `pi_agent_session::SessionFact` | `crates/pi-agent-session/src/types.rs:754` | data enum | shared | extended | Latest-wins session name and entry-label facts. |
| `pi_agent_session::LaneState` | `crates/pi-agent-session/src/types.rs:816` | data struct | shared | extended | Named lane plus current optional leaf. |
| `pi_agent_session::RecoveryDecision` | `crates/pi-agent-session/src/types.rs:838` | large data enum | shared | extended | Idle, resume, abandon, or corrupt recovery result containing operational records and optional public error. |
| `pi_agent_session::SessionStats` | `crates/pi-agent-session/src/types.rs:864` | data struct | shared | extended | Derived message count, token ledger, and per-currency fixed-point costs. |
| `SESSION_HEADER_SCHEMA_VERSION`, `SESSION_STATE_SCHEMA_VERSION`, `SESSION_METADATA_SCHEMA_VERSION`, `APPEND_RECEIPT_SCHEMA_VERSION`, `TAIL_REPAIR_REPORT_SCHEMA_VERSION` | `crates/pi-agent-session/src/types.rs:14`, `crates/pi-agent-session/src/types.rs:17`, `crates/pi-agent-session/src/types.rs:20`, `crates/pi-agent-session/src/types.rs:23`, `crates/pi-agent-session/src/types.rs:26` | constants | shared | extended | Native persistence/version values used by the session data graph. |

## Explicit Rust-side FFI-hard spots

This section records difficult Rust shapes only. It deliberately does not say
whether any binding generator supports them.

| Hard spot | Exact Rust evidence | Direction / consequence for inventory |
|---|---|---|
| Generic typed tools | `TypedTool<I,F>` has two public type parameters (`crates/pi-agent-core/src/tools.rs:277`); `I` must implement deserialization, schema generation, and `Send`, while `F` is a higher-shape async closure returning a boxed future (`crates/pi-agent-core/src/tools.rs:285`, `crates/pi-agent-core/src/tools.rs:332`). | Host -> library. This is core because ordinary Rust apps use it directly (`crates/pi-agent-core/tests/m2_3_tools.rs:607`); replacing it with a hand-written envelope would violate the requested inventory target. |
| Host-implemented tool trait object | `ToolRegistry::register` accepts `Arc<dyn Tool>` (`crates/pi-agent-core/src/tools.rs:502`), and `Tool::execute` receives another trait object, `Arc<dyn ToolUpdateSink>` (`crates/pi-agent-core/src/tools.rs:211`). | `Tool`: host -> library. `ToolUpdateSink`: library -> host callback. Both directions occur in one invocation. |
| Host-implemented session traits | `SessionStorage` returns five boxed futures (`crates/pi-agent-session/src/storage.rs:18`); `SessionRepository` returns `Arc<dyn SessionStorage>` inside boxed futures (`crates/pi-agent-session/src/storage.rs:70`). | Host -> library for a custom backend; library -> host for returned storage capabilities. |
| Library-implemented model runtime | `Agent::new` takes `Arc<dyn ModelRuntime>` (`crates/pi-agent-core/src/run.rs:140`); `Models` implements that trait (`crates/pi-ai/src/models.rs:1399`). | Library object -> Agent. Native Rust can also inject a host implementation, as tests do, but the normal production bridge is `Models`. |
| Event sink callback trait | `AgentEventSink::on_event` takes owned event/token and returns `SendBoxFuture<'static, ()>` (`crates/pi-agent-runtime-tokio/src/lib.rs:45`); subscriptions accept `Arc<dyn AgentEventSink>` (`crates/pi-agent-runtime-tokio/src/lib.rs:306`). | Host -> library callback, with asynchronous acknowledgement as a producer barrier. |
| Nested auth callback traits | `Models::login` accepts `Arc<dyn AuthInteraction>` (`crates/pi-ai/src/models.rs:313`); `AuthInteraction::create_redirect_receiver` asynchronously returns `Box<dyn RedirectReceiver>` (`crates/pi-ai/src/auth.rs:1100`); `RedirectReceiver::receive` consumes that box and returns a `'static` future (`crates/pi-ai/src/auth.rs:1212`). `CredentialStore::acquire_lease` similarly returns `Box<dyn CredentialLease>` (`crates/pi-ai/src/auth.rs:274`). | Host -> library for interaction/store capabilities; nested host objects return across the boundary and are later called or consumed by the library. Login therefore contains two callback-object levels plus challenge/event/error value graphs. |
| Models builder callback traits | `ModelsBuilder` directly accepts `CredentialStore`, `AuthContext`, `ModelsStore`, `ModelOverrideStore`, `HeaderTransform`, generic `PayloadTransform<A>`, `ErasedPayloadTransform`, `ResponseObserver`, and `AttemptMiddleware` trait objects (`crates/pi-ai/src/models.rs:1476`, `crates/pi-ai/src/models.rs:1513`, `crates/pi-ai/src/models.rs:1535`). Several methods pass borrowed contexts and mutable borrowed request/payload values (`crates/pi-ai/src/middleware.rs:340`, `crates/pi-ai/src/middleware.rs:370`, `crates/pi-ai/src/middleware.rs:744`). | Host -> library. This is a heterogeneous callback registry containing synchronous, asynchronous, generic-associated-type, borrowed, and mutable-borrowed signatures. |
| Nested provider-registration callbacks | `ProviderRegistration` stores `Arc<dyn AuthResolver>`, `Arc<dyn ModelCatalog>`, an optional `Arc<ModelAvailabilityFilter>`, `Arc<dyn ChatApi>` values, and `Arc<dyn RetryClassifier>` (`crates/pi-ai/src/provider.rs:2320`). Dynamic catalog construction adds `Arc<dyn ModelCatalogSource>` (`crates/pi-ai/src/provider.rs:2442`). Standard auth composition adds `ApiKeyAuth`, `OAuthAuth`, and `AuthClock` (`crates/pi-ai/src/auth.rs:1728`); standard HTTP composition adds `ErasedApiHandler`, `HttpTransport`, and `RetrySleeper` (`crates/pi-ai/src/provider.rs:959`). | Provider/host -> library control plane. The complete ordinary provider builder is a graph of nested callback objects, not one flat provider record. |
| Generic API-family execution | `Models::stream_api<A: ApiFamily>` accepts `A::FullOptions` (`crates/pi-ai/src/models.rs:785`); `ApiFamily` has five associated types (`crates/pi-ai/src/options.rs:368`); `ModelsBuilder::payload_transform<A>` accepts `Arc<dyn PayloadTransform<A>>` (`crates/pi-ai/src/models.rs:1513`). | Host -> library for typed options/transforms. Concrete surface varies with the selected API-family marker and its associated types. |
| Two executor/carrier boxed future/stream families | `LocalBoxFuture`, `SendBoxFuture`, `LocalBoxStream`, and `SendBoxStream` are four generic trait-object aliases divided by Local versus Send execution (`crates/pi-ai/src/async_types.rs:8`, `crates/pi-ai/src/async_types.rs:11`, `crates/pi-ai/src/async_types.rs:14`, `crates/pi-ai/src/async_types.rs:17`). Send and Local runtimes then return correspondingly different owned assistant-stream wrappers (`crates/pi-ai/src/runtime.rs:87`, `crates/pi-ai/src/runtime.rs:100`). | The Tokio/Swift target needs the Send carrier family; Local counterparts are explicitly inventoried as exclude, not silently conflated. |
| Two event semantics across three app-visible delivery shapes | `AssistantStream` owns a `'static` `AssistantEvent` stream (`crates/pi-ai/src/streaming.rs:1900`). Bare `Agent::run` returns a borrowed `AgentEvent` stream (`crates/pi-agent-core/src/run.rs:283`). `TokioAgentRun` delivers the same `AgentEvent` semantics through `tokio::sync::mpsc::Receiver` and `next_event` (`crates/pi-agent-runtime-tokio/src/lib.rs:126`). | The normal app-facing path is `TokioAgentRun`, while R1 also names the bare agent stream and narrower model-assistant stream as distinct native contracts. |
| Cancellation capability | `CancellationToken` contains shared synchronization state and child propagation (`crates/pi-ai/src/cancellation.rs:28`), while `cancelled()` returns a future borrowing the token (`crates/pi-ai/src/cancellation.rs:81`). | Crosses model, tool, policy, event-sink, Models auth/catalog/deferred, and Tokio-environment operations; synchronous `cancel_now` exists specifically for re-entrant foreign callbacks (`crates/pi-agent-runtime-tokio/src/lib.rs:301`). |
| Large heterogeneous enums | `AssistantEvent` has twenty-one forms (`crates/pi-ai/src/streaming.rs:360`), `AgentEvent` has eleven (`crates/pi-agent-core/src/events.rs:81`), and `ContentBlock` has four variants with different payloads (`crates/pi-ai/src/messages.rs:247`). `Message`, replay, session-entry, operation, and mutation enums add nested tagged graphs. | Library -> host for observations and host -> library for prompts, tool results, persistence, and replay. Exact variants and nested values are core/extended data, not reducible to text-only deltas. |
| JSON and opaque byte carriers | Tool calls/specs use `serde_json::Value` (`crates/pi-ai/src/messages.rs:300`, `crates/pi-ai/src/messages.rs:311`); custom records and tool details use `Box<RawValue>` (`crates/pi-agent-core/src/state.rs:62`, `crates/pi-agent-core/src/tools.rs:47`); replay data includes bytes and exact JSON bytes (`crates/pi-ai/src/streaming.rs:563`). | Both directions. These types preserve structured or opaque payloads that ordinary Rust callers can construct and inspect. |
| Recursive ordered JSON and header maps | `SimpleGenerationOptions` directly stores `OrderedJsonObject` and `HeaderMapSpec` (`crates/pi-ai/src/options.rs:598`, `crates/pi-ai/src/options.rs:602`). `OrderedJsonObject` maps exact `OrderedJsonString` keys to recursive `OrderedJsonValue` values containing arrays and objects (`crates/pi-ai/src/json_compat.rs:114`, `crates/pi-ai/src/json_compat.rs:324`); `HeaderMapSpec` is `BTreeMap<String, Option<String>>` (`crates/pi-ai/src/model.rs:17`). | Host -> library request configuration. This is not equivalent to a flat string map: it carries recursive values, exact UTF-16 strings, insertion order, and explicit header deletion. |
| Borrowed and iterator-generic APIs | Bare runs borrow `&mut Agent`; policy callback inputs contain nested references and lifetimes (`crates/pi-agent-core/src/policy.rs:16`, `crates/pi-agent-core/src/policy.rs:253`); several constructors accept `impl IntoIterator` or `impl Into` (`crates/pi-agent-core/src/run.rs:75`, `crates/pi-agent-core/src/run.rs:302`). | These are exact native signatures. Any later binding design must decide how to expose them without pretending the Rust signatures are concrete owned functions. |
| Tokio implementation types in public methods | `TokioAgentRun::events` exposes `tokio::sync::mpsc::Receiver` (`crates/pi-agent-runtime-tokio/src/lib.rs:133`), and `TokioAgentHandle::snapshots` exposes `tokio::sync::watch::Receiver` (`crates/pi-agent-runtime-tokio/src/lib.rs:369`). | Runtime-specific public types leak through otherwise app-facing methods; they are recorded as core/extended rather than erased from the inventory. |

## Resulting R1 boundary

The smallest faithful ordinary-embedding boundary observed in the repository
is not a JSON command/event envelope. It consists of:

1. `Models` construction and its library implementation of `ModelRuntime`.
2. `AgentState`, `ToolRegistry`, host `Tool` implementations (including
   `TypedTool<I,F>`), and `Agent::new`/restore/configuration.
3. The cloneable `TokioAgentHandle`, `TokioAgentRun`, async command methods,
   acknowledged `AgentEventSink`, direct cancellation, snapshots, and outcomes.
4. The complete `AgentEvent` -> `AssistantEvent` -> canonical message/content,
   replay, usage, error, tool, and deferred data graph.
5. The optional but real session-storage/repository trait boundary and its
   authoritative mutation/state graph.

Identified exclusions include low-level scheduler/assembler and lowering-plan
machinery, the Local executor twins, scripted runtime and deterministic OAuth
fixtures, provider request scratch state, binding-private fixture
configuration, and the Tokio coding-harness environment. They are excluded for
the stated scope reasons, not because they are private or because of an assumed
binding limitation.
