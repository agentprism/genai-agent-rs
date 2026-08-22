# The goal

**Build a Rust crate that is pi-ai, and then a Rust crate that is pi-agent-core on top of it — behaviorally the same libraries, written as idiomatic Rust.**

Two requirements, and they are not in tension:

1. **Idiomatic Rust is about how the code is written.** Ownership, `Result`, traits, async, real types — not a transliteration of TypeScript, and not a Rust program pretending to be a JavaScript one. The crate never impersonates the JS runtime or the JS SDKs: no spawning `node`, no fabricated runtime versions, no JS-isms kept for their own sake. Its identity on the wire and in its outputs is truthful.

2. **Behavioral parity is about what the code does.** Every feature and every observable behavior of pi-ai exists in the crate: the public surface, the event protocol (including what each event carries), what is sent to providers, how responses, errors, retries, aborts, and hooks behave, what is persisted and how it reads back, the catalog, auth resolution, OAuth flows. The litmus test is the one we just ran: **any pi-ai README example or test must be recreatable against the crate with the same observable results, without needing a workaround and without reading the Rust internals.** If pi's quickstart reads `event.partial`, the Rust event has the partial.

When the two seem to conflict, parity wins and idiom adapts. A feature is never dropped because the idiomatic or efficient Rust shape is less convenient — you find the idiomatic way to provide the same behavior (an `Arc<AssistantMessage>` snapshot on every event costs nothing a JS shared reference doesn't). Performance, "the proxy strips it anyway," or an earlier design note are not grounds for removing something a consumer can observe.

**What is free:** internals. An SDK's own framing, a different HTTP stack, how a snapshot is produced, memory strategy, module-private structure. The bar for anything SDK-backed is observable equivalence with what pi does with its SDK.

**What is not free:** silent deviation. Anything that genuinely cannot be preserved in Rust is reported as a four-column row for you to judge; the reviewer rejects any row Rust can in fact achieve. Docs and prior decisions are background — pi's pinned source is the only authority, and a doc that contradicts it is wrong, not binding.

**Why the bar is this high:** pi-agent-core is a *consumer* of pi-ai. It will be ported next, on top of this crate, by the same standard — so every pi-ai surface pi-agent-core touches has to already be there, behaving identically, or the next port inherits the gap.
