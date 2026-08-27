# Architecture v2 — Part 1: the proposal

> **Status: ADOPTED (owner decision, 2026-08-22).** This is the verbatim first answer from the design agent to the question below. Part 2 (`architecture-v2-part2-revision.md`) revises several sections after a deeper source review and takes precedence where the two differ. Together they are the architecture; `goal.md` is the governing statement that binds them.
>
> **The question asked:** design an extendable AI agent harness — an idiomatic Rust architecture for two crates representing the Rust equivalent of `pi-ai` and `pi-agent-core` (and eventually `pi-coding-agent`), taking advantage of Rust for cross-platform, FFI bindings, performance, and size, while learning from pi's package separation and the seam through which `pi-agent-core` builds on `pi-ai` (shared types, an injected `streamFn`, and a full `Models` instance only in the harness for auxiliary calls).

# Recommended direction

Port **the contracts and dependency boundaries**, not the TypeScript implementation structure.

Use two foundational crates:

- **`pi-ai`**: provider-neutral model, message, tool-schema, streaming, catalog, authentication, cost, and provider-dispatch runtime.
- **`pi-agent-core`**: stateful agent state machine, executable tools, tool scheduling, context preparation, model switching, and agent lifecycle events.

However, "two foundational crates" should not mean "only two Cargo packages." Provider implementations and language bindings are better represented as leaf packages around those two cores.

The most important seam should be:

```text
pi-agent-core
    depends on pi-ai's data types
    receives Arc<dyn ModelRuntime>
    never imports provider implementations
    never resolves authentication
    never queries a built-in provider catalog
```

`pi_ai::Models` implements `ModelRuntime`, but it is not the only possible implementation. Tests, proxies, replay backends, remote agent gateways, and FFI adapters can implement the same narrow trait.

That preserves the architectural elegance you identified in Pi while translating it into Rust's strengths.

---

## 1. What should be preserved from Pi

Pi currently treats a provider as the runtime unit that owns its model catalog, authentication behavior, and streaming implementation. The `Models` collection owns registered providers and routes a model request to the provider that owns that model. Providers may share wire-protocol implementations such as OpenAI Responses, OpenAI-compatible completions, or Anthropic Messages.

The selective provider factory mechanism is also worth preserving conceptually: one provider factory brings in only that provider's catalog and API implementation, while an explicit "all providers" entrypoint brings in everything.

Pi's model catalog behavior is particularly suitable for Rust:

- Catalog reads are synchronous against the last published snapshot.
- Dynamic provider refresh is an explicit asynchronous operation.
- Static providers treat refresh as a no-op.

On the agent side, the canonical setup constructs a provider collection, resolves a model, and injects `models.streamSimple.bind(models)` into the agent. The `StreamFn` contract is intentionally narrow and requires streaming failures to remain within the streaming protocol.

Pi also establishes useful ordering semantics for parallel tool execution: preflight is deterministic, executions may complete concurrently, completion events can follow completion order, but transcript tool-result messages remain in assistant source order.

Those are the architectural properties to retain.

---

# 2. Workspace shape

I would start with this workspace:

```text
workspace/
├── crates/
│   ├── pi-ai/
│   └── pi-agent-core/
├── providers/
│   ├── pi-ai-openai/
│   ├── pi-ai-anthropic/
│   ├── pi-ai-openrouter/
│   └── pi-ai-providers-all/
├── bindings/
│   └── pi-ffi/
└── examples/
```

The two core crates remain the stable conceptual architecture. Provider and binding packages are adapters.

```text
                   ┌────────────────────┐
                   │ App / CLI / FFI    │
                   └─────────┬──────────┘
                             │
                 ┌───────────┴────────────┐
                 │                        │
       ┌─────────▼──────────┐   ┌────────▼─────────┐
       │ pi-agent-core      │   │ pi-ai::Models    │
       │                    │   │                  │
       │ agent state machine├──►│ ModelRuntime     │
       │ tools and policies │   │ registry/auth    │
       └────────────────────┘   └────────┬─────────┘
                                         │
                   ┌─────────────────────┼────────────────────┐
                   │                     │                    │
         ┌─────────▼─────────┐ ┌─────────▼────────┐ ┌────────▼────────┐
         │ OpenAI provider   │ │ Anthropic        │ │ Third-party     │
         │ registration     │ │ registration     │ │ registration    │
         └─────────┬─────────┘ └─────────┬────────┘ └────────┬────────┘
                   │                     │                    │
             API protocol          API protocol         custom API
             implementation        implementation       implementation
```

## Why separate provider packages

TypeScript's lazy imports and bundler code-splitting do not translate directly to a statically linked Rust binary. Lazy initialization can defer constructing an HTTP client or loading a catalog, but it does not remove compiled code from the binary.

The Rust equivalent of selective imports is:

1. separate provider crates, or
2. disabled-by-default optional dependencies and Cargo features.

Separate provider crates scale better once the provider set becomes large. Cargo features are additive and unified across the dependency graph, so a feature enabled by any dependency can cause that implementation to be compiled for the whole graph. Cargo explicitly recommends splitting functionality into separate packages where feature combinations become problematic.

An explicit `pi-ai-providers-all` aggregator can recreate Pi's heavy `providers/all` entrypoint without making the core crate heavy.

---

# 3. `pi-ai`: responsibilities and boundaries

`pi-ai` should own five layers:

```text
Canonical data model
        ↓
Normalized streaming protocol
        ↓
ModelRuntime abstraction
        ↓
Models registry, auth, and routing
        ↓
Provider/API composition
```

It should not own any agent-loop logic.

## 3.1 Canonical data model

Keep the canonical model entirely provider-neutral and serializable.

Representative types:

```rust
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ModelId(pub String);

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: ProviderId,
    pub model: ModelId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelDescriptor {
    pub model_ref: ModelRef,
    pub display_name: String,
    pub api: ApiId,
    pub capabilities: ModelCapabilities,
    pub limits: ModelLimits,
    pub pricing: Option<ModelPricing>,

    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}
```

Use open string newtypes for `ProviderId`, `ModelId`, and `ApiId`, rather than closed enums. A closed enum would require a `pi-ai` release every time a third party introduces a provider or protocol.

`ModelDescriptor` should be plain data. Do not attach provider functions or SDK clients to it. This preserves straightforward persistence and model handoff. Pi similarly treats models and contexts as serializable values and performs provider-specific conversion only at the API boundary.

### Canonical messages

A reasonable message hierarchy is:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(TextBlock),
    Image(ImageBlock),
    Thinking(ThinkingBlock),
    ToolCall(ToolCallBlock),
}
```

The persisted envelope should carry a schema version:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub schema_version: u32,
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
}
```

Do not rely solely on the Rust crate version as the persistence schema version. Persisted sessions commonly outlive several library releases.

### Opaque provider metadata

Some APIs attach signatures, response identifiers, encrypted reasoning metadata, or resumable response handles. Preserve those in a deliberately opaque structure:

```rust
pub struct ProviderOpaque {
    pub provider: ProviderId,
    pub api: ApiId,
    pub kind: String,
    pub data: Box<serde_json::value::RawValue>,
}
```

An API-family encoder can then:

- pass it through when returning to the same provider;
- convert displayable thinking to tagged text during cross-provider handoff;
- omit unsupported opaque data under a documented policy.

This avoids polluting the canonical message enum with every provider-specific field.

---

## 3.2 The narrow runtime seam

The direct Rust analogue of Pi's injected `streamFn` should be a one-method object-safe trait:

```rust
use std::{future::Future, pin::Pin};

pub type BoxFuture<'a, T> =
    Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait ModelRuntime: Send + Sync {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<AssistantStream, AiError>>;
}
```

```rust
pub struct ModelRequest {
    pub model: ModelRef,
    pub context: Context,
    pub options: SimpleGenerationOptions,
}
```

`Models` implements this trait:

```rust
impl ModelRuntime for Models {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<AssistantStream, AiError>> {
        Box::pin(async move {
            self.stream_simple(request, cancellation).await
        })
    }
}
```

This is the entire provider-facing dependency needed by `pi-agent-core`.

### Why boxed futures here

The registry must hold heterogeneous implementations behind `dyn ModelRuntime`, `dyn ChatApi`, `dyn AuthResolver`, and similar boundaries. Public trait methods written directly as `async fn` are not dyn-compatible because they have an opaque future return type. Explicit `BoxFuture` and boxed stream types make dynamic dispatch and `Send` requirements unambiguous.

Within concrete, statically dispatched implementation code, ordinary `async fn` remains appropriate.

---

## 3.3 Streaming protocol

Use `futures_core::Stream` as the public abstraction rather than a Tokio channel type. `Stream` is the asynchronous counterpart to `Iterator`, without coupling the semantic interface to one executor.

```rust
pub type BoxEventStream<T> =
    Pin<Box<dyn futures_core::Stream<Item = T> + Send + 'static>>;

pub struct AssistantStream {
    inner: BoxEventStream<Result<AssistantEvent, StreamError>>,
}
```

A normalized event model might be:

```rust
#[non_exhaustive]
pub enum AssistantEvent {
    Started {
        message_id: MessageId,
    },

    ContentBlockStarted {
        index: u32,
        kind: ContentBlockKind,
    },

    TextDelta {
        index: u32,
        delta: String,
    },

    ThinkingDelta {
        index: u32,
        delta: String,
    },

    ToolCallDelta {
        index: u32,
        call_id: ToolCallId,
        name: Option<String>,
        arguments_fragment: String,
    },

    ContentBlockFinished {
        index: u32,
    },

    UsageUpdated {
        cumulative: Usage,
    },

    Finished {
        message: AssistantMessage,
    },
}
```

### Do not clone the entire partial message on every delta

Pi's protocol exposes a convenient partial message snapshot on streaming updates. In Rust, making the full snapshot part of every event can create substantial copying or `Arc` churn.

Prefer:

- delta-only canonical events;
- an `AssistantAssembler` utility that applies those events;
- an optional higher-level event adapter that includes snapshots for UI clients.

```rust
let mut assembler = AssistantAssembler::new();

while let Some(event) = stream.next().await {
    let event = event?;
    assembler.apply(&event)?;

    if let Some(snapshot) = assembler.snapshot() {
        render(snapshot);
    }
}
```

### Tool-call arguments

Expose raw argument fragments during streaming. Do not make a partially parsed `serde_json::Value` authoritative.

Only after a complete `ToolCallFinished` or final assistant message should arguments be:

1. parsed;
2. schema validated;
3. accepted for execution.

Pi explicitly avoids executing tool calls from a response truncated by the output-token limit because apparently valid arguments may still be incomplete. That invariant belongs in the Rust agent as well.

### Error semantics

This is one place where an idiomatic Rust design should diverge slightly from a literal port.

I recommend:

- errors before a stream is established return `Err(AiError)` from `ModelRuntime::stream`;
- errors after streaming starts produce one terminal `Err(StreamError)`;
- `StreamError` carries the partially assembled assistant message;
- after `Finished` or terminal `Err`, the stream is fused and emits nothing else.

```rust
pub struct StreamError {
    pub error: AiError,
    pub partial: Option<AssistantMessage>,
}
```

A compatibility helper can convert that into an `AssistantMessage` whose finish reason is `Error` or `Cancelled` when a persisted transcript requires that representation.

This preserves Pi's important semantic property—partial output is not lost—while still using Rust's `Result` type for recoverable failures.

---

## 3.4 Common options and API-specific options

Do not directly port TypeScript's pervasive `Model<Api>` generic into the dynamic Rust registry.

A heterogeneous runtime registry naturally wants type erasure. A foreign-language boundary wants serializable data. Meanwhile, Rust callers still benefit from typed API-specific options.

Use two surfaces.

### Stable simple surface

This is what the agent and FFI use:

```rust
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SimpleGenerationOptions {
    pub max_output_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub stop: Vec<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub seed: Option<u64>,
    pub session_id: Option<String>,
    pub headers: HeaderMapSpec,

    #[serde(default)]
    pub extensions: BTreeMap<ApiId, Box<serde_json::value::RawValue>>,
}
```

### Typed convenience surface

API crates can offer typed wrappers:

```rust
pub trait ApiOptions: Serialize {
    const API_ID: &'static str;
}

pub struct AnthropicMessagesOptions {
    pub thinking_budget_tokens: Option<u32>,
    pub thinking_enabled: bool,
}

impl ApiOptions for AnthropicMessagesOptions {
    const API_ID: &'static str = "anthropic-messages";
}
```

A `TypedModel<A>` wrapper can provide compile-time narrowing for applications that need it:

```rust
let model = models.model(&model_ref)?;
let anthropic = model.require_api::<AnthropicMessages>()?;

let stream = anthropic_messages::stream(
    &models,
    anthropic,
    context,
    AnthropicMessagesOptions {
        thinking_enabled: true,
        thinking_budget_tokens: Some(2_048),
    },
    cancellation,
).await?;
```

Internally, the typed wrapper can serialize an API-specific extension payload. The runtime validates that the model's API identifier and option payload agree.

This provides:

- a stable dynamic path for agents and FFI;
- good Rust ergonomics for provider-specific callers;
- no closed global enum that third-party API crates cannot extend.

---

## 3.5 Provider composition

Prefer a concrete `ProviderRegistration` assembled from smaller behavior traits over one very broad `Provider` trait.

```rust
pub struct ProviderRegistration {
    pub descriptor: ProviderDescriptor,
    pub auth: Arc<dyn AuthResolver>,
    pub catalog: Arc<dyn ModelCatalog>,
    pub APIs: HashMap<ApiId, Arc<dyn ChatApi>>,
    pub request_transform: Option<Arc<dyn RequestTransform>>,
}
```

```rust
pub trait ChatApi: Send + Sync {
    fn stream(
        &self,
        request: ResolvedApiRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<AssistantStream, AiError>>;
}
```

A factory becomes:

```rust
pub fn openrouter_provider(config: OpenRouterConfig) -> ProviderRegistration {
    ProviderRegistration::builder("openrouter")
        .catalog(static_catalog())
        .auth(openrouter_auth(config.auth))
        .api(
            ApiId::new("openai-completions"),
            shared_openai_completions_api(config.http),
        )
        .request_transform(openrouter_transform())
        .build()
}
```

This directly models the relationship:

```text
Provider
  = identity
  + catalog
  + auth semantics
  + API dispatch table
  + small compatibility/request customizations
```

### Prefer configuration over subclasses

Many OpenAI-compatible providers differ only in:

- base URL;
- default headers;
- field naming compatibility;
- support for developer/system roles;
- usage-stream behavior;
- tool-choice quirks;
- model catalog.

Represent those as a compatibility profile:

```rust
pub struct OpenAiCompatibility {
    pub supports_stream_usage: bool,
    pub uses_max_completion_tokens: bool,
    pub supports_developer_role: bool,
    pub tool_choice_encoding: ToolChoiceEncoding,
}
```

Reserve custom `RequestTransform` or a custom `ChatApi` implementation for actual protocol differences.

This keeps provider code declarative and makes conformance testing much easier.

---

## 3.6 `Models`: registry and router

`Models` should be a concrete cloneable handle around shared state:

```rust
#[derive(Clone)]
pub struct Models {
    inner: Arc<ModelsInner>,
}
```

It owns:

```rust
struct ModelsInner {
    providers: ProviderRegistry,
    credentials: Arc<dyn CredentialStore>,
    auth_context: AuthContext,
    request_middleware: Vec<Arc<dyn RequestMiddleware>>,
}
```

Its request path is:

```text
ModelRef
  ↓
look up current ModelDescriptor
  ↓
look up owning ProviderRegistration
  ↓
resolve provider auth
  ↓
merge base URL and headers
  ↓
apply Models-level transforms
  ↓
dispatch by ModelDescriptor.api
  ↓
normalize provider stream
```

No global mutable provider registry should be necessary.

### Registration

Prefer a builder for the common immutable setup:

```rust
let models = Models::builder()
    .credential_store(credentials)
    .provider(openai_provider(openai_config))
    .provider(anthropic_provider(anthropic_config))
    .build()?;
```

Runtime extension can still be supported through atomic provider-map replacement:

```rust
models.set_provider(custom_provider)?;
models.remove_provider(&provider_id)?;
```

Do not hold registry locks across network requests. Resolve the provider to an `Arc<ProviderRegistration>` and release the registry lock before awaiting anything.

---

## 3.7 Model catalogs

Represent the currently visible model list as an immutable snapshot.

```rust
pub trait ModelCatalog: Send + Sync {
    fn snapshot(&self) -> Arc<[ModelDescriptor]>;

    fn refresh(
        &self,
        context: RefreshContext,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<RefreshOutcome, CatalogError>>;
}
```

For a static catalog, `refresh` returns a no-op result.

For a dynamic catalog:

1. restore the last persisted snapshot;
2. optionally perform a network refresh;
3. validate the entire new list;
4. atomically publish it;
5. persist it after or as part of the publish transaction.

Readers should see either the old complete snapshot or the new complete snapshot, never a partially updated vector.

`Models::refresh` should return per-provider results instead of failing the whole operation on one provider:

```rust
pub struct RefreshReport {
    pub cancelled: bool,
    pub providers: BTreeMap<ProviderId, Result<RefreshStats, CatalogError>>,
}
```

This follows Pi's synchronous-read/explicit-refresh model while fitting Rust's ownership and concurrency model.

---

## 3.8 Authentication and credential storage

The auth layering in your provider notes is well chosen:

```text
provider auth headers
  → model headers
  → explicit request headers
  → final header transform
  → API implementation
```

The header transform belongs to `Models`, not to the provider API implementation, and should be consumed before API dispatch.

Use `http::HeaderMap` internally rather than a case-sensitive `HashMap<String, String>`.

### Provider-owned auth

Each provider supplies an `AuthResolver`:

```rust
pub trait AuthResolver: Send + Sync {
    fn resolve(
        &self,
        request: ResolveAuthRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Option<ResolvedAuth>, AuthError>>;

    fn login(
        &self,
        interaction: Arc<dyn AuthInteraction>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Credential, AuthError>>;

    fn logout(
        &self,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<(), AuthError>>;
}
```

> Correction: Pinned Pi's `Models.logout` deletes the provider credential from the credential store and does not invoke provider-owned logout cleanup (`packages/ai/src/models.ts:617–627`). The resolver method remains part of the adopted Rust trait shape, but the `Models` control-plane operation follows Pi's delete-only behavior.

```rust
pub struct ResolvedAuth {
    pub api_key: Option<SecretString>,
    pub headers: http::HeaderMap,
    pub base_url: Option<Url>,
    pub source: AuthSource,
}
```

> Correction: Pinned Pi keeps provider-scoped environment/config values on the outer `AuthResult.env`, separate from `ModelAuth`, and carries request authentication plus that environment through the same resolution operation. The Rust object-safe resolver returns one owned `ResolvedAuth`, so its chosen representation flattens `environment: BTreeMap<String, String>` onto `ResolvedAuth`; `Models` overlays request-scoped environment values only after resolution. `ResolvedAuth` also carries a private `transport_headers: HeaderMap` channel for credential-derived signer/SDK invariants required by Part 2 §2.4. Neither addition is serialized or exposed as model auth, and ordinary logical header precedence remains unchanged (`packages/ai/src/auth/types.ts`; `packages/ai/src/auth/resolve.ts`; `packages/ai/src/images-models.ts:183–212`).

Do not derive ordinary `Debug` or `Serialize` for secret-bearing types. Use redacted wrappers.

### Credential store transaction

The attached design requires serialized read-modify-write so that OAuth refresh cannot race and rotate the same credential twice.

A closure-based `modify<T, F>` method becomes awkward on an object-safe async Rust trait. An owned lease is a better fit:

```rust
pub trait CredentialStore: Send + Sync {
    fn read(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Option<Credential>, StoreError>>;

    fn list(
        &self,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<CredentialInfo>, StoreError>>;

    fn acquire_lease(
        &self,
        provider: ProviderId,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn CredentialLease>, StoreError>>;
}
```

```rust
pub trait CredentialLease: Send {
    fn current(&self) -> Option<&Credential>;

    fn replace(&mut self, credential: Option<Credential>);

    fn commit(
        self: Box<Self>,
    ) -> BoxFuture<'static, Result<(), StoreError>>;
}
```

The lease implementation can own:

- an asynchronous process-local mutex guard;
- a file lock;
- a database transaction;
- a distributed lease.

OAuth refresh occurs while holding this lease. A failed refresh should remain a failed stored credential, not silently fall back to an environment variable. That makes authentication precedence deterministic.

---

## 3.9 Usage and cost

Keep usage and monetary cost separate.

```rust
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub source: UsageSource,
}
```

> Correction: pinned Pi's `calculateContextTokens` prefers the provider's nonzero/truthy `usage.totalTokens`, and otherwise falls back to the normalized input/output/cache component sum. Google can report a nonzero total that differs from those components, or zero alongside nonzero components. `Usage` therefore also retains `total_tokens: Option<u64>`, and context planning gives only a nonzero value precedence; omitting the field or treating zero as authoritative changes turn-two max-output request bytes.

```rust
pub struct Cost {
    pub currency: Currency,
    pub micros: u128,
}
```

Avoid `f64` for money. Store pricing as integer currency units per million tokens and perform integer arithmetic.

Be explicit about whether a streaming usage event is:

- a delta; or
- the cumulative total so far.

I recommend cumulative usage events plus one authoritative final usage record. That prevents accidental double-counting when consumers reconnect or replay events.

`pi-ai` calculates per-response cost because it owns model pricing and provider usage normalization. `pi-agent-core` may aggregate those values into run-level totals.

---

# 4. `pi-agent-core`: an agent state machine, not an LLM client

`pi-agent-core` should depend on the following parts of `pi-ai`:

```text
ModelRef
Context / Message / AssistantMessage
ToolSpec / ToolCall / ToolResultMessage
AssistantEvent / AssistantStream
Usage / Cost
ModelRuntime
CancellationToken
```

It should not depend on:

```text
Models provider registry methods
ProviderRegistration
CredentialStore
AuthResolver
built-in model catalogs
OpenAI/Anthropic SDK or protocol types
```

## 4.1 Agent construction

```rust
pub struct Agent {
    runtime: Arc<dyn ModelRuntime>,
    state: AgentState,
    tools: ToolRegistry,
    context_policy: Arc<dyn ContextPolicy>,
    hooks: Arc<dyn AgentHooks>,
    config: AgentConfig,
}
```

```rust
let mut agent = Agent::builder(Arc::new(models.clone()))
    .model(ModelRef::new("anthropic", "some-model"))
    .system_prompt("You are a helpful assistant.")
    .tool(read_file_tool)
    .context_policy(context_policy)
    .build()?;
```

The `Models` value is passed because it implements `ModelRuntime`, not because `Agent` knows it is a provider collection.

A fake runtime is equally valid:

```rust
let runtime = Arc::new(ScriptedRuntime::new([
    scripted_text("I will inspect the file."),
    scripted_tool_call("read_file", json!({ "path": "README.md" })),
    scripted_text("Here is the result."),
]));
```

---

## 4.2 State

Use a model reference rather than embedding a complete model descriptor in the agent state:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentState {
    pub schema_version: u32,
    pub system_prompt: String,
    pub model: ModelRef,
    pub reasoning: ReasoningLevel,
    pub transcript: Vec<AgentRecord>,
}
```

The registry resolves the current descriptor at request time. This means:

- catalog metadata can be refreshed without rewriting session state;
- snapshots stay small;
- restoring a session does not deserialize runtime behavior;
- switching models is simply replacing a `ModelRef`.

### Custom agent messages

TypeScript declaration merging does not have a useful direct Rust equivalent.

For a persistence- and FFI-friendly harness, use an explicit extensible record:

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentRecord {
    Llm(Message),

    Custom {
        type_name: String,
        payload: Box<serde_json::value::RawValue>,
    },
}
```

A `ContextPolicy` decides which custom records become LLM messages and which remain UI-only.

That is less statically expressive than a generic `Agent<M>`, but it avoids propagating a generic message parameter through every event, snapshot, tool hook, binding, and session store.

A separate advanced generic API could be added later, but it should not be the baseline harness representation.

---

## 4.3 Running the state machine

For the low-level API, let the caller drive the stream:

```rust
pub type AgentEventStream<'a> =
    Pin<Box<dyn Stream<Item = AgentEvent> + Send + 'a>>;

impl Agent {
    pub fn run<'a>(
        &'a mut self,
        input: AgentInput,
        cancellation: CancellationToken,
    ) -> AgentEventStream<'a>;
}
```

Borrowing `&mut self` ensures that one `Agent` cannot accidentally run two turns concurrently.

A higher-level actor-style `AgentHandle` can serialize commands for GUI and FFI clients:

```rust
let run = handle.prompt("Inspect this repository").await?;
let mut events = run.events();

while let Some(event) = events.next().await {
    render(event);
}
```

The actor facade belongs behind an optional runtime feature or in the FFI/application layer. The core state machine should not require spawning detached Tokio tasks.

### Backpressure

A directly polled stream gives natural backpressure: the state machine does not progress until the consumer polls again.

When adapting to channels:

- use bounded channels;
- document the behavior when the consumer is slow;
- never silently discard terminal, tool-result, or state-commit events;
- give each event a monotonically increasing sequence number.

---

## 4.4 Event model

Use explicit run, turn, message, and tool identifiers.

```rust
#[non_exhaustive]
pub enum AgentEvent {
    RunStarted {
        run_id: RunId,
    },

    TurnStarted {
        run_id: RunId,
        turn: u32,
        model: ModelRef,
    },

    MessageStarted {
        message_id: MessageId,
        role: MessageRole,
    },

    AssistantUpdate {
        message_id: MessageId,
        event: AssistantEvent,
    },

    MessageCommitted {
        message: AgentRecord,
    },

    ToolExecutionStarted {
        call: ToolCall,
    },

    ToolExecutionUpdated {
        call_id: ToolCallId,
        update: ToolUpdate,
    },

    ToolExecutionFinished {
        call_id: ToolCallId,
        result: ToolOutput,
        is_error: bool,
    },

    TurnFinished {
        outcome: TurnOutcome,
    },

    RunFinished {
        outcome: RunOutcome,
    },
}
```

Wrap this in an envelope for persistence and FFI:

```rust
pub struct AgentEventEnvelope {
    pub sequence: u64,
    pub run_id: RunId,
    pub event: AgentEvent,
}
```

Expected operational outcomes—cancellation, provider errors, tool failures—should appear in `RunOutcome`. Reserve panics or outer `AgentError`s for violated harness invariants and configuration errors.

---

## 4.5 Executable tools

`pi-ai` defines a provider-neutral `ToolSpec`. `pi-agent-core` adds executable behavior.

```rust
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Parallel
    }

    fn execute(
        &self,
        context: ToolCallContext,
        updates: Arc<dyn ToolUpdateSink>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>>;
}
```

```rust
pub struct ToolOutput {
    pub content: Vec<ToolResultContent>,
    pub details: Option<Box<serde_json::value::RawValue>>,
    pub usage: Option<Usage>,
    pub added_tool_names: Vec<String>,
    pub terminate: bool,
}
```

The dynamic tool-registry boundary necessarily operates on `serde_json::Value` or raw JSON. Provide a typed adapter for normal Rust tools:

```rust
pub struct TypedTool<I, F> {
    spec: ToolSpec,
    function: F,
    _input: PhantomData<I>,
}
```

Where `I` implements:

```rust
DeserializeOwned + JsonSchema + Send + 'static
```

The adapter performs:

```text
raw JSON
  → JSON Schema validation
  → serde deserialization into I
  → typed implementation
  → ToolOutput
```

This gives tool authors a typed implementation without making the heterogeneous registry generic.

---

## 4.6 Tool execution phases

Make the phases explicit and testable:

```text
1. Finalize all tool calls from the assistant message.
2. Reject all calls if the assistant response was truncated.
3. Resolve each tool by name.
4. Normalize compatibility arguments.
5. Validate arguments against the schema.
6. Run authorization/preflight sequentially.
7. Execute allowed calls according to scheduling policy.
8. Apply post-execution hooks.
9. Emit completion events in actual completion order.
10. Commit ToolResult messages in assistant source order.
11. Decide whether another model turn is required.
```

This preserves the useful Pi semantics around deterministic preflight and source-ordered transcripts.

For cancellation:

- create a child cancellation token for the tool batch;
- cancel every outstanding tool when the run is cancelled;
- continue driving or joining those tool futures until they settle;
- only then emit `RunFinished`.

Do not detach tool tasks that can continue modifying the filesystem after the agent reports that it has stopped.

CPU-bound tools should explicitly offload work; the harness should treat "parallel" as asynchronous concurrency, not assume every tool is safe to execute on an executor worker thread.

---

## 4.7 Hooks versus policy objects

Avoid accumulating dozens of unrelated closure fields on `AgentConfig`.

Group behavior by responsibility:

```rust
pub trait ContextPolicy: Send + Sync {
    fn prepare(
        &self,
        state: AgentStateView<'_>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<PreparedContext, ContextError>>;
}
```

```rust
pub trait ToolPolicy: Send + Sync {
    fn before_call(
        &self,
        context: BeforeToolCall<'_>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolAuthorization, AgentError>>;

    fn after_call(
        &self,
        context: AfterToolCall<'_>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<ToolOutputPatch, AgentError>>;
}
```

```rust
pub trait TurnPolicy: Send + Sync {
    fn prepare_next_turn(
        &self,
        context: CompletedTurn<'_>,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<NextTurn, AgentError>>;
}
```

These phases should have documented ordering. Hooks that can mutate state at arbitrary times eventually make replay, debugging, and event ordering difficult.

### Authorization is not sandboxing

A `ToolPolicy` can block a call or ask a user for approval. It cannot provide OS-level isolation.

Pi itself explicitly distinguishes its runtime from a permission sandbox and recommends external containerization or sandboxing when stronger boundaries are required.

Make the same distinction:

```text
ToolPolicy       = logical authorization
Tool implementation / process sandbox = security boundary
```

---

## 4.8 Context compaction and auxiliary model calls

Do not inject a full `Models` instance into the core `Agent` merely for summarization.

Instead, an application-provided `ContextPolicy` may hold whatever it needs:

```rust
pub struct SummarizingContextPolicy {
    runtime: Arc<dyn ModelRuntime>,
    summary_model: ModelRef,
    token_budget: u64,
}
```

Or, when it genuinely needs model lookup and auth-management operations:

```rust
pub struct RegistryBackedContextPolicy {
    models: Models,
    summary_model: ModelRef,
}
```

The dependency remains outside the agent state machine:

```text
Agent
  → ContextPolicy trait
      → optional Models instance
```

not:

```text
Agent
  → concrete Models management API
```

The result of `ContextPolicy::prepare` could include both the context and a model override:

```rust
pub struct PreparedContext {
    pub context: Context,
    pub model_override: Option<ModelRef>,
    pub options_override: Option<SimpleGenerationOptions>,
}
```

That supports:

- pruning;
- compaction;
- retrieval injection;
- model switching;
- reasoning-level switching;
- provider failover.

---

## 4.9 Persistence

Keep storage I/O outside the fundamental loop.

`pi-agent-core` should expose:

```rust
pub struct AgentSnapshot {
    pub schema_version: u32,
    pub state: AgentState,
    pub next_sequence: u64,
}
```

Tools are persisted by name and possibly version, not serialized as executable objects. Restoring requires a `ToolRegistry` that can resolve those names.

Likewise, a persisted `ModelRef` must be resolved against the application's current `Models` collection.

A robust restore process therefore looks like:

```text
deserialize AgentSnapshot
  → migrate schema
  → resolve ModelRef
  → bind ToolRegistry
  → validate unresolved custom message kinds
  → construct Agent
```

An optional session-store adapter can subscribe to `MessageCommitted`, `TurnFinished`, and `RunFinished` events. Partial token deltas generally should not be the sole durable record unless crash recovery during a stream is a hard requirement.

---

# 5. The precise dependency seam

The most important public trait in the design should remain small:

```rust
pub trait ModelRuntime: Send + Sync {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<AssistantStream, AiError>>;
}
```

That is enough for the agent.

The full `Models` API is separate:

```rust
impl Models {
    pub fn providers(&self) -> ProviderSnapshot;
    pub fn models(&self) -> ModelSnapshot;
    pub fn model(&self, model_ref: &ModelRef) -> Option<ModelDescriptor>;

    pub async fn refresh(
        &self,
        request: RefreshRequest,
        cancellation: CancellationToken,
    ) -> RefreshReport;

    pub async fn auth_status(
        &self,
        provider: &ProviderId,
        cancellation: CancellationToken,
    ) -> Result<AuthStatus, AuthError>;

    pub async fn login(/* ... */) -> Result<(), AuthError>;
    pub async fn logout(/* ... */) -> Result<(), AuthError>;
}
```

The distinction is:

```text
ModelRuntime = request execution capability
Models       = model-management control plane
```

`pi-agent-core` receives the former.

Applications and context policies may use the latter.

---

# 6. FFI architecture

Do not expose Rust trait objects, futures, streams, references, or Tokio types directly over FFI.

Add a binding facade:

```text
pi-ffi
  depends on pi-ai
  depends on pi-agent-core
  owns executor/runtime integration
  translates events to foreign-language values
  exposes opaque handles
```

A generic C-style surface might look like:

```c
pi_models_handle *pi_models_create(const char *config_json);

pi_agent_handle *pi_agent_create(
    pi_models_handle *,
    const char *agent_config_json
);

uint64_t pi_agent_run(
    pi_agent_handle *,
    const char *input_json,
    pi_event_callback callback,
    void *user_data
);

void pi_agent_cancel(pi_agent_handle *, uint64_t run_id);
void pi_agent_destroy(pi_agent_handle *);
```

The callback receives a versioned event envelope:

```json
{
  "schemaVersion": 1,
  "sequence": 42,
  "runId": "…",
  "type": "assistant_text_delta",
  "data": {
    "messageId": "…",
    "index": 0,
    "delta": "hello"
  }
}
```

For Swift, Kotlin, Python, or Ruby, UniFFI is a reasonable facade option and supports generated bindings and asynchronous Rust functions.

For a broad C-compatible surface, use `extern "C"` with opaque handles. Rust's native `"Rust"` ABI has no stability guarantee, so dynamically loading Rust libraries and passing trait objects across versions should not be part of the provider-extension design.

## Runtime-loadable provider plugins

Compile-time third-party provider crates are straightforward:

```rust
models.set_provider(acme_provider(config))?;
```

Runtime-loaded providers require a different boundary. Suitable choices are:

- a versioned C ABI;
- a WebAssembly component interface;
- JSON-RPC over stdio or a socket.

Do not treat a Rust `cdylib` exporting `Box<dyn Provider>` as a stable plugin ABI.

---

# 7. Error hierarchy

Avoid leaking `reqwest`, SDK-specific, OAuth-library, or JSON-parser errors into the public contract.

```rust
#[non_exhaustive]
pub enum AiErrorKind {
    InvalidRequest,
    UnknownProvider,
    UnknownModel,
    Authentication,
    Authorization,
    RateLimited,
    Transport,
    ProviderRejected,
    Protocol,
    Cancelled,
    Internal,
}
```

```rust
pub struct AiError {
    pub kind: AiErrorKind,
    pub message: String,
    pub provider: Option<ProviderId>,
    pub model: Option<ModelRef>,
    pub retryable: bool,
    pub retry_after: Option<Duration>,
    pub provider_code: Option<String>,
}
```

Internally retain the source error for Rust diagnostics, but create a separately sanitized `AiErrorReport` for serialization and FFI.

For the agent:

```rust
pub enum RunOutcome {
    Completed {
        new_messages: Vec<AgentRecord>,
        usage: Usage,
        cost: Option<Cost>,
    },

    Cancelled {
        partial: Option<AssistantMessage>,
    },

    Failed {
        error: AiErrorReport,
        partial: Option<AssistantMessage>,
    },
}
```

Tool execution errors ordinarily become `ToolResultMessage { is_error: true }` and allow the model to recover. Harness configuration or invariant errors terminate the run.

---

# 8. Testing architecture

The fake provider/runtime should be a first-class component of `pi-ai`, not an afterthought.

```rust
let runtime = ScriptedRuntime::builder()
    .response(text_response("First response"))
    .response(tool_call_response(
        "read_file",
        json!({ "path": "Cargo.toml" }),
    ))
    .response(text_response("Final response"))
    .build();
```

## `pi-ai` conformance tests

Every API implementation should pass a shared suite covering:

- exactly one stream start;
- exactly one terminal result;
- no events after termination;
- monotonically valid content indexes;
- valid final tool-call JSON;
- partial content preserved on transport failure;
- cancellation propagation;
- usage normalization;
- provider error sanitization;
- cross-provider context conversion;
- header precedence;
- authentication resolution precedence.

Use golden tests for:

- canonical context → provider request;
- provider SSE/chunk stream → canonical events;
- canonical message → persisted JSON.

## `pi-agent-core` conformance tests

Use only `ScriptedRuntime`, never live providers, for the main state-machine tests:

- text-only completion;
- one and multiple tool calls;
- unknown tool;
- invalid arguments;
- blocked preflight;
- postprocessing;
- parallel completion order;
- source-order transcript persistence;
- per-tool sequential override;
- cancellation during model stream;
- cancellation during tools;
- truncated tool call is never executed;
- context compaction;
- model switching;
- queued steering and follow-up messages;
- restoring from a snapshot.

The most important property test is:

> For every completed run, replaying committed transcript events produces exactly the final `AgentState`.

That invariant will pay off when building CLIs, GUIs, RPC interfaces, and session branching.

---

# 9. What I would deliberately not port

## TypeScript module augmentation

Replace it with tagged custom records and a context projection policy.

## Global registries

Every registry, credential store, provider cache, and runtime should belong to an explicit `Models` instance.

## Dynamic imports as the size mechanism

Use separate provider packages or disabled optional dependencies. Lazy construction is useful, but it is not code elimination.

## `Model<Api>` everywhere

Use `ModelRef` and dynamic `ModelDescriptor` in the main path. Add optional typed wrappers at API-specific call sites.

## Provider SDK types in public APIs

Normalize all provider requests, events, usage, and errors before exposing them.

## Full `Models` access inside `Agent`

Inject `ModelRuntime`. Let auxiliary policies independently hold `Models` when they truly need the management API.

## Errors represented only as successful assistant values

Preserve partial messages, but use `Result` at Rust boundaries.

## Unbounded event channels

Let the low-level stream provide backpressure; make channel buffering explicit in higher-level facades.

---

# 10. Suggested implementation sequence

### Milestone 1: contracts and fake runtime

Implement in `pi-ai`:

- IDs and canonical message model;
- `ModelRef` and `ModelDescriptor`;
- tool specifications;
- usage and cost types;
- `AssistantEvent`;
- `AssistantAssembler`;
- `ModelRuntime`;
- `ScriptedRuntime`;
- versioned serialization.

No real provider is needed yet.

### Milestone 2: agent loop

Implement in `pi-agent-core`:

- `AgentState`;
- run/turn state machine;
- event stream;
- dynamic and typed tools;
- schema validation;
- sequential and parallel scheduling;
- cancellation;
- context policy;
- model switching;
- snapshot serialization.

Develop the entire loop against `ScriptedRuntime`.

### Milestone 3: `Models` control plane

Add to `pi-ai`:

- provider registration;
- model snapshots;
- explicit refresh;
- API dispatch;
- request/header transforms;
- in-memory credential store;
- auth resolution.

### Milestone 4: prove API/provider separation

Implement:

- one OpenAI-family API protocol;
- one Anthropic-family API protocol;
- two providers sharing the OpenAI-family implementation;
- one provider using the Anthropic-family implementation.

This proves that API protocols and providers are genuinely independent.

### Milestone 5: persistent credentials and FFI

Add:

- file-backed or database-backed credential leases;
- OAuth refresh locking;
- versioned FFI event envelopes;
- opaque handles;
- cancellation;
- one generated binding target.

---

# 11. The core recommendation in code

The architecture can be summarized by these definitions:

```rust
// pi-ai

pub trait ModelRuntime: Send + Sync {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<AssistantStream, AiError>>;
}

#[derive(Clone)]
pub struct Models {
    inner: Arc<ModelsInner>,
}

impl ModelRuntime for Models {
    fn stream(
        &self,
        request: ModelRequest,
        cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<AssistantStream, AiError>> {
        Box::pin(self.stream_simple(request, cancellation))
    }
}

pub struct ProviderRegistration {
    descriptor: ProviderDescriptor,
    catalog: Arc<dyn ModelCatalog>,
    auth: Arc<dyn AuthResolver>,
    apis: HashMap<ApiId, Arc<dyn ChatApi>>,
}
```

```rust
// pi-agent-core

pub struct Agent {
    runtime: Arc<dyn ModelRuntime>,
    state: AgentState,
    tools: ToolRegistry,
    context_policy: Arc<dyn ContextPolicy>,
    hooks: Arc<dyn AgentHooks>,
}

impl Agent {
    pub fn run<'a>(
        &'a mut self,
        input: AgentInput,
        cancellation: CancellationToken,
    ) -> AgentEventStream<'a>;
}
```

That yields the desired dependency graph:

```text
provider crates ───────► pi-ai
                            ▲
                            │ types + ModelRuntime
                            │
                       pi-agent-core
                            ▲
                            │
                      app / CLI / FFI
```

The central design judgment is therefore:

> **`Models` is the full model-provider control plane. `ModelRuntime` is its narrow execution capability. The agent depends only on the capability.**

That is the Rust-native expression of the strongest architectural idea in `pi-ai` and `pi-agent-core`.
