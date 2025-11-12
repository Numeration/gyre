//! Provides a simple, async-spin-based lock `Fence`.
//!
//! This module implements a basic asynchronous mutex used to protect short
//! critical sections within `gyre`. It works by using an `AtomicBool` flag and an
//! async loop that yields the CPU via `tokio::task::yield_now()` while waiting
//! for the lock to be released, instead of performing a blocking busy-wait.
//!
//! This implementation avoids introducing a heavier `tokio::sync::Mutex` and is
//! suitable for scenarios where lock contention is low and hold times are extremely short.

use crossbeam_utils::CachePadded;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// An RAII guard that signifies a `Fence` has been locked.
///
/// When this guard is `drop`ped, it automatically releases the `Fence` lock it holds.
/// This design ensures that the lock is always released correctly, even in the event of a panic.
pub struct Guard<'a>(&'a Fence);

impl<'a> Drop for Guard<'a> {
    fn drop(&mut self) {
        // Release the lock. Use Release ordering to ensure that memory writes
        // preceding this point are visible to other threads trying to acquire the lock.
        self.0.flag.store(false, Ordering::Release);
    }
}

/// An owned version of `Guard` for use with `Fence`s wrapped in an `Arc`.
///
/// This guard is used when the `Fence` itself is shared via an `Arc`.
/// Its behavior is identical to `Guard`.
pub struct OwnedGuard(Arc<Fence>);

impl Drop for OwnedGuard {
    fn drop(&mut self) {
        // Release the lock.
        self.0.flag.store(false, Ordering::Release);
    }
}

/// A lightweight, asynchronous spin lock.
///
/// `Fence` uses an atomic boolean to represent its locked state.
/// A task attempting to acquire the lock enters a loop, using `compare_exchange_weak`
/// to try to set the flag. If the attempt fails, the task calls `tokio::task::yield_now()`
/// to yield execution to the Tokio scheduler, thus avoiding CPU-intensive busy-waiting.
///
/// The internal `AtomicBool` is wrapped in `CachePadded` to reduce performance
/// degradation on multi-core systems due to false sharing.
#[derive(Debug, Default)]
pub struct Fence {
    flag: CachePadded<AtomicBool>,
}

impl Fence {
    /// Attempts to acquire the lock immediately.
    ///
    /// Returns `Some(Guard)` if the lock was successfully acquired,
    /// or `None` if the lock is currently held by another task.
    #[inline]
    pub fn try_acquire(&self) -> Option<Guard<'_>> {
        if self
            .flag
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(Guard(self))
        } else {
            None
        }
    }

    /// Acquires the lock asynchronously.
    ///
    /// This method will spin-wait until the lock is successfully acquired and then
    /// returns a `Guard`. The lock is automatically released when the returned
    /// `Guard` is `drop`ped.
    pub async fn acquire(&self) -> Guard<'_> {
        // Use `compare_exchange_weak` in a loop to attempt to acquire the lock.
        // `Acquire` ordering ensures that after successfully acquiring the lock, all
        // memory writes from other threads that occurred before they released the lock
        // are visible to the current thread.
        loop {
            if let Some(guard) = self.try_acquire() {
                return guard;
            }
            // The lock is held by another task; yield to the scheduler and try again later.
            tokio::task::yield_now().await;
        }
    }

    /// Attempts to acquire the lock immediately for `Arc<Fence>`.
    ///
    /// Returns `Some(OwnedGuard)` if successful, or `None` otherwise.
    pub fn try_acquire_owned(self: &Arc<Self>) -> Option<OwnedGuard> {
        if self
            .flag
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(OwnedGuard(Arc::clone(self)))
        } else {
            None
        }
    }

    /// A variant of `acquire` for use with `Fence`s wrapped in an `Arc`.
    ///
    /// Returns an `OwnedGuard`.
    pub async fn acquire_owned(self: &Arc<Self>) -> OwnedGuard {
        loop {
            if let Some(guard) = self.try_acquire_owned() {
                return guard;
            }
            tokio::task::yield_now().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn test_fence_acquire_and_release() {
        let fence = Fence::default();

        // 1. First acquire
        let guard1 = fence.acquire().await;
        assert!(fence.flag.load(Ordering::Relaxed));

        // 2. Release
        drop(guard1);
        assert!(!fence.flag.load(Ordering::Relaxed));

        // 3. Second acquire
        let guard2 = fence.acquire().await;
        assert!(fence.flag.load(Ordering::Relaxed));
        drop(guard2);
    }

    #[tokio::test]
    async fn test_fence_exclusive_blocking() {
        let fence = Arc::new(Fence::default());

        // Task 1: Acquire the lock and hold it for 100ms
        let fence_clone = fence.clone();
        let task1 = tokio::spawn(async move {
            let _guard = fence_clone.acquire().await;
            // Hold it long enough for Task 2 to try to acquire it
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Implicitly dropped
        });

        // Wait for Task 1 to acquire the lock
        tokio::task::yield_now().await;
        assert!(fence.flag.load(Ordering::Relaxed));

        // Task 2: Attempt to acquire the lock
        let fence_clone = fence.clone();
        let mut task2 = tokio::spawn(async move {
            let _guard = fence_clone.acquire().await; // Should be blocked
            fence_clone.flag.load(Ordering::Relaxed) // Return the state after successful acquisition
        });

        // Ensure Task 2 does not complete within 50ms (it's blocked)
        assert!(
            timeout(Duration::from_millis(50), &mut task2)
                .await
                .is_err(),
            "Task 2 should be blocked"
        );

        // Wait for Task 1 to complete and release the lock
        task1.await.unwrap();
        assert!(!fence.flag.load(Ordering::Relaxed));

        // Task 2 should now be woken up and complete quickly
        let result = timeout(Duration::from_millis(500), task2)
            .await
            .expect("Task 2 should be unblocked")
            .unwrap();

        // Task 2 successfully acquired the lock; verify its state
        assert!(result, "Task 2 should hold the flag now");
    }

    #[tokio::test]
    async fn test_owned_fence_acquire_and_drop() {
        let fence_arc = Arc::new(Fence::default());

        // 1. Acquire OwnedGuard
        let guard = fence_arc.acquire_owned().await;
        assert!(fence_arc.flag.load(Ordering::Relaxed));

        // 2. Release
        drop(guard);
        assert!(!fence_arc.flag.load(Ordering::Relaxed));

        // Ensure Arc count is still correct
        assert_eq!(Arc::strong_count(&fence_arc), 1);
    }
}
