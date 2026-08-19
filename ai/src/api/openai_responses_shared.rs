//! PORT TARGET ⇐ pi `src/api/openai-responses-shared.ts`.
//!
//! No I/O lives here: message/tool conversion into Responses input shapes and the semantic
//! event-processing loop (`processResponsesStream`) consumed by `openai_responses`,
//! azure (not ported), and both Codex transports.
