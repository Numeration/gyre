//! Provides the core synchronization primitive used within `gyre`: the sequence barrier.
//!
//! This module is the cornerstone of communication and synchronization between `Publisher`s
//! and `Consumer`s. It revolves around a shared, atomically-updated cursor (`Cursor`)
//! that represents the highest event sequence number that has been successfully
//! published and is visible to all consumers.
//!
//! The core concept is:
//! - **`SequenceNotifier`**: Held by the `Publisher`, it is used to **advance** the shared
//!   cursor and **notify** all waiting `Consumer`s that new events are available.
//! - **`SequenceWaiter`**: Held by each `Consumer`, it is used to **wait** for the shared
//!   cursor to advance beyond its own current consumption position.
//!
//! This mechanism is implemented using `tokio::sync::Notify`, avoiding locks and
//! busy-waiting to achieve efficient asynchronous coordination.

use crate::cursor::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio::sync::futures::Notified;

/// The internal state shared between a `SequenceNotifier` and its `SequenceWaiter`s.
///
/// This struct is wrapped in an `Arc` and is the core of the entire sequence barrier mechanism.
#[derive(Debug)]
struct SharedState {
    /// Marks whether the channel is closed. This becomes `true` when all `Publisher`s have been dropped.
    closed: AtomicBool,

    /// The globally visible, highest published sequence number.
    /// The `Publisher` advances this cursor, and `Consumer`s wait for it to advance.
    cursor: Cursor,

    /// Used to wake up `Consumer`s that are waiting for new events.
    /// `notify_waiters()` is called when a `Publisher` successfully publishes an event.
    consumer_notify: Notify,

    /// Used to wake up a `Publisher` that is blocked due to backpressure.
    /// When a `Consumer` finishes processing an event (i.e., its `EventGuard` is dropped),
    /// it calls `notify_waiters()` to inform the `Publisher` that space may now be available.
    publisher_notify: Notify,
}

impl SharedState {
    fn new(cursor: Cursor) -> Self {
        Self {
            closed: AtomicBool::new(false),
            cursor,
            consumer_notify: Notify::new(),
            publisher_notify: Notify::new(),
        }
    }

    /// Closes the barrier and wakes up all waiting `Consumer`s, allowing them
    /// to observe the closed state and exit.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.consumer_notify.notify_waiters();
    }

    /// Checks if the barrier is closed.
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// The notifying side of the sequence barrier, held by the `Publisher`.
///
/// Its primary responsibility is to update the shared publish cursor and wake up
/// waiting consumers.
#[derive(Debug)]
pub(crate) struct SequenceNotifier(Arc<SharedState>);

impl Drop for SequenceNotifier {
    fn drop(&mut self) {
        // When the last Publisher is dropped, close the channel.
        self.0.close();
    }
}

impl SequenceNotifier {
    /// Notifies all potentially waiting `Consumers`. This is used by the publisher
    /// during backpressure to nudge a consumer forward.
    pub(crate) fn notify(&self) {
        self.0.consumer_notify.notify_waiters();
    }

    /// Returns a `Notified` future that completes when a consumer makes progress.
    /// A `Publisher` will `await` this future during backpressure.
    pub(crate) fn waiter(&self) -> Notified<'_> {
        self.0.publisher_notify.notified()
    }

    pub(crate) fn cursor(&self) -> i64 {
        self.0.cursor.relaxed()
    }

    /// Publishes a sequence number, making it visible to all consumers.
    ///
    /// This method directly stores the `sequence` number, overwriting the previous one.
    /// It assumes that the caller has already ensured exclusive access and that
    /// `sequence` is the correct next value in the series. In `gyre`, this
    /// exclusivity is guaranteed by the `claim_fence` in the `SequenceController`.
    ///
    /// After updating the cursor, it notifies all waiting consumers.
    pub(crate) fn publish(&self, sequence: i64) {
        self.0.cursor.store(sequence);

        // Notify all waiting subscribers
        self.0.consumer_notify.notify_waiters();
    }

    pub(crate) fn subscribe(&self) -> SequenceWaiter {
        SequenceWaiter(self.0.clone())
    }
}

/// The waiting side of the sequence barrier, held by the `Consumer`.
///
/// Its primary responsibility is to asynchronously wait for the global publish cursor
/// to advance to a certain target value.
#[derive(Debug, Clone)]
pub(crate) struct SequenceWaiter(Arc<SharedState>);

impl SequenceWaiter {
    /// Wakes up a `Publisher` that may be waiting due to backpressure.
    /// Called by a `Consumer` after its cursor has advanced.
    pub(crate) fn notify(&self) {
        self.0.publisher_notify.notify_one();
    }

    /// Asynchronously waits until the shared cursor's value is greater than `target`.
    ///
    /// This is the core logic for a `Consumer` to wait for new events.
    ///
    /// # Returns
    ///
    /// * `Ok(())`: Returned when `cursor` > `target`.
    /// * `Err(())`: Returned if the channel is closed during the wait.
    pub(crate) async fn wait_until(&self, target: i64) -> Result<(), ()> {
        loop {
            // First check: optimistically check if the condition is already met
            // to avoid unnecessary waiting.
            if self.0.cursor.relaxed() > target {
                return Ok(());
            }

            if self.0.is_closed() {
                // If the channel is already closed when checked and the condition is
                // still not met, exit.
                return Err(());
            }

            let notified = self.0.consumer_notify.notified();

            // Second check: check again right before preparing to `await`.
            // This handles the race condition that can occur between the first check
            // and the call to `notified()`.
            if self.0.cursor.relaxed() > target {
                return Ok(());
            }

            if self.0.is_closed() {
                return Err(());
            }

            // Wait for a notification from the Publisher.
            notified.await;
        }
    }
}

/// Creates a pair of interconnected `SequenceNotifier` and `SequenceWaiter`.
///
/// The `cursor` parameter is used to initialize the shared publish cursor.
pub(crate) fn sequence_barrier_pair(cursor: Cursor) -> (SequenceNotifier, SequenceWaiter) {
    let shared = Arc::new(SharedState::new(cursor));
    (SequenceNotifier(shared.clone()), SequenceWaiter(shared))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn test_sequence_pair_initialization() {
        let initial_cursor = Cursor::new(99);
        let (notifier, waiter) = sequence_barrier_pair(initial_cursor);

        // Verify initial state
        assert_eq!(notifier.0.cursor.relaxed(), 99);
        assert_eq!(waiter.0.cursor.relaxed(), 99);
        assert!(!waiter.0.is_closed());
    }

    #[tokio::test]
    async fn test_notifier_publish_advances_cursor() {
        let (notifier, waiter) = sequence_barrier_pair(Cursor::new(-1));

        // First publish, sequence 0
        notifier.publish(0);
        assert_eq!(waiter.0.cursor.relaxed(), 0);

        // Second publish, sequence 1
        notifier.publish(1);
        assert_eq!(waiter.0.cursor.relaxed(), 1);
    }

    #[tokio::test]
    async fn test_waiter_waits_until_target() {
        let (notifier, waiter) = sequence_barrier_pair(Cursor::new(-1));

        // Start a waiting task, waiting for a sequence > 4
        let waiter_clone = waiter.clone();
        let mut wait_task = tokio::spawn(async move { waiter_clone.wait_until(4).await });

        // Ensure the task does not complete in a short time (it's blocked)
        assert!(
            timeout(Duration::from_millis(10), &mut wait_task)
                .await
                .is_err(),
            "Waiter should be blocked"
        );

        // Publish sequences 0 through 4
        for i in 0..=4 {
            notifier.publish(i);
        }

        // The cursor is now at 4. The waiting task should still be blocked (waiting for > 4).
        assert_eq!(waiter.0.cursor.relaxed(), 4);
        assert!(
            timeout(Duration::from_millis(50), &mut wait_task)
                .await
                .is_err(),
            "Waiter should still be blocked at target 4"
        );

        // Publish sequence 5
        notifier.publish(5);

        // The task should complete quickly
        let result = timeout(Duration::from_millis(500), wait_task).await;
        assert!(
            result.is_ok(),
            "Waiter waiting for cursor > 4 should be unblocked when cursor becomes 5"
        );
        assert!(result.unwrap().unwrap().is_ok());
    }

    #[tokio::test]
    async fn test_wait_until_already_met() {
        let (notifier, waiter) = sequence_barrier_pair(Cursor::new(-1));

        // Publish up to sequence 10
        for i in 0..=10 {
            notifier.publish(i);
        }

        // Immediately wait for a sequence > 5 (target is already met)
        let result = waiter.wait_until(5).await;
        assert!(
            result.is_ok(),
            "Wait should return immediately if target is met"
        );
    }

    #[tokio::test]
    async fn test_waiter_unblocked_by_notifier_drop() {
        let (notifier, waiter) = sequence_barrier_pair(Cursor::new(-1));

        // Start a task that will wait indefinitely
        let waiter_clone = waiter.clone();
        let wait_task = tokio::spawn(async move { waiter_clone.wait_until(100).await });

        // Drop the Notifier (simulating Publisher exit)
        drop(notifier);

        // The task should be woken up and return Err(())
        let result = timeout(Duration::from_millis(50), wait_task).await;
        assert!(result.is_ok(), "Task should be unblocked by drop");
        assert!(
            result.unwrap().unwrap().is_err(),
            "Wait should return error on close"
        );

        // Verify closed state
        assert!(waiter.0.is_closed());
    }

    #[tokio::test]
    async fn test_waiter_double_check() {
        let cursor = Cursor::new(-1);
        let shared = Arc::new(SharedState::new(cursor));
        let waiter = SequenceWaiter(shared.clone());
        let _notifier = SequenceNotifier(shared.clone());

        // Use the double-check to ensure correctness under race conditions

        // Start a task to wait for > -1 (i.e., sequence 0)
        let waiter_clone = waiter.clone();
        let wait_task = tokio::spawn(async move { waiter_clone.wait_until(-1).await });

        // To simulate a race, advance the cursor *before* notifying,
        // which the double-check logic should handle.

        // 1. Advance the cursor to 0 (without calling notify_waiters)
        shared.cursor.fetch_add(1);

        // 2. Now, notify the task
        shared.consumer_notify.notify_one();

        // The task should complete successfully because the second check inside `wait_until`
        // will see the updated cursor value.
        let result = timeout(Duration::from_millis(500), wait_task).await;
        assert!(result.is_ok(), "Wait should succeed due to double check");
        assert!(result.unwrap().unwrap().is_ok());
    }
}
