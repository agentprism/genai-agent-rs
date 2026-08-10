# rust-genai-agent

Provider-neutral, streaming agent loops and tools for Rust, built on
[`genai`](https://docs.rs/genai). The crate supplies a stateful [`Agent`], a stateless loop, an
injectable provider boundary, schema-validated tools, ordered lifecycle events, steering and
follow-up queues, cancellation, offline scripted testing, and an optional authenticated proxy
transport.

This crate ports the non-harness contract of `@earendil-works/pi-agent-core`. Durable sessions,
compaction, reducers, reference tools, skills, prompt templates, telemetry setup, and Node-specific
bindings are deliberately **not** included. They belong in an application or a separate harness
crate.

At the M1-M6 completion checkpoint, all **52/52** case-mapped upstream behaviors were green and
the all-feature repository suite was **112/112** tests green. Eight proxy/streaming-JSON security
regressions landed during release hardening, and upstream additions (blocked-call termination and
the reset-while-processing guard) plus TypeScript error-text pins grew the matrix to **55/55**
mapped cases. Subsequent production-parity batches — the `thinking_budgets` map, the `transport`
advisory, and `AgentUsage` cost/cache-write-1h/reasoning accounting with an injectable
`PriceCatalog` — keep the mapped matrix at **55/55**. The 0.2.0 release wrap-up adds
`#[non_exhaustive]` plus complete builder coverage to the public config, usage, and error types,
and the 0.2.0 suite was **149/149** green. The core-runtime batch — independent
`session_id`/`max_retries`/`max_retry_delay_ms` request fields, shared `ThinkingBudgets`
resolution for next-turn thinking updates, and fallible tool preparation and before/after hook
channels whose errors become in-band tool results — brings the current suite to **215/215** green.
The execution-seam batch — request-level `on_payload`/`on_response` overrides honored by
`GenaiStreamFn` through the fork's new `ExecOptions` execution methods, a saturating retry-delay
conversion, and the DIST-01 interim distribution gate — brings the current suite to **220/220**
green.

## Installation

The minimum supported Rust version is 1.88.

**This crate is not available on crates.io.** It depends on fork-only `genai` APIs that no
registry serves, so its manifest carries `publish = false` and there is no version-only
dependency line to copy. The supported interim consumption path is **locally packaged crate
archives plus an exact pinned `[patch.crates-io]` entry**:

1. Package both crates from the sibling checkouts:
   `cargo package --allow-dirty --no-verify` in `rust-genai` (produces
   `genai-0.7.0-beta.19.1-agentprism.crate`) and in this repository (produces
   `rust-genai-agent-0.2.0.crate`).
2. Extract both archives into a `vendor/` directory inside your workspace.
3. Depend on the extracted agent archive and patch the exact fork `genai` version to the
   extracted fork archive:

```toml
[dependencies]
rust-genai-agent = { path = "vendor/rust-genai-agent-0.2.0" }
# Requirement window whose floor is the last published crates.io beta (cargo's [patch] mechanics
# require a registry-matching requirement); the code always comes from the patched fork archive.
genai = ">=0.7.0-beta.18, <0.7.0-beta.20"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

[patch.crates-io]
genai = { path = "vendor/genai-0.7.0-beta.19.1-agentprism" }
```

[`tests/fixtures/fresh-consumer/`](tests/fixtures/fresh-consumer/Cargo.toml) is a runnable
reference consumer in exactly this shape, and [`scripts/check-distribution.sh`](scripts/check-distribution.sh)
is the release gate that packages both crates, extracts the archives into a fresh temporary
consumer, applies the patch, builds/tests it, and asserts the resolution pinned `genai` to the
exact fork version from the extracted archive — failing on pin or version drift, missing package
contents, `publish = true`, or documentation implying registry-only installation. No publication
is performed by any step of the workflow.

> **Note:** this release line builds against the [agentprism `genai` fork](https://github.com/agentprism/rust-genai)
> until upstream [PR #277](https://github.com/jeremychone/rust-genai/pull/277) merges. The crate
> uses fork APIs (for example `ToolResponse::parts`, the client-level
> `PayloadInterceptor`/`ResponseObserver` exec hooks plus their request-level `ExecOptions`
> overrides backing `on_payload`/`on_response`, and the response `headers` carried by
> streaming-path HTTP errors — `Error::HttpError.headers` — that the `GenaiStreamFn` retry layer
> reads to honor `retry-after`/`x-should-retry`), so a plain crates.io `genai` version field
> cannot resolve them. The fork version `0.7.0-beta.19.1-agentprism` is pinned to fork commit
> `cee6008346595fcf14f77b53ee3bffe682d651c6` (the `feat/exec-interceptors-error-headers-tool-parts`
> branch head) plus subsequent reviewed fork changes; the gate script re-verifies both the commit
> pin and the exact version on every run.

The feature layers are intentionally small:

| Layer | Cargo feature | Contents |
|---|---|---|
| Runtime | default features | `Agent`, stateless loops, `GenaiStreamFn`, tools, hooks, queues, events, and cancellation |
| Test support | `testing` | `MockStreamFn`, `ScriptedStream`, fixtures, scripted responses, and event recording |
| Proxy transport | `proxy` | `ProxyStreamFn`, authenticated HTTP/SSE client, and the versioned proxy wire types |

Enable only what the consumer needs, for example
`rust-genai-agent = { version = "0.2.0", features = ["proxy"] }`.

## Quick start

`GenaiStreamFn` adapts the normal `genai::Client` streaming API. A subscription is RAII-based, so
retain it for as long as events should be observed; dropping it unsubscribes.

```no_run
use std::io::Write;
use std::sync::Arc;

use genai::Client;
use rust_genai_agent::{
    Agent, AgentConfig, AgentEvent, AgentState, AssistantMessageEvent, GenaiStreamFn,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let state = AgentState {
        model: "gpt-4o-mini".into(),
        system_prompt: "Answer clearly and briefly.".into(),
        ..AgentState::default()
    };

    let agent = Agent::new(
        AgentConfig::default()
            .with_initial_state(state)
            .with_stream_fn(Arc::new(GenaiStreamFn::new(Client::default()))),
    );

    // Keep this value alive until the run has finished.
    let subscription = agent.subscribe_fn(|event, _cancel| async move {
        if let AgentEvent::MessageUpdate {
            assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
            ..
        } = event
        {
            print!("{delta}");
            let _ = std::io::stdout().flush();
        }
    });

    agent.prompt("Why is the sky blue?").await?;
    println!();

    if let Some(error) = agent.state().error_message {
        eprintln!("provider run failed: {error}");
    }

    drop(subscription);
    Ok(())
}
```

`genai` resolves provider credentials from its usual environment variables, such as
`OPENAI_API_KEY`, and resolves the adapter from the model name. See the compileable live examples:
[`c01-basic.rs`](examples/c01-basic.rs), [`c02-tools.rs`](examples/c02-tools.rs), and
[`c03-steering-abort.rs`](examples/c03-steering-abort.rs). Repository tests only compile these
examples; they never contact a provider.

## Choose the right entry point

- **`Agent`** is the application-facing facade. It owns transcript state, tools, runtime options,
  listeners, queues, and one active run. Use `prompt`, `continue_`, `abort`, `wait_for_idle`, and the
  state mutators for interactive applications.
- **`agent_loop` / `agent_loop_continue`** spawn a stateless loop and return an `AgentEventStream`
  plus a cloneable result handle. Use them when the host owns transcript persistence. The
  convenience event stream is an unbounded observational channel; dropping event iteration does
  not stop a retained result handle.
- **`run_agent_loop` / `run_agent_loop_continue`** are the lowest-level awaited forms. They drive an
  `AgentEventSink` in exact order and apply sink backpressure.
- **A custom `StreamFn`** is the only provider boundary. Implement it to connect another backend or
  replay source while preserving the assistant-event protocol. It returns an
  `AssistantMessageEventStream`, not a `Result`, and should finish with exactly one `Done` or `Error`
  terminal event.

An explicit stream function in `AgentConfig` is easiest to reason about. Applications that need a
shared fallback may install one with `set_default_stream_fn(Some(...))`; an agent or low-level loop
with no explicit stream function resolves that process-wide default at run admission.

## Runtime contract

### Tools and turns

`ToolSpec` carries a JSON Schema. Arguments are coerced where the schema permits, validated before
execution, and validation or execution failures become error tool-result messages. Tool batches are
parallel by default; preflight remains in source order, completion events use completion order, and
persisted result messages use source order. A sequential override on any call makes the entire batch
sequential. Updates emitted after a tool settles are ignored. `ThinkingLevel` supports named
reasoning levels and `ThinkingLevel::Budget(u32)` for a provider-specific reasoning-token budget. An
optional `ThinkingBudgets` map resolves a named level to an explicit `ReasoningEffort::Budget` per
run (`xhigh`/`max` clamp through the `high` entry); an unconfigured level falls back to the named
effort, and an explicit `ThinkingLevel::Budget` always bypasses the map. The initial snapshot and
every prepare-next-turn thinking update resolve through the same map
(`resolve_reasoning_effort`). A `Transport` advisory
(`sse`, `websocket`, `websocket-cached`, `auto`; default `auto`) is forwarded onto each
`StreamRequest` for custom stream functions to honor; `GenaiStreamFn` is SSE-only and ignores it.

`session_id`, `max_retries`, and `max_retry_delay_ms` are first-class request fields: `AgentConfig`
snapshots them into each run's `AgentLoopConfig`, and the loop forwards them onto every
`StreamRequest`. The session id is independent of `ChatOptions::prompt_cache_key` — setting or
clearing it never writes the cache key, and an explicit cache key survives construction, runtime
setters, reset, prompt, and continue paths. `GenaiStreamFn` honors the per-request retry fields as
overrides of its construction-time `RetryPolicy` and ignores the session id (genai's explicit
cache-affinity path is `prompt_cache_key`); custom stream functions may honor all three.

Stream-function, hook, `ChatOptions`, tool-execution, and `thinking_budgets` runtime setters are
between-run only. They return `AgentError::Busy` rather than changing configuration during an active
run. `set_transport` is deliberately unguarded, mirroring the TypeScript CLI's live reassignment of
`agent.transport`; a value set mid-run is observed by the next run.

Tool preparation and the before/after tool-call hooks each have a legacy infallible form and an
opt-in fallible form (`AgentTool::try_prepare_arguments`, `TryBeforeToolCallHook`,
`TryAfterToolCallHook`). The fallible form takes precedence when both are installed, and the two are
never both invoked for one call. An `Err` becomes an ordinary in-band error tool result: preparation
and before-hook failures skip execution; an after-hook failure replaces the completed result
(content, details, usage, and any termination request are discarded) without rolling back tool side
effects. `FnTool::with_try_prepare_arguments` is the fallible builder; the legacy builders keep
working unchanged.

### Queues and cancellation

Steering is polled between turns; follow-ups are polled when the loop would otherwise stop. Both
queues default to `QueueMode::OneAtATime`; opt into `QueueMode::All` explicitly. `Agent::abort()`
cancels the active run and is harmless while idle. Lower-level callers pass a
`CancellationToken`, which is also forwarded to tools, hooks, and listeners.

### Event ordering

The loop has one event writer. The awaited low-level sink and `Agent` listeners observe lifecycle
events in emission order; agent listeners are awaited in registration order, including the final
`AgentEnd`. Keep the returned `Subscription` alive. The spawned `agent_loop` convenience stream uses
an unbounded channel so observation cannot stall the loop; consumers with a slow event path should
drain it continuously or use the awaited API.

### Errors stay in band

Provider startup, streaming, protocol, and cancellation failures terminate the assistant stream
with `StopReason::Error` or `StopReason::Aborted`. Tool failures become tool results. Consequently,
`Agent::prompt()` can return `Ok(())` for a completed lifecycle whose assistant message contains an
error; inspect events or `AgentState::error_message`. Returned errors are reserved for admission and
programming conditions such as a busy agent, no configured stream function, or an invalid
continuation. Proxy construction can likewise return a local configuration error before a run.

## Testing and proxying

With `testing`, use `MockStreamFn` and `ScriptedStream` to exercise the real agent loop without API
keys or network access. The repository's parity manifest is checked by
`python3 scripts/check_test_parity.py`; its ordered fragments and aggregate must describe exactly the
same 55 cases.

With `proxy`, `ProxyStreamFn` is a drop-in `StreamFn`. It authenticates to the normalized
`/api/stream` endpoint with a bearer token and reconstructs compact SSE events into the same
assistant stream. The SSE event protocol matches the TypeScript proxy's compact events (snake-case
tags, `contentIndex` fields, the `toolUse` terminal reason), but the request body does not: it is
this crate's own `ProxyRequestV1` schema, not the TypeScript `proxy.ts` request contract (a pi-ai
`Model` object, pi-schema messages, and an eleven-option subset), so a server built for the pi
proxy request contract cannot serve this client without a translation layer. Whitespace-only SSE
data is ignored. HTTP, SSE, wire-protocol, and cancellation failures follow the same in-band
terminal model.

The wire DTO excludes the proxy bearer token and resolved `ServiceTarget` endpoint/auth material;
it is not generically secret-free. `ChatOptions::extra_headers` and `extra_body` are intentionally
forwarded and may contain application secrets. Treat the configured endpoint and HTTP client as a
trusted network/resource boundary. Use HTTPS in production and reserve plain HTTP for loopback or
development. The built-in client disables redirects; a client supplied with `with_client` uses the
caller's redirect policy.

Progressive tool arguments are bounded to nesting depth 128, 1 MiB raw JSON and 4,096 deltas per
tool call, and 16 MiB cumulative reparse work per proxy invocation. Exceeding a cap produces one
partial-preserving in-band protocol error. SSE framing and assistant text buffering remain
unbounded, however, so a hostile endpoint can still consume process resources; the proxy is not a
sandbox for untrusted servers.

## Compatibility and intentional Rust choices

| Area | Contract or deliberate difference |
|---|---|
| Mapped behavior | The pinned non-harness matrix is 55/55 green with no mapped divergence. The M1-M6 checkpoint contained 112 green tests; release-hardening regressions, upstream/production-parity additions (including the `on_payload`/`on_response` exec hooks and the `GenaiStreamFn` retry layer), the core-runtime batch, and the execution-seam batch bring the current all-feature suite to 220/220. |
| Hook snapshots and failure contract | Async hook contexts own cloned transcript/context snapshots (the before-tool hook alone borrows its local context mutably so it can replace arguments). Legacy hook return types are intentionally infallible rather than `Result`-shaped. The opt-in fallible tool channels (`try_prepare_arguments`, `TryBeforeToolCallHook`, `TryAfterToolCallHook`) return `Result<_, ToolHookError>`; an `Err` becomes the call's in-band error tool result with the error's verbatim display text (pi's `error.message` semantics), preparation/before failures skip execution, and an after-hook failure replaces the completed result without rolling back side effects. The fallible channel takes precedence over its legacy counterpart when both are installed; they are never both invoked for one call. A panic is a programming fault: `Agent` synthesizes an in-band failure lifecycle, spawned `agent_loop` reports `LoopError::TaskPanicked`, and direct `run_agent_loop` callers retain normal Rust unwind responsibility. |
| Unbounded observation/update policy | Spawned loop events and internal parallel tool-update handoff are unbounded so observers cannot change execution ordering or deadlock tool tasks. Tool `UpdateSink::emit` is synchronous and returns whether the update was accepted; producers must choose a sensible update rate. Use the awaited loop API when consumer backpressure is required. |
| Same-sink re-entry | `UpdateSink` serializes `emit` and `close` across clones and runs the callback while holding its gate. That callback must not call `emit`, `close`, or `is_closed` on the same sink or a clone; same-sink re-entry would deadlock. |
| Proxy wire protocol | The compact SSE event protocol is TypeScript-compatible (snake-case tags, `contentIndex`, `toolUse`), but the request body is this crate's own `ProxyRequestV1` schema, not the pi `proxy.ts` request contract. Servers implementing the TypeScript request contract cannot serve this client without translation. |
| Proxy trust boundary | Tool-argument parsing is capped, URL userinfo is rejected, and the built-in client does not redirect, but SSE/text buffering is unbounded. Production callers must use HTTPS and trust the endpoint; injected HTTP clients retain their caller-selected redirect policy. Extra request headers/body may carry secrets even though transport and resolved target credentials are excluded from the wire DTO. |
| `genai` foundation limits | `ModelSpec` identifies a target but `genai` carries no context-window or price catalog. Token accounting is native, but monetary cost is opt-in: `AgentUsage::cost` stays `None` unless an application supplies a `PriceCatalog`. User and tool-result images are supported: user images become content parts, and tool-result images attach as `genai::chat::ToolResponse` binary `parts` (an agentprism-fork API until upstream PR #277 merges; see Installation). |
| Cost accounting | `AgentUsage` also carries `cache_write_1h_tokens` and `reasoning_tokens` (both mapped from `genai::chat::Usage` details; genai zero-elides `reasoning_tokens`, so `Some(0)` is unreachable through it). `AgentCost` + the injectable `PriceCatalog` port pi-ai's model-catalog cost step (`models.ts` `calculateCost`): the highest tier whose `input_tokens_above` is strictly below the request's `input + cache_read + cache_write` count prices the whole request. pi-ai's hardcoded Anthropic "1h writes at 2x base input" rule is generalized to an explicit `cache_write_1h` rate (falling back to `cache_write` when unset). Cost is attached at stream finalization by `GenaiStreamFn::with_price_catalog`; `AgentConfig::price_catalog` is a convenience store the application installs on the stream function it builds — the facade constructs no stream functions and applies no catalog itself. |
| Thinking-budget resolution | The `ThinkingBudgets` map mirrors pi-ai's per-level budgets and `clampReasoning` (`xhigh`/`max` → `high`), but has **no implicit default budget table** (an entry must be configured or the named level falls back) and omits pi-ai's maxTokens-fitting step, which is impossible without the model catalog `genai` does not carry. |
| Transport advisory | `Transport` is accepted and forwarded on every `StreamRequest`, but the SSE-only `GenaiStreamFn` ignores it. The TypeScript contract states providers that do not support a requested transport ignore it, so ignoring it is compliant; custom `StreamFn` implementations may honor it. |
| Exec hooks (`on_payload`/`on_response`) | Ports pi's `onPayload`/`onResponse`. `on_payload(payload, model)` may inspect or replace the serialized provider payload before send (return `Some` to replace, `None` to keep); `on_response({status, headers}, model)` observes the HTTP response head — status and headers, never the body — before the body/stream is consumed, including on 4xx/5xx. Both are `AgentConfig`/`StreamRequest` hooks with `Busy`-guarded setters, infallible like the other hooks. The agent loop forwards them onto every `StreamRequest`; custom `StreamFn` implementations and `ProxyStreamFn` honor the per-request hooks directly. `GenaiStreamFn` honors them too: a per-request hook **replaces** the construction-time hook of its channel for that execution via the genai fork's request-level `ExecOptions` overrides (upstream PR #277 lineage) — the two never compose, exactly one hook fires per channel per physical attempt (including retries and HTTP-error responses), and an absent request hook inherits the construction-time default installed via `GenaiStreamFn::with_exec_hooks`. The facade snapshots the configured hooks at run admission, so an idle `set_on_payload`/`set_on_response` replacement takes effect on the next run while an in-flight run keeps its snapshot. |
| Provider retry layer (`RetryPolicy`/`with_retry`) | Ports pi-ai's provider retry policy (`provider-retry.ts`). `GenaiStreamFn::with_retry(RetryPolicy { max_retries, max_retry_delay_ms })` re-issues **only** the initial request/handshake when its in-band terminal error downcasts to a retryable `genai::Error::HttpError`: status `408`/`409`/`429`/`>=500`, or `x-should-retry: true` (with `x-should-retry: false` a hard no-retry override). Delay precedence mirrors pi exactly — `retry-after-ms` > `retry-after` (seconds or HTTP-date) > exponential backoff `min(0.5 * 2^attempt, 8)s` with a `1 - rand*0.25` jitter; a **server-requested** delay above `max_retry_delay_ms` fails fast with pi's byte-exact `Server requested {ceil}s retry delay (max: {ceil}s). {message}`, while the computed backoff is never capped. Only the handshake is wrapped (the layer peeks past the synthetic SSE `Start` to the first real event); once content is emitted no later failure is retried, and the retry-delay sleep is cancellation-aware (a cancel aborts in-band). Defaults are pi's: `max_retries = 0` (retries off — behavior byte-identical to the pre-retry path) and `max_retry_delay_ms = 60000` (`0` disables the cap). Jitter uses a tiny built-in xorshift, adding no `rand`/`fastrand` dependency. `AgentConfig::max_retries`/`max_retry_delay_ms` are forwarded onto every run's `StreamRequest` (pi's per-request stream options); `GenaiStreamFn` treats a request's `Some` values as overrides of its construction-time policy, so configuring them on the agent is sufficient when the stream function is a `GenaiStreamFn` — no duplicate `with_retry` wiring needed. The facade itself still performs no retries. This needs the fork's streaming-error `headers` (`Error::HttpError.headers`; see Installation). |
| API evolution | The public configuration, usage, and error types (`AgentConfig`, `AgentLoopConfig`, `StreamRequest`, `AgentUsage`, `AgentCost`, `ThinkingBudgets`, `Transport`, `AgentError`, `LoopError`) are `#[non_exhaustive]` with complete `with_*` builder coverage, so adding fields or variants stays semver-minor. Construct them through `Default`/`new` plus builders instead of struct literals. `AgentState` is intentionally left exhaustive so applications can keep using functional-update construction (`..AgentState::default()`). |
| Convenience exports | Telemetry helpers and UUID generation from the TypeScript package's convenience exports are intentionally not re-exported. Applications should select their own tracing/telemetry and UUID crates. |
| Excluded packages | `src/harness/**` and `src/node.ts` are outside this crate. There is no durable-session/compaction harness and no Node binding hidden behind a feature. |

## License

Licensed under either of Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT license ([LICENSE-MIT](LICENSE-MIT)), at your option.

[`Agent`]: https://docs.rs/rust-genai-agent/latest/rust_genai_agent/struct.Agent.html
