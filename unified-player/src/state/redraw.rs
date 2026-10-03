//! Redraw scheduling for the UI render loop.
//!
//! The render loop sleeps until shared state changes instead of redrawing on a
//! fixed timer. State that the UI projects lives behind the tracked locks in
//! this module: releasing a guard that was mutably dereferenced wakes the
//! render loop, so writers do not have to remember to request redraws, while
//! pollers that only read do not cause redraws.

use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Instant,
};

use parking_lot::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Wakes the render loop when the projected state may have changed.
#[derive(Default)]
pub struct RedrawSignal {
    pending: Mutex<bool>,
    wake: Condvar,
}

impl RedrawSignal {
    pub fn request(&self) {
        *self.pending.lock() = true;
        self.wake.notify_one();
    }

    /// Block until a redraw is requested or `deadline` passes, then clear the
    /// pending request.
    pub fn wait_until(&self, deadline: Instant) {
        let mut pending = self.pending.lock();
        if !*pending {
            self.wake.wait_until(&mut pending, deadline);
        }
        *pending = false;
    }
}

/// A mutex whose guard requests a redraw when released after a mutable access.
pub struct TrackedMutex<T> {
    inner: Mutex<T>,
    signal: Arc<RedrawSignal>,
}

impl<T> TrackedMutex<T> {
    /// Create a mutex with its own signal, for state used outside `State`.
    pub fn new(value: T) -> Self {
        Self::with_signal(value, Arc::default())
    }

    pub fn with_signal(value: T, signal: Arc<RedrawSignal>) -> Self {
        Self {
            inner: Mutex::new(value),
            signal,
        }
    }

    pub fn lock(&self) -> TrackedMutexGuard<'_, T> {
        TrackedMutexGuard {
            guard: self.inner.lock(),
            signal: Some(&self.signal),
            dirty: false,
        }
    }

    /// Lock without requesting a redraw on release. For the render loop, whose
    /// own writes are already on screen, and for pollers that take mutable
    /// access on every tick but request a redraw only when they change state.
    pub(crate) fn lock_untracked(&self) -> TrackedMutexGuard<'_, T> {
        TrackedMutexGuard {
            guard: self.inner.lock(),
            signal: None,
            dirty: false,
        }
    }
}

pub struct TrackedMutexGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    signal: Option<&'a RedrawSignal>,
    dirty: bool,
}

impl<T> Deref for TrackedMutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for TrackedMutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        &mut self.guard
    }
}

impl<T> Drop for TrackedMutexGuard<'_, T> {
    fn drop(&mut self) {
        if let Some(signal) = self.signal.filter(|_| self.dirty) {
            signal.request();
        }
    }
}

/// A read-write lock whose write guard requests a redraw when released after a
/// mutable access.
pub struct TrackedRwLock<T> {
    inner: RwLock<T>,
    signal: Arc<RedrawSignal>,
}

impl<T> TrackedRwLock<T> {
    pub fn with_signal(value: T, signal: Arc<RedrawSignal>) -> Self {
        Self {
            inner: RwLock::new(value),
            signal,
        }
    }

    pub fn read(&self) -> RwLockReadGuard<'_, T> {
        self.inner.read()
    }

    pub fn write(&self) -> TrackedRwLockWriteGuard<'_, T> {
        TrackedRwLockWriteGuard {
            guard: self.inner.write(),
            signal: &self.signal,
            dirty: false,
        }
    }
}

pub struct TrackedRwLockWriteGuard<'a, T> {
    guard: RwLockWriteGuard<'a, T>,
    signal: &'a RedrawSignal,
    dirty: bool,
}

impl<T> Deref for TrackedRwLockWriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for TrackedRwLockWriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.dirty = true;
        &mut self.guard
    }
}

impl<T> Drop for TrackedRwLockWriteGuard<'_, T> {
    fn drop(&mut self) {
        if self.dirty {
            self.signal.request();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    use super::{RedrawSignal, TrackedMutex, TrackedRwLock};

    fn is_pending(signal: &RedrawSignal) -> bool {
        *signal.pending.lock()
    }

    #[test]
    fn releasing_tracked_guards_requests_a_redraw() {
        let signal = Arc::new(RedrawSignal::default());
        let mutex = TrackedMutex::with_signal(0, signal.clone());
        let lock = TrackedRwLock::with_signal(0, signal.clone());

        *mutex.lock() += 1;
        assert!(is_pending(&signal));
        signal.wait_until(Instant::now());

        *lock.write() += 1;
        assert!(is_pending(&signal));
    }

    #[test]
    fn render_locks_and_reads_do_not_request_a_redraw() {
        let signal = Arc::new(RedrawSignal::default());
        let mutex = TrackedMutex::with_signal(0, signal.clone());
        let lock = TrackedRwLock::with_signal(0, signal.clone());

        *mutex.lock_untracked() += 1;
        let _ = *mutex.lock();
        let _ = *lock.read();
        let _ = *lock.write();

        assert!(!is_pending(&signal));
    }

    #[test]
    fn a_pending_request_ends_the_wait_immediately() {
        let signal = RedrawSignal::default();
        signal.request();

        let started = Instant::now();
        signal.wait_until(started + Duration::from_secs(5));

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!is_pending(&signal));
    }
}
