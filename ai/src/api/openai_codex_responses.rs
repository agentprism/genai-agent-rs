//! PORT TARGET ⇐ pi `src/api/openai-codex-responses.ts`.
//!
//! Fully hand-rolled in pi (types-only `openai` import): local request body/auth/session
//! headers, local fetch POST + SSE parser, WebSocket / `websocket-cached` / `auto`
//! transports (default `auto`) with pre-stream SSE fallback and session socket caching
//! (5 min idle / 55 min total), zstd request compression on the SSE path when available,
//! and its own retry loop (Retry-After / Retry-After-Ms, exponential backoff, zero-retry
//! default).
//!
//! Rust transport: lift the fork's `genai/src/codex/` module (pi-line-cited WS+SSE port —
//! protocol, token handling, transport lifecycle), collapsing its internal
//! wire → `ChatStreamEvent` → assistant-event double hop to wire → assistant events
//! directly through `openai_responses_shared`.
