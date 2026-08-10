//! Authenticated proxy transport and version-frozen wire protocol.
//!
//! Requests cross the boundary through explicit DTOs: neither the proxy bearer token nor a
//! resolved `ModelSpec::Target` endpoint/authentication is serialized. The caller-controlled
//! `extraHeaders` and `extraBody` options are still forwarded as wire data and may contain secrets.
//!
//! [`ProxyStreamOptions`] rejects URL userinfo. Its built-in HTTP client disables redirects, while
//! a client injected with [`ProxyStreamOptions::with_client`] retains the caller's redirect policy.
//! Use HTTPS in production and reserve HTTP for loopback or development; see the
//! [crate README's proxy guidance](https://docs.rs/crate/rust-genai-agent/latest#testing-and-proxying).
//!
//! Whitespace-only SSE `data` frames are ignored. Transport, HTTP, decoding, protocol, resource,
//! and cancellation failures become partial-preserving in-band assistant terminals, and the
//! returned assistant stream fuses after its first terminal event. Compact events are validated
//! for ordering, dense content indexes, and block lifecycles while partial snapshots are rebuilt.
//!
//! Streaming tool-argument parsing accepts at most 128 nested JSON containers, 1 MiB of raw JSON
//! and 4,096 deltas (including empty deltas) per tool call, plus 16 MiB of cumulative reparse work
//! per invocation. SSE event/text framing and accumulated assistant text remain unbounded, so the
//! endpoint must still be trusted as a network and resource boundary.
//!
//! The compact EVENT protocol matches the TypeScript proxy: snake-case event tags, camel-case
//! event fields such as `contentIndex`, and the `toolUse` tool terminal reason. The REQUEST body
//! does not: it is this crate's own version-one schema ([`ProxyRequestV1`]) built from `genai`
//! types, not the TypeScript `proxy.ts` request contract (a pi-ai `Model` object, the pi message
//! schema, and an eleven-option `SimpleStreamOptions` subset). A server implementing the pi proxy
//! request contract cannot serve this client without a translation layer.

mod accumulator;
mod client;
mod options;
mod wire;

pub use client::{ProxyStreamFn, stream_proxy};
pub use options::{ProxyConfigError, ProxyStreamOptions};
pub use wire::*;
