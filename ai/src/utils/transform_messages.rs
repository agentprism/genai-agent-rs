//! PORT TARGET ⇐ pi `src/api/transform-messages.ts` (seam #11).
//!
//! The shared lowering pass every API implementation runs before building its request:
//! same-model turns replay thinking verbatim (signatures included); cross-model turns
//! convert thinking to `<thinking>` text and drop redacted blocks; tool-call ids are
//! normalized per target-API constraints; errored/aborted assistant turns are dropped;
//! orphaned tool calls get synthetic error results; images degrade to text placeholders
//! for non-vision models. A pure function — this pass plus the fidelity fields IS the
//! cross-provider handoff feature.
