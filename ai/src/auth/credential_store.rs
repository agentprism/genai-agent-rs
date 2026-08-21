//! In-memory credential store ⇐ pi `src/auth/credential-store.ts`.

use super::types::{
    AuthError, AuthFuture, AuthOperationOptions, Credential, CredentialInfo, CredentialModify,
    CredentialStore,
};
use crate::types::AbortSignal;
use crate::utils::abort::{abort_reason, race_with_abort_signal};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Default)]
struct StoreInner {
    credentials: AsyncMutex<IndexMap<String, Credential>>,
    locks: Mutex<HashMap<String, Arc<AsyncMutex<()>>>>,
}

#[derive(Clone, Default)]
pub struct InMemoryCredentialStore {
    inner: Arc<StoreInner>,
}

fn aborted(signal: Option<&Arc<dyn AbortSignal>>) -> Result<(), AuthError> {
    if let Some(signal) = signal.filter(|signal| signal.is_aborted()) {
        return Err(AuthError::abort(abort_reason(signal.as_ref())));
    }
    Ok(())
}

impl InMemoryCredentialStore {
    fn provider_lock(&self, provider_id: &str) -> Arc<AsyncMutex<()>> {
        self.inner
            .locks
            .lock()
            .expect("credential lock map poisoned")
            .entry(provider_id.to_owned())
            .or_default()
            .clone()
    }

    fn release_provider_lock(inner: &StoreInner, provider_id: &str, lock: &Arc<AsyncMutex<()>>) {
        let mut locks = inner.locks.lock().expect("credential lock map poisoned");
        if Arc::strong_count(lock) == 2
            && locks
                .get(provider_id)
                .is_some_and(|current| Arc::ptr_eq(current, lock))
        {
            locks.remove(provider_id);
        }
    }

    async fn observe_abort<T: Send + 'static>(
        operation: impl std::future::Future<Output = Result<T, AuthError>> + Send + 'static,
        signal: Option<Arc<dyn AbortSignal>>,
    ) -> Result<T, AuthError> {
        if let Some(signal) = signal {
            race_with_abort_signal(operation, signal)
                .await
                .map_err(AuthError::abort)?
        } else {
            operation.await
        }
    }
}

impl CredentialStore for InMemoryCredentialStore {
    fn read(
        &self,
        provider_id: String,
        options: AuthOperationOptions,
    ) -> AuthFuture<Option<Credential>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            aborted(options.signal.as_ref())?;
            Ok(inner.credentials.lock().await.get(&provider_id).cloned())
        })
    }

    fn list(&self, options: AuthOperationOptions) -> AuthFuture<Vec<CredentialInfo>> {
        let inner = self.inner.clone();
        Box::pin(async move {
            aborted(options.signal.as_ref())?;
            Ok(inner
                .credentials
                .lock()
                .await
                .iter()
                .map(|(provider_id, credential)| CredentialInfo {
                    provider_id: provider_id.clone(),
                    kind: credential.auth_type(),
                })
                .collect())
        })
    }

    fn modify(
        &self,
        provider_id: String,
        modify: CredentialModify,
        options: AuthOperationOptions,
    ) -> AuthFuture<Option<Credential>> {
        let inner = self.inner.clone();
        let lock = self.provider_lock(&provider_id);
        Box::pin(async move {
            let operation_signal = options.signal.clone();
            let operation = async move {
                let result = {
                    let _guard = lock.lock().await;
                    async {
                        aborted(operation_signal.as_ref())?;
                        let current = inner.credentials.lock().await.get(&provider_id).cloned();
                        let next = modify(current.clone()).await?;
                        aborted(operation_signal.as_ref())?;
                        if let Some(next) = next.clone() {
                            inner
                                .credentials
                                .lock()
                                .await
                                .insert(provider_id.clone(), next);
                        }
                        Ok(next.or(current))
                    }
                    .await
                };
                Self::release_provider_lock(&inner, &provider_id, &lock);
                result
            };
            Self::observe_abort(operation, options.signal).await
        })
    }

    fn delete(&self, provider_id: String, options: AuthOperationOptions) -> AuthFuture<()> {
        let inner = self.inner.clone();
        let lock = self.provider_lock(&provider_id);
        Box::pin(async move {
            let operation_signal = options.signal.clone();
            let operation = async move {
                let result = {
                    let _guard = lock.lock().await;
                    async {
                        aborted(operation_signal.as_ref())?;
                        inner.credentials.lock().await.shift_remove(&provider_id);
                        Ok(())
                    }
                    .await
                };
                Self::release_provider_lock(&inner, &provider_id, &lock);
                result
            };
            Self::observe_abort(operation, options.signal).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::types::{ApiKeyCredential, ApiKeyCredentialType};
    use crate::utils::abort::{AbortController, AbortReason};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::oneshot;

    fn credential(key: &str) -> Credential {
        Credential::ApiKey(ApiKeyCredential {
            kind: ApiKeyCredentialType::ApiKey,
            key: Some(key.to_owned()),
            env: None,
        })
    }

    #[tokio::test]
    async fn modify_is_serialized_and_undefined_preserves_current() {
        let store = InMemoryCredentialStore::default();
        store
            .modify(
                "p".to_owned(),
                Box::new(|_| Box::pin(async { Ok(Some(credential("one"))) })),
                AuthOperationOptions::default(),
            )
            .await
            .expect("write");
        let post = store
            .modify(
                "p".to_owned(),
                Box::new(|current| {
                    Box::pin(async move {
                        assert_eq!(current, Some(credential("one")));
                        Ok(None)
                    })
                }),
                AuthOperationOptions::default(),
            )
            .await
            .expect("modify");
        assert_eq!(post, Some(credential("one")));

        store
            .modify(
                "q".to_owned(),
                Box::new(|_| Box::pin(async { Ok(Some(credential("two"))) })),
                AuthOperationOptions::default(),
            )
            .await
            .expect("write");
        assert_eq!(
            store
                .list(AuthOperationOptions::default())
                .await
                .expect("list")
                .into_iter()
                .map(|entry| entry.provider_id)
                .collect::<Vec<_>>(),
            ["p", "q"]
        );
    }

    /// Ports pi `test/models-runtime.test.ts:704-733`.
    #[tokio::test]
    async fn aborting_a_queued_modify_prevents_its_callback() {
        let store = InMemoryCredentialStore::default();
        let (started_sender, started_receiver) = oneshot::channel();
        let (finish_sender, finish_receiver) = oneshot::channel();
        let first = tokio::spawn(store.modify(
            "p".to_owned(),
            Box::new(move |_| {
                Box::pin(async move {
                    let _ = started_sender.send(());
                    let _ = finish_receiver.await;
                    Ok(Some(credential("first")))
                })
            }),
            AuthOperationOptions::default(),
        ));
        started_receiver.await.expect("first modify started");

        let second_ran = Arc::new(AtomicBool::new(false));
        let callback_ran = second_ran.clone();
        let controller = AbortController::new();
        let second = store.modify(
            "p".to_owned(),
            Box::new(move |_| {
                callback_ran.store(true, Ordering::SeqCst);
                Box::pin(async { Ok(Some(credential("second"))) })
            }),
            AuthOperationOptions {
                signal: Some(controller.signal()),
            },
        );
        controller.abort(AbortReason::new("AbortError", "stop"));
        let error = second.await.expect_err("queued modify aborted");
        assert_eq!(error.name, "AbortError");
        assert_eq!(error.message, "stop");

        let _ = finish_sender.send(());
        first
            .await
            .expect("first task joined")
            .expect("first modify");
        tokio::task::yield_now().await;
        assert!(!second_ran.load(Ordering::SeqCst));
        assert_eq!(
            store
                .read("p".to_owned(), AuthOperationOptions::default())
                .await
                .expect("read"),
            Some(credential("first"))
        );
    }
}
