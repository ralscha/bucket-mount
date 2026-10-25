//! Coordinates Windows rename/delete notifications with remote deletion.
//!
//! A directory scan is not an atomic filesystem snapshot. A rename may otherwise
//! move an unhydrated placeholder out of the scanner's path just before its S3
//! source is deleted. Native BEFORE callbacks hold an operation token through
//! completion. Deletions freeze the namespace only if the scan's generation is
//! still current and no native operation is underway.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};

#[derive(Debug, Default)]
pub struct Namespace {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    generation: u64,
    active_operations: usize,
    waiting_operations: usize,
    freezes: usize,
}

impl Namespace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Called before acknowledging a native rename/delete notification.
    /// Retain the returned token until its completion notification arrives.
    pub fn begin_operation(self: &Arc<Self>) -> OperationGuard {
        let mut state = self.lock();
        state.waiting_operations += 1;
        while state.freezes != 0 {
            state = self.changed.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        state.waiting_operations -= 1;
        state.active_operations += 1;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        OperationGuard {
            namespace: self.clone(),
        }
    }

    /// Capture before scanning; re-check with `try_freeze` before remote deletion.
    pub fn snapshot(&self) -> Option<u64> {
        let state = self.lock();
        (state.active_operations == 0 && state.waiting_operations == 0).then_some(state.generation)
    }

    /// Hold through the S3 DELETE request, never through a local rename/delete.
    /// Multiple remote deletions may share the same stable generation.
    pub fn try_freeze(&self, expected_generation: u64) -> Option<Freeze<'_>> {
        let mut state = self.lock();
        if state.generation != expected_generation
            || state.active_operations != 0
            || state.waiting_operations != 0
        {
            return None;
        }
        state.freezes += 1;
        Some(Freeze { namespace: self })
    }

    fn finish_operation(&self) {
        let mut state = self.lock();
        debug_assert!(state.active_operations > 0);
        state.active_operations -= 1;
        state.generation = state.generation.wrapping_add(1);
        drop(state);
        self.changed.notify_all();
    }

    fn unfreeze(&self) {
        let mut state = self.lock();
        debug_assert!(state.freezes > 0);
        state.freezes -= 1;
        drop(state);
        self.changed.notify_all();
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// Owned by a native notification entry; Drop accounts for completion or failure.
#[derive(Debug)]
#[must_use = "retain the operation token until the native operation completes"]
pub struct OperationGuard {
    namespace: Arc<Namespace>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.namespace.finish_operation();
    }
}

/// No mutex is held, so this guard may safely span an asynchronous S3 request.
#[derive(Debug)]
#[must_use = "hold this guard through the remote deletion request"]
pub struct Freeze<'a> {
    namespace: &'a Namespace,
}

impl Drop for Freeze<'_> {
    fn drop(&mut self) {
        self.namespace.unfreeze();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn any_native_operation_invalidates_an_earlier_scan() {
        let namespace = Arc::new(Namespace::new());
        let scanned = namespace.snapshot().unwrap();
        let operation = namespace.begin_operation();
        assert!(namespace.snapshot().is_none());
        assert!(namespace.try_freeze(scanned).is_none());
        drop(operation);
        assert!(namespace.try_freeze(scanned).is_none());
        let fresh = namespace.snapshot().unwrap();
        assert_ne!(fresh, scanned);
        assert!(namespace.try_freeze(fresh).is_some());
    }

    #[test]
    fn all_inflight_operations_must_complete_before_deleting() {
        let namespace = Arc::new(Namespace::new());
        let first = namespace.begin_operation();
        let second = namespace.begin_operation();
        drop(first);
        assert!(namespace.snapshot().is_none());
        drop(second);
        let generation = namespace.snapshot().unwrap();
        assert!(namespace.try_freeze(generation).is_some());
    }

    #[test]
    fn native_operation_waits_until_every_remote_delete_finishes() {
        let namespace = Arc::new(Namespace::new());
        let generation = namespace.snapshot().unwrap();
        let first = namespace.try_freeze(generation).unwrap();
        let second = namespace.try_freeze(generation).unwrap();
        let worker_namespace = namespace.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let _operation = worker_namespace.begin_operation();
            entered_tx.send(()).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(entered_rx.recv_timeout(Duration::from_millis(40)).is_err());
        drop(first);
        assert!(entered_rx.recv_timeout(Duration::from_millis(40)).is_err());
        drop(second);
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert!(namespace.try_freeze(generation).is_none());
    }

    #[test]
    fn freeze_guard_can_cross_async_worker_threads() {
        fn is_send<T: Send>() {}
        fn is_sync<T: Sync>() {}
        is_send::<Freeze<'_>>();
        is_send::<OperationGuard>();
        is_sync::<Namespace>();
    }
}
