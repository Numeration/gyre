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
        while self
            .flag
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // The lock is held by another task; yield to the scheduler and try again later.
            tokio::task::yield_now().await;
        }

        Guard(self)
    }

    /// A variant of `acquire` for use with `Fence`s wrapped in an `Arc`.
    ///
    /// Returns an `OwnedGuard`.
    pub async fn acquire_owned(self: &Arc<Self>) -> OwnedGuard {
        while self
            .flag
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tokio::task::yield_now().await;
        }

        OwnedGuard(Arc::clone(&self))
    }

    /// Asynchronously waits until the lock is released.
    ///
    /// Unlike `acquire`, this method does **not** acquire the lock. It simply waits
    /// until the `flag` becomes `false`.
    ///
    /// This can be useful in scenarios where a task needs to wait for another task
    /// holding the lock to complete an operation, but does not need to hold the
    /// lock itself afterward.
    pub async fn until_released(&self) {
        while self.flag.load(Ordering::Acquire) {
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

    #[tokio::test]
    async fn test_until_released_blocking() {
        let fence = Arc::new(Fence::default());

        // 1. Task 1 acquires and holds the lock
        let fence_clone = fence.clone();
        let task1 = tokio::spawn(async move {
            let _guard = fence_clone.acquire().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Implicitly dropped
        });

        // Wait for Task 1 to acquire the lock
        tokio::task::yield_now().await;

        // 2. Task 2 waits for release
        let fence_clone = fence.clone();
        let mut task2 = tokio::spawn(async move {
            fence_clone.until_released().await;
        });

        // Ensure Task 2 does not complete within 50ms (it's blocked)
        assert!(
            timeout(Duration::from_millis(50), &mut task2)
                .await
                .is_err(),
            "Task 2 should be blocked waiting for release"
        );

        // Wait for Task 1 to complete and release the lock
        task1.await.unwrap();

        // Task 2 should complete quickly
        timeout(Duration::from_millis(50), task2)
            .await
            .expect("Task 2 should be unblocked")
            .unwrap();
    }
}
