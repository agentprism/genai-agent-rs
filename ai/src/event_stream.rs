//! PORT TARGET ⇐ pi `src/utils/event-stream.ts` (+ the v2 seams-doc ruling on seam #5).
//!
//! Binding decisions from the seams doc:
//! - Events are `content_index`-addressed block deltas (`start`, then
//!   `{text,thinking,toolcall}_{start,delta,end}`, then terminal `done`/`error`).
//! - The partial-free protocol is canonical: events do NOT carry a `partial` snapshot;
//!   a `MessageBuilder::apply(event)` accumulator serves consumers that want snapshots.
//! - Single-consumer iteration + an independently awaitable terminal `result()`.
//! - Terminal failures are in-band `error` events carrying a complete `AssistantMessage`
//!   with stop reason `error`/`aborted` — never a `Result::Err` (seam #12).
//!
//! The fork's `genai/src/assistant_stream.rs` (terminal-authoritative result handles,
//! exactly-once settlement) is the lift candidate for the stream primitive; it must be
//! audited against current pi main and reshaped to the partial-free canon during the port.

use crate::types::AssistantMessage;

/// PORT TARGET shell (pi `AssistantMessageEvent`).
#[derive(Debug, Clone)]
pub struct AssistantMessageEvent;

/// PORT TARGET shell (pi `AssistantMessageEventStream`): a
/// `Stream<Item = AssistantMessageEvent>` plus a terminal-result handle.
pub struct AssistantMessageEventStream;

impl AssistantMessageEventStream {
    /// PORT TARGET shell (pi `EventStream.result()`).
    pub async fn result(&self) -> AssistantMessage {
        unimplemented!("port target: utils/event-stream.ts result()")
    }
}
