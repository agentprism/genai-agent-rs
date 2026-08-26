//! Executor-neutral cancellation with parent-to-child propagation from
//! Architecture v2 part 2 §9.5.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};

/// Error returned when an executor-neutral operation observes cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancellationError;

impl std::fmt::Display for CancellationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("operation cancelled")
    }
}

impl std::error::Error for CancellationError {}

/// A cloneable, executor-neutral cancellation signal.
///
/// Cancelling a token wakes all futures currently waiting on it and propagates
/// to every live child. Cancelling a child does not cancel its parent or
/// siblings.
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<CancellationState>,
}

struct CancellationState {
    cancelled: AtomicBool,
    next_waiter: AtomicU64,
    wakers: Mutex<Vec<(u64, Waker)>>,
    children: Mutex<Vec<Weak<CancellationState>>>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    /// Creates a token in the non-cancelled state.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                next_waiter: AtomicU64::new(1),
                wakers: Mutex::new(Vec::new()),
                children: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Cancels this token and all of its current descendants.
    ///
    /// The operation is idempotent. Every waiter registered before or during
    /// cancellation is either woken or observes the cancelled flag directly.
    pub fn cancel(&self) {
        cancel_state(&self.inner);
    }

    /// Returns whether this token has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// Returns an error when cancellation has already been requested.
    pub fn check(&self) -> Result<(), CancellationError> {
        if self.is_cancelled() {
            Err(CancellationError)
        } else {
            Ok(())
        }
    }

    /// Returns a future that resolves when this token is cancelled.
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            token: self,
            registration: None,
        }
    }

    /// Creates a child that is cancelled by this token but can also be
    /// cancelled independently.
    pub fn child(&self) -> Self {
        let child = Self::new();
        let mut children = lock_unpoisoned(&self.inner.children);
        if self.is_cancelled() {
            drop(children);
            child.cancel();
        } else {
            children.retain(|state| state.strong_count() > 0);
            children.push(Arc::downgrade(&child.inner));
        }
        child
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Future returned by [`CancellationToken::cancelled`].
#[derive(Debug)]
pub struct Cancelled<'a> {
    token: &'a CancellationToken,
    registration: Option<u64>,
}

impl Future for Cancelled<'_> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }

        let mut wakers = lock_unpoisoned(&self.token.inner.wakers);
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        match self.registration {
            Some(registration) => {
                if let Some((_, waker)) = wakers
                    .iter_mut()
                    .find(|(candidate, _)| *candidate == registration)
                    && !waker.will_wake(context.waker())
                {
                    *waker = context.waker().clone();
                }
            }
            None => {
                let registration = self.token.inner.next_waiter.fetch_add(1, Ordering::Relaxed);
                wakers.push((registration, context.waker().clone()));
                drop(wakers);
                self.registration = Some(registration);
                return Poll::Pending;
            }
        }
        Poll::Pending
    }
}

impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        let Some(registration) = self.registration else {
            return;
        };
        lock_unpoisoned(&self.token.inner.wakers)
            .retain(|(candidate, _)| *candidate != registration);
    }
}

fn cancel_state(state: &Arc<CancellationState>) {
    if state.cancelled.swap(true, Ordering::AcqRel) {
        return;
    }

    let children = {
        let mut registered = lock_unpoisoned(&state.children);
        let live = registered
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        registered.clear();
        live
    };
    let wakers = std::mem::take(&mut *lock_unpoisoned(&state.wakers));

    for child in children {
        cancel_state(&child);
    }
    for (_, waker) in wakers {
        waker.wake();
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
