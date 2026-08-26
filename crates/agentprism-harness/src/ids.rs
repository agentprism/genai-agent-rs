//! Host-injectable identifiers used by durable harness operations.

use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe source of stable harness identifiers.
pub trait HarnessIdGenerator: Send + Sync + 'static {
    /// Allocates an open identifier with a diagnostic kind prefix.
    fn next_id(&self, kind: &'static str) -> String;
}

/// Single-threaded counterpart of [`HarnessIdGenerator`].
pub trait LocalHarnessIdGenerator: 'static {
    /// Allocates an open identifier with a diagnostic kind prefix.
    fn next_id(&self, kind: &'static str) -> String;
}

/// Deterministic monotonic identifier source suitable for native hosts and tests.
#[derive(Debug)]
pub struct MonotonicHarnessIdGenerator {
    namespace: String,
    next: AtomicU64,
}

impl MonotonicHarnessIdGenerator {
    /// Creates a generator whose first identifier ends in `1`.
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            next: AtomicU64::new(1),
        }
    }
}

impl HarnessIdGenerator for MonotonicHarnessIdGenerator {
    fn next_id(&self, kind: &'static str) -> String {
        let value = self.next.fetch_add(1, Ordering::Relaxed);
        format!("{}-{kind}-{value}", self.namespace)
    }
}

impl LocalHarnessIdGenerator for MonotonicHarnessIdGenerator {
    fn next_id(&self, kind: &'static str) -> String {
        HarnessIdGenerator::next_id(self, kind)
    }
}
