//! Session-scoped transport cleanup registry ⇐ pi `src/session-resources.ts`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub type SessionResourceCleanup = Arc<dyn Fn(Option<&str>) -> Result<(), String> + Send + Sync>;

fn cleanups() -> &'static Mutex<BTreeMap<u64, SessionResourceCleanup>> {
    static CLEANUPS: OnceLock<Mutex<BTreeMap<u64, SessionResourceCleanup>>> = OnceLock::new();
    CLEANUPS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

pub fn register_session_resource_cleanup(cleanup: SessionResourceCleanup) -> u64 {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    cleanups()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(id, cleanup);
    id
}

pub fn unregister_session_resource_cleanup(id: u64) {
    cleanups()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(&id);
}

pub fn cleanup_session_resources(session_id: Option<&str>) -> Result<(), Vec<String>> {
    let callbacks = cleanups()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let errors = callbacks
        .into_iter()
        .filter_map(|cleanup| cleanup(session_id).err())
        .collect::<Vec<_>>();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn registers_runs_and_unregisters_cleanup() {
        let calls = Arc::new(AtomicUsize::new(0));
        let captured = calls.clone();
        let id = register_session_resource_cleanup(Arc::new(move |session_id| {
            assert_eq!(session_id, Some("session"));
            captured.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }));
        cleanup_session_resources(Some("session")).expect("cleanup");
        unregister_session_resource_cleanup(id);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
