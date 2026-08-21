//! Pluggable model-catalog storage ⇐ pi `src/models-store.ts`.

use crate::types::{AbortSignal, Model};
use crate::utils::abort::{AbortReason, abort_reason};
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, PartialEq)]
pub struct ModelsStoreEntry {
    pub models: Vec<Model>,
    pub last_modified: Option<f64>,
    pub checked_at: Option<f64>,
    pub etag: Option<String>,
}

#[derive(Clone, Default)]
pub struct ModelsStoreOperationOptions {
    pub signal: Option<Arc<dyn AbortSignal>>,
}

pub trait ModelsStore: Send + Sync {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
        options: ModelsStoreOperationOptions,
    ) -> BoxFuture<'a, Result<Option<ModelsStoreEntry>, AbortReason>>;
    fn write<'a>(
        &'a self,
        provider_id: &'a str,
        entry: ModelsStoreEntry,
        options: ModelsStoreOperationOptions,
    ) -> BoxFuture<'a, Result<(), AbortReason>>;
    fn delete<'a>(
        &'a self,
        provider_id: &'a str,
        options: ModelsStoreOperationOptions,
    ) -> BoxFuture<'a, Result<(), AbortReason>>;
}

#[derive(Default)]
pub struct InMemoryModelsStore {
    entries: RwLock<HashMap<String, ModelsStoreEntry>>,
}

fn check_signal(options: &ModelsStoreOperationOptions) -> Result<(), AbortReason> {
    if let Some(signal) = options
        .signal
        .as_deref()
        .filter(|signal| signal.is_aborted())
    {
        return Err(abort_reason(signal));
    }
    Ok(())
}

impl ModelsStore for InMemoryModelsStore {
    fn read<'a>(
        &'a self,
        provider_id: &'a str,
        options: ModelsStoreOperationOptions,
    ) -> BoxFuture<'a, Result<Option<ModelsStoreEntry>, AbortReason>> {
        Box::pin(async move {
            check_signal(&options)?;
            Ok(self
                .entries
                .read()
                .expect("models store poisoned")
                .get(provider_id)
                .cloned())
        })
    }

    fn write<'a>(
        &'a self,
        provider_id: &'a str,
        entry: ModelsStoreEntry,
        options: ModelsStoreOperationOptions,
    ) -> BoxFuture<'a, Result<(), AbortReason>> {
        Box::pin(async move {
            check_signal(&options)?;
            self.entries
                .write()
                .expect("models store poisoned")
                .insert(provider_id.to_owned(), entry.clone());
            Ok(())
        })
    }

    fn delete<'a>(
        &'a self,
        provider_id: &'a str,
        options: ModelsStoreOperationOptions,
    ) -> BoxFuture<'a, Result<(), AbortReason>> {
        Box::pin(async move {
            check_signal(&options)?;
            self.entries
                .write()
                .expect("models store poisoned")
                .remove(provider_id);
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::abort::{AbortController, AbortReason};

    /// Pins pi `src/models-store.ts:37-55` cloning and pre-abort behavior.
    #[tokio::test]
    async fn in_memory_store_clones_entries_and_honors_abort() {
        let store = InMemoryModelsStore::default();
        let mut entry = ModelsStoreEntry {
            models: Vec::new(),
            last_modified: Some(1.5),
            checked_at: Some(2.5),
            etag: Some("\"opaque\"".to_owned()),
        };
        store
            .write(
                "provider",
                entry.clone(),
                ModelsStoreOperationOptions::default(),
            )
            .await
            .expect("write");
        entry.etag = Some("changed".to_owned());
        let read = store
            .read("provider", ModelsStoreOperationOptions::default())
            .await
            .expect("read")
            .expect("entry");
        assert_eq!(read.etag.as_deref(), Some("\"opaque\""));

        let controller = AbortController::new();
        controller.abort(AbortReason::new("CustomAbort", "stop"));
        let error = store
            .delete(
                "provider",
                ModelsStoreOperationOptions {
                    signal: Some(controller.signal()),
                },
            )
            .await
            .expect_err("aborted");
        assert_eq!(error, AbortReason::new("CustomAbort", "stop"));
        assert!(
            store
                .read("provider", ModelsStoreOperationOptions::default())
                .await
                .expect("read")
                .is_some()
        );
    }
}
