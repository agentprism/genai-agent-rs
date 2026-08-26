//! Passive harness lifecycle events and gap-free snapshot watches.

use pi_agent_session::{EntryId, LaneName};
use pi_ai::RunId;
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard},
};

/// Stable harness lifecycle event discriminant.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum HarnessEventType {
    /// A durable run operation started.
    RunStart,
    /// A durable run operation finished.
    RunEnd,
}

/// Terminal classification exposed by pinned Pi's `run_end` event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessRunOutcome {
    /// The run completed normally or by tool termination.
    Completed,
    /// The run was cancelled.
    Aborted,
    /// The run failed.
    Failed,
}

/// Passive top-level harness event from pinned Pi's `harness/events.ts`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    /// Begins one durably recorded run operation.
    RunStart {
        /// Selected durable lane.
        lane: LaneName,
        /// Durable operation identity.
        #[serde(rename = "runId")]
        run_id: RunId,
    },
    /// Finishes one durably recorded run operation.
    RunEnd {
        /// Selected durable lane.
        lane: LaneName,
        /// Durable operation identity.
        #[serde(rename = "runId")]
        run_id: RunId,
        /// Terminal operation classification.
        outcome: HarnessRunOutcome,
        /// Durable branch leaf after the operation.
        #[serde(rename = "leafId")]
        leaf_id: EntryId,
    },
}

impl HarnessEvent {
    /// Returns the stable event discriminant.
    pub fn event_type(&self) -> HarnessEventType {
        match self {
            Self::RunStart { .. } => HarnessEventType::RunStart,
            Self::RunEnd { .. } => HarnessEventType::RunEnd,
        }
    }
}

type Listener = Arc<dyn Fn(&HarnessEvent) + Send + Sync + 'static>;

#[derive(Default)]
struct EventBusState {
    next_listener_id: u64,
    listeners: BTreeMap<u64, (HarnessEventType, Listener)>,
    watchers: BTreeMap<u64, Arc<Mutex<WatchState>>>,
}

#[derive(Default)]
struct WatchState {
    listener: Option<Listener>,
    buffered: Vec<HarnessEvent>,
}

/// Cloneable passive event registry.
///
/// Direct listeners receive only future matching events. Watches register
/// before capturing their snapshot, buffer events until `start`, and therefore
/// have no snapshot-to-stream observation gap. Listener futures are
/// deliberately absent: pinned Pi's harness event bus is observational and
/// does not make run completion wait on listeners.
#[derive(Clone, Default)]
pub struct HarnessEventBus {
    inner: Arc<Mutex<EventBusState>>,
}

impl HarnessEventBus {
    /// Creates an empty event registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one passive listener and returns an owned subscription.
    pub fn on(
        &self,
        event_type: HarnessEventType,
        listener: impl Fn(&HarnessEvent) + Send + Sync + 'static,
    ) -> HarnessEventSubscription {
        let mut state = lock_unpoisoned(&self.inner);
        let id = state.next_listener_id;
        state.next_listener_id = state.next_listener_id.saturating_add(1);
        state.listeners.insert(id, (event_type, Arc::new(listener)));
        HarnessEventSubscription {
            bus: self.clone(),
            id: Some(id),
        }
    }

    /// Registers a watcher before capturing its snapshot.
    pub fn watch<T>(&self, capture_snapshot: impl FnOnce() -> T) -> HarnessWatch<T> {
        let watch_state = Arc::new(Mutex::new(WatchState::default()));
        let id = {
            let mut state = lock_unpoisoned(&self.inner);
            let id = state.next_listener_id;
            state.next_listener_id = state.next_listener_id.saturating_add(1);
            state.watchers.insert(id, watch_state.clone());
            id
        };
        let snapshot = capture_snapshot();
        HarnessWatch {
            snapshot,
            bus: self.clone(),
            id: Some(id),
            state: watch_state,
        }
    }

    /// Publishes one event synchronously to current listeners and watchers.
    pub fn emit(&self, event: HarnessEvent) {
        let (listeners, watchers) = {
            let state = lock_unpoisoned(&self.inner);
            let listeners = state
                .listeners
                .values()
                .filter(|(event_type, _)| *event_type == event.event_type())
                .map(|(_, listener)| listener.clone())
                .collect::<Vec<_>>();
            let watchers = state.watchers.values().cloned().collect::<Vec<_>>();
            (listeners, watchers)
        };
        for listener in listeners {
            listener(&event);
        }
        for watcher in watchers {
            let listener = {
                let mut watcher = lock_unpoisoned(&watcher);
                match watcher.listener.clone() {
                    Some(listener) => Some(listener),
                    None => {
                        watcher.buffered.push(event.clone());
                        None
                    }
                }
            };
            if let Some(listener) = listener {
                listener(&event);
            }
        }
    }

    fn remove(&self, id: u64) {
        let mut state = lock_unpoisoned(&self.inner);
        state.listeners.remove(&id);
        state.watchers.remove(&id);
    }
}

impl fmt::Debug for HarnessEventBus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = lock_unpoisoned(&self.inner);
        formatter
            .debug_struct("HarnessEventBus")
            .field("listener_count", &state.listeners.len())
            .field("watcher_count", &state.watchers.len())
            .finish()
    }
}

/// Owned direct-listener registration.
pub struct HarnessEventSubscription {
    bus: HarnessEventBus,
    id: Option<u64>,
}

impl HarnessEventSubscription {
    /// Removes the listener. Repeated calls are harmless.
    pub fn unsubscribe(&mut self) {
        if let Some(id) = self.id.take() {
            self.bus.remove(id);
        }
    }
}

impl Drop for HarnessEventSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Snapshot plus buffered future events with no observation gap.
pub struct HarnessWatch<T> {
    /// Snapshot captured after the watch became visible to emitters.
    pub snapshot: T,
    bus: HarnessEventBus,
    id: Option<u64>,
    state: Arc<Mutex<WatchState>>,
}

impl<T> HarnessWatch<T> {
    /// Flushes buffered events in order, then receives future events live.
    pub fn start(&self, listener: impl Fn(&HarnessEvent) + Send + Sync + 'static) {
        let listener: Listener = Arc::new(listener);
        loop {
            let pending = {
                let mut state = lock_unpoisoned(&self.state);
                if state.buffered.is_empty() {
                    state.listener = Some(listener.clone());
                    return;
                }
                std::mem::take(&mut state.buffered)
            };
            for event in pending {
                listener(&event);
            }
        }
    }

    /// Removes this watch. Repeated calls are harmless.
    pub fn unsubscribe(&mut self) {
        if let Some(id) = self.id.take() {
            self.bus.remove(id);
        }
        let mut state = lock_unpoisoned(&self.state);
        state.listener = None;
        state.buffered.clear();
    }
}

impl<T> Drop for HarnessWatch<T> {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

type LocalListener = Rc<dyn Fn(&HarnessEvent) + 'static>;

#[derive(Default)]
struct LocalEventBusState {
    next_listener_id: u64,
    listeners: BTreeMap<u64, (HarnessEventType, LocalListener)>,
    watchers: BTreeMap<u64, Rc<RefCell<LocalWatchState>>>,
}

#[derive(Default)]
struct LocalWatchState {
    listener: Option<LocalListener>,
    buffered: Vec<HarnessEvent>,
}

/// Cloneable single-threaded counterpart of [`HarnessEventBus`].
///
/// Listener closures may retain `Rc`-owned host state and are never required
/// to cross an executor thread.
#[derive(Clone, Default)]
pub struct LocalHarnessEventBus {
    inner: Rc<RefCell<LocalEventBusState>>,
}

impl LocalHarnessEventBus {
    /// Creates an empty local event registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one passive local listener.
    pub fn on(
        &self,
        event_type: HarnessEventType,
        listener: impl Fn(&HarnessEvent) + 'static,
    ) -> LocalHarnessEventSubscription {
        let mut state = self.inner.borrow_mut();
        let id = state.next_listener_id;
        state.next_listener_id = state.next_listener_id.saturating_add(1);
        state.listeners.insert(id, (event_type, Rc::new(listener)));
        LocalHarnessEventSubscription {
            bus: self.clone(),
            id: Some(id),
        }
    }

    /// Registers a local watcher before capturing its snapshot.
    pub fn watch<T>(&self, capture_snapshot: impl FnOnce() -> T) -> LocalHarnessWatch<T> {
        let watch_state = Rc::new(RefCell::new(LocalWatchState::default()));
        let id = {
            let mut state = self.inner.borrow_mut();
            let id = state.next_listener_id;
            state.next_listener_id = state.next_listener_id.saturating_add(1);
            state.watchers.insert(id, watch_state.clone());
            id
        };
        let snapshot = capture_snapshot();
        LocalHarnessWatch {
            snapshot,
            bus: self.clone(),
            id: Some(id),
            state: watch_state,
        }
    }

    /// Publishes one event synchronously to local listeners and watchers.
    pub fn emit(&self, event: HarnessEvent) {
        let (listeners, watchers) = {
            let state = self.inner.borrow();
            let listeners = state
                .listeners
                .values()
                .filter(|(event_type, _)| *event_type == event.event_type())
                .map(|(_, listener)| listener.clone())
                .collect::<Vec<_>>();
            let watchers = state.watchers.values().cloned().collect::<Vec<_>>();
            (listeners, watchers)
        };
        for listener in listeners {
            listener(&event);
        }
        for watcher in watchers {
            let listener = {
                let mut watcher = watcher.borrow_mut();
                match watcher.listener.clone() {
                    Some(listener) => Some(listener),
                    None => {
                        watcher.buffered.push(event.clone());
                        None
                    }
                }
            };
            if let Some(listener) = listener {
                listener(&event);
            }
        }
    }

    fn remove(&self, id: u64) {
        let mut state = self.inner.borrow_mut();
        state.listeners.remove(&id);
        state.watchers.remove(&id);
    }
}

impl fmt::Debug for LocalHarnessEventBus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.inner.borrow();
        formatter
            .debug_struct("LocalHarnessEventBus")
            .field("listener_count", &state.listeners.len())
            .field("watcher_count", &state.watchers.len())
            .finish()
    }
}

/// Owned local direct-listener registration.
pub struct LocalHarnessEventSubscription {
    bus: LocalHarnessEventBus,
    id: Option<u64>,
}

impl LocalHarnessEventSubscription {
    /// Removes the listener. Repeated calls are harmless.
    pub fn unsubscribe(&mut self) {
        if let Some(id) = self.id.take() {
            self.bus.remove(id);
        }
    }
}

impl Drop for LocalHarnessEventSubscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

/// Local snapshot plus buffered future events with no observation gap.
pub struct LocalHarnessWatch<T> {
    /// Snapshot captured after the watch became visible to emitters.
    pub snapshot: T,
    bus: LocalHarnessEventBus,
    id: Option<u64>,
    state: Rc<RefCell<LocalWatchState>>,
}

impl<T> LocalHarnessWatch<T> {
    /// Flushes buffered events in order, then receives future events live.
    pub fn start(&self, listener: impl Fn(&HarnessEvent) + 'static) {
        let listener: LocalListener = Rc::new(listener);
        loop {
            let pending = {
                let mut state = self.state.borrow_mut();
                if state.buffered.is_empty() {
                    state.listener = Some(listener.clone());
                    return;
                }
                std::mem::take(&mut state.buffered)
            };
            for event in pending {
                listener(&event);
            }
        }
    }

    /// Removes this watch. Repeated calls are harmless.
    pub fn unsubscribe(&mut self) {
        if let Some(id) = self.id.take() {
            self.bus.remove(id);
        }
        let mut state = self.state.borrow_mut();
        state.listener = None;
        state.buffered.clear();
    }
}

impl<T> Drop for LocalHarnessWatch<T> {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}
