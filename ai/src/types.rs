//! PORT TARGET ⇐ pi `src/types.ts`. Every item below is a placeholder shell that exists only
//! so the `api` seam compiles; the real port replaces this file wholesale. Fidelity notes that
//! bind the port (seams doc v2): `Api`/`ProviderId` are open unions, never closed enums
//! (seam #10); assistant content carries the opaque round-trip fidelity fields
//! (`thinking_signature`, `thought_signature`, `text_signature`, `reasoning_details`,
//! `redacted`, `response_id`, `raw_stop_reason`) untouched (seam #11); presence-bearing
//! options distinguish unset from explicit values.

use serde::{Deserialize, Serialize};

/// Open union of wire-protocol ids (pi `Api = KnownApi | (string & {})`, types.ts).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Api(pub String);

/// PORT TARGET shell (pi `Model<TApi>`).
#[derive(Debug, Clone)]
pub struct Model {
    pub id: String,
    pub api: Api,
    pub base_url: Option<String>,
}

/// PORT TARGET shell (pi `Context`): provider-neutral conversation input.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
}

/// PORT TARGET shell (pi `Message` union).
#[derive(Debug, Clone)]
pub struct Message;

/// PORT TARGET shell (pi `Tool`): schema data, not code (seam #11).
#[derive(Debug, Clone)]
pub struct Tool;

/// PORT TARGET shell (pi `AssistantMessage`).
#[derive(Debug, Clone)]
pub struct AssistantMessage;

/// PORT TARGET shell (pi `StreamOptions`): the full per-request tier.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions;

/// PORT TARGET shell (pi `SimpleStreamOptions`): the provider-neutral tier —
/// the only tier the agent layer consumes (seam #8).
#[derive(Debug, Clone, Default)]
pub struct SimpleStreamOptions;

/// PORT TARGET shell (pi `DeferredHandle`): plain serializable data — the harness persists
/// it across process restarts, so no live objects inside (seam #1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeferredHandle;

/// PORT TARGET shell (pi `DeferredFetchOptions`).
#[derive(Debug, Clone, Default)]
pub struct DeferredFetchOptions;

/// PORT TARGET shell (pi `DeferredCancelOptions`).
#[derive(Debug, Clone, Default)]
pub struct DeferredCancelOptions;
