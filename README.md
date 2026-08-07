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
mapped cases; the current suite is **128/128** green.

## Installation

The minimum supported Rust version is 1.88.

```toml
[dependencies]
rust-genai-agent = "0.1.0"
genai = "0.7.0-beta.18"
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

The feature layers are intentionally small:

| Layer | Cargo feature | Contents |
|---|---|---|
| Runtime | default features | `Agent`, stateless loops, `GenaiStreamFn`, tools, hooks, queues, events, and cancellation |
| Test support | `testing` | `MockStreamFn`, `ScriptedStream`, fixtures, scripted responses, and event recording |
| Proxy transport | `proxy` | `ProxyStreamFn`, authenticated HTTP/SSE client, and the versioned proxy wire types |

Enable only what the consumer needs, for example
`rust-genai-agent = { version = "0.1.0", features = ["proxy"] }`.

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
reasoning levels and `ThinkingLevel::Budget(u32)` for a provider-specific reasoning-token budget.

Stream-function, hook, `ChatOptions`, and tool-execution runtime setters are between-run only. They
return `AgentError::Busy` rather than changing configuration during an active run.

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
| Mapped behavior | The pinned non-harness matrix is 55/55 green with no mapped divergence. The M1-M6 checkpoint contained 112 green tests; release-hardening regressions and upstream-parity additions bring the current all-feature suite to 128/128. |
| Hook snapshots and failure contract | Async hook contexts own cloned transcript/context snapshots (the before-tool hook alone borrows its local context mutably so it can replace arguments). Hook return types are intentionally infallible rather than `Result`-shaped. A panic is a programming fault: `Agent` synthesizes an in-band failure lifecycle, spawned `agent_loop` reports `LoopError::TaskPanicked`, and direct `run_agent_loop` callers retain normal Rust unwind responsibility. |
| Unbounded observation/update policy | Spawned loop events and internal parallel tool-update handoff are unbounded so observers cannot change execution ordering or deadlock tool tasks. Tool `UpdateSink::emit` is synchronous and returns whether the update was accepted; producers must choose a sensible update rate. Use the awaited loop API when consumer backpressure is required. |
| Same-sink re-entry | `UpdateSink` serializes `emit` and `close` across clones and runs the callback while holding its gate. That callback must not call `emit`, `close`, or `is_closed` on the same sink or a clone; same-sink re-entry would deadlock. |
| Proxy wire protocol | The compact SSE event protocol is TypeScript-compatible (snake-case tags, `contentIndex`, `toolUse`), but the request body is this crate's own `ProxyRequestV1` schema, not the pi `proxy.ts` request contract. Servers implementing the TypeScript request contract cannot serve this client without translation. |
| Proxy trust boundary | Tool-argument parsing is capped, URL userinfo is rejected, and the built-in client does not redirect, but SSE/text buffering is unbounded. Production callers must use HTTPS and trust the endpoint; injected HTTP clients retain their caller-selected redirect policy. Extra request headers/body may carry secrets even though transport and resolved target credentials are excluded from the wire DTO. |
| `genai` foundation limits | `ModelSpec` identifies a target but does not provide a catalog of context-window or price metadata. `AgentUsage` therefore tracks tokens, not monetary cost. User images are supported, but `genai::chat::ToolResponse` is text-only, so tool-result images become an `[image omitted]` marker when converted back to the model. |
| Convenience exports | Telemetry helpers and UUID generation from the TypeScript package's convenience exports are intentionally not re-exported. Applications should select their own tracing/telemetry and UUID crates. |
| Excluded packages | `src/harness/**` and `src/node.ts` are outside this crate. There is no durable-session/compaction harness and no Node binding hidden behind a feature. |

## License

Licensed under either of Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT license ([LICENSE-MIT](LICENSE-MIT)), at your option.

[`Agent`]: https://docs.rs/rust-genai-agent/latest/rust_genai_agent/struct.Agent.html
