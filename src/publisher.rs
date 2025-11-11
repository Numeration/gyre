//! Defines the `Publisher` end of a `gyre` channel, responsible for writing events
//! into the shared `Bus`.
//!
//! `Publisher` implements multi-producer support (via `Clone`) and has a built-in
//! asynchronous backpressure mechanism. When the ring buffer is full, `publish` calls
//! will pause asynchronously until consumers have processed older events and freed up space.

use crate::cursor::Cursor;
use crate::fence::Fence;
use crate::sequence_barrier::{SequenceNotifier, SequenceWaiter};
use crate::{Bus, Consumer, fence};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::futures::Notified;

/// An RAII guard used to lock the reading of the sequence number during a `subscribe` operation.
///
/// When `subscribe` is called, we need to get the latest published sequence number
/// to ensure the new consumer starts at the correct position. `SequenceGuard` ensures
/// that no other `publish` operations are modifying this sequence number while it is being read,
/// thus avoiding race conditions.
#[allow(dead_code)]
struct SequenceGuard<'a>(i64, fence::Guard<'a>);

impl Deref for SequenceGuard<'_> {
    type Target = i64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Manages the publisher-side sequence number and notification mechanism.
///
/// This is the core controller within the `Publisher`, responsible for:
/// 1.  Atomically incrementing the global publish sequence number (`claimed`).
/// 2.  Using `claim_fence` to ensure that sequence claiming and subscription operations are mutually exclusive.
/// 3.  Communicating with the `Consumer` side's `SequenceWaiter` via `notifier`
///     to awaken consumers waiting for new events.
#[derive(Debug)]
struct SequenceController {
    claim_fence: Fence,
    claimed: Cursor,
    notifier: SequenceNotifier,
}

impl SequenceController {
    fn new(notifier: SequenceNotifier) -> Self {
        Self {
            claim_fence: Default::default(),
            claimed: Default::default(),
            notifier,
        }
    }

    /// Gets the current highest claimed sequence number while holding a lock to prevent concurrent modification.
    /// Primarily used by the `subscribe` method.
    async fn acquire_sequence(&self) -> SequenceGuard<'_> {
        let guard = self.claim_fence.acquire().await;
        SequenceGuard(self.claimed.relaxed(), guard)
    }

    /// Atomically claims and increments the next available publish sequence number. This method first waits for
    /// `claim_fence` to be released to ensure mutual exclusion with `subscribe` operations, preventing race conditions.
    async fn next_sequence(&self) -> i64 {
        self.claim_fence.until_released().await;
        self.claimed.fetch_add(1)
    }

    /// Notifies a potentially waiting consumer.
    fn notify_one(&self) {
        self.notifier.notify();
    }

    /// Broadcasts the published status of an event (via its `sequence`) to all consumers.
    /// This updates the globally visible publish cursor and wakes up all waiting `Consumer`s.
    async fn publish(&self, sequence: i64) {
        self.notifier.publish(sequence).await;
    }

    /// Gets a future that completes when a consumer makes progress.
    /// Used in backpressure scenarios to wait for consumers to free up space.
    fn consumer_progress_waiter(&self) -> Notified<'_> {
        self.notifier.waiter()
    }

    /// Creates a sequence waiter for a new `Consumer`.
    fn subscriber_waiter(&self) -> SequenceWaiter {
        self.notifier.subscribe()
    }
}

/// The sending end of a `gyre` channel.
///
/// `Publisher` is responsible for sending events to the shared ring buffer.
/// It is `Send` and `Sync`, and can be cloned to support multiple concurrent producers.
/// The channel is closed when the last `Publisher` (including all its clones) is dropped.
#[derive(Debug)]
pub struct Publisher<T> {
    bus: Arc<Bus<T>>,
    controller: Arc<SequenceController>,
}

impl<T> Clone for Publisher<T> {
    /// Clones a `Publisher`.
    ///
    /// All cloned `Publisher` instances share the same publish sequence number, allowing
    /// multiple producers to publish to the same channel in a thread-safe manner.
    fn clone(&self) -> Self {
        Self {
            bus: Arc::clone(&self.bus),
            controller: Arc::clone(&self.controller),
        }
    }
}

impl<T> Publisher<T> {
    /// Creates a new `Publisher` instance. For internal `gyre` use only.
    pub(crate) fn new(bus: Arc<Bus<T>>, notifier: SequenceNotifier) -> Self {
        Self {
            bus,
            controller: Arc::new(SequenceController::new(notifier)),
        }
    }

    /// Asynchronously publishes an event to the channel.
    ///
    /// This method first claims the next sequence number and then checks for sufficient
    /// space in the ring buffer. If the slowest consumer has not yet processed older
    /// events, causing the buffer to be full, this method will wait asynchronously
    /// (**backpressure**) until space is freed.
    ///
    /// # Arguments
    ///
    /// * `event`: The event to be published.
    ///
    /// # Returns
    ///
    /// * `Ok(())`: If the event was successfully published.
    /// * `Err(T)`: If there are currently no active `Consumer`s, the event will not be
    ///   published and is returned as-is. This prevents losing events when there are
    ///   no subscribers.
    ///
    /// # Cancellation Safety
    ///
    /// This method is **not** cancellation safe. It first atomically claims a sequence
    /// number for the new event and only then waits for space to become available in
    /// the buffer. If the `publish` future is cancelled (e.g., via `tokio::select!` or
    /// a timeout) while waiting on backpressure, the claimed sequence number will be
    /// permanently lost, creating a "gap" in the event stream.
    ///
    /// This will cause all consumers to eventually stall indefinitely while waiting for the
    /// missing event, leading to a **deadlock**.
    ///
    /// Therefore, you **must ensure** that the future returned by `publish` is driven
    /// to completion and not dropped prematurely after it has started.
    pub async fn publish(&self, event: T) -> Result<(), T> {
        let bus = &self.bus;
        let controller = &self.controller;
        let next_seq = controller.next_sequence().await;
        let buffer = &self.bus.buffer;

        // Wait for a free slot in the ring buffer
        loop {
            let waiter = controller.consumer_progress_waiter();

            let consumers = bus.consumers.pin_owned();
            let Some(min_seq) = consumers
                .values()
                .map(Deref::deref)
                .map(Cursor::relaxed)
                .min()
            else {
                // No consumers, no need to wait. Return the event directly.
                controller.publish(next_seq).await;
                return Err(event);
            };

            if min_seq < next_seq {
                let Ok(offset) = usize::try_from(next_seq - min_seq) else {
                    // This is an unlikely edge case, e.g., i64 overflow.
                    drop(consumers);
                    controller.notify_one();
                    waiter.await;
                    continue;
                };

                if offset >= buffer.capacity() {
                    // Buffer is full, wait for consumers to free up space.
                    drop(consumers);
                    controller.notify_one();
                    waiter.await;
                    continue;
                }
            }

            break;
        }

        // Write the data
        // SAFETY: We have already ensured this slot is safe via the backpressure check,
        // and `next_sequence` guarantees we have exclusive write access for this sequence number.
        unsafe {
            *buffer.get(next_seq) = Some(event);
        }

        // Publish is complete, update the global cursor and notify all consumers.
        controller.publish(next_seq).await;
        Ok(())
    }

    /// Dynamically subscribes a new `Consumer` to the event bus.
    ///
    /// The newly created `Consumer` will receive all events that are **newly published**
    /// after this method call completes. It will not receive any historical events that
    /// were already in the ring buffer before the subscription.
    ///
    /// This is different from `consumer.clone()`, which creates a copy that starts
    /// consuming from the same position.
    ///
    /// # Returns
    ///
    /// A new `Consumer` instance.
    ///
    /// # Cancellation Safety
    ///
    /// This method **is** cancellation safe. If the future is cancelled, it will be
    /// before any changes to the shared state are made. The new consumer is only
    /// registered with the bus *after* all `await` points have completed. It is safe
    /// to use this method in contexts like `tokio::select!`.
    pub async fn subscribe(&self) -> Consumer<T> {
        let bus = self.bus.clone();
        let controller = &self.controller;

        // Get the current highest claimed sequence number as the starting point for the new consumer.
        let next_seq = controller.acquire_sequence().await;
        let subscriber = controller.subscriber_waiter();

        Consumer::new(bus, subscriber, *next_seq - 1)
    }
}

#[cfg(test)]
mod tests {
    use crate::channel;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn test_basic_publish_and_consume() {
        let (tx, mut rx) = channel::<i32>(4);

        // 1. Publish events
        tx.publish(100).await.unwrap();
        tx.publish(200).await.unwrap();

        // 2. Consume event 1
        let guard1 = rx.next().await.unwrap();
        assert_eq!(*guard1, 100);
        // cursor advances when guard1 is dropped
        drop(guard1);

        // 3. Consume event 2
        let guard2 = rx.next().await.unwrap();
        assert_eq!(*guard2, 200);
    }

    #[tokio::test]
    async fn test_publish_no_consumer() {
        let (tx, rx) = channel::<i32>(4);

        // Drop the initial consumer
        drop(rx);

        // Publisher should return Err(T), indicating the event could not be published
        let result = tx.publish(999).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 999);
    }

    #[tokio::test]
    async fn test_backpressure_blocking() {
        // RingBuffer with capacity 2
        let (tx, mut rx) = channel::<u32>(2);

        // 1. Publish 1 (fills the buffer, as effective capacity is buffer_size - 1 for publishers)
        tx.publish(1).await.unwrap();

        // 2. Try to publish 2 (buffer is full, publisher should block)
        let mut publish_task = tokio::spawn(async move {
            tx.publish(2).await.unwrap();
        });

        // Ensure the publish task does not complete within 50ms (i.e., it's blocked)
        assert!(
            timeout(Duration::from_millis(50), &mut publish_task)
                .await
                .is_err(),
            "Publisher should be blocked due to backpressure"
        );

        // 3. Consumer consumes one event (freeing up a slot)
        let g1 = rx.next().await.unwrap();
        assert_eq!(*g1, 1);
        drop(g1); // cursor advances

        // 4. Now, the publish task should be unblocked and complete quickly
        timeout(Duration::from_millis(5000), publish_task)
            .await
            .expect("Publisher should be unblocked")
            .unwrap();

        // 5. Verify that event 2 has been published
        let g2 = rx.next().await.unwrap();
        assert_eq!(*g2, 2);
    }

    #[tokio::test]
    async fn test_dynamic_subscribe() {
        let (tx, mut rx1) = channel::<String>(4);

        // 1. rx1 consumes the initial event
        tx.publish("A".to_string()).await.unwrap();
        let g_a = rx1.next().await.unwrap();
        assert_eq!(*g_a, "A");
        // At this point, the global cursor is 0, rx1's cursor is -1

        // 2. Dynamically create subscriber rx2
        let mut rx2 = tx.subscribe().await;

        // 3. Publish events B and C
        tx.publish("B".to_string()).await.unwrap();
        tx.publish("C".to_string()).await.unwrap();

        // 4. Verify rx1 receives all events
        // rx1 should first process A (after the drop), then B and C
        drop(g_a); // cursor advances

        let g_b1 = rx1.next().await.unwrap();
        assert_eq!(*g_b1, "B");
        drop(g_b1);

        let g_c1 = rx1.next().await.unwrap();
        assert_eq!(*g_c1, "C");
        drop(g_c1);

        // 5. Verify rx2 only receives events published after subscription (B and C)
        // rx2's cursor should have been initialized just before B.
        let g_b2 = rx2.next().await.unwrap();
        assert_eq!(*g_b2, "B");
        drop(g_b2);

        let g_c2 = rx2.next().await.unwrap();
        assert_eq!(*g_c2, "C");
        drop(g_c2);

        // 6. Verify rx2 cannot get more events
        let result = timeout(Duration::from_millis(10), rx2.next()).await;
        assert!(result.is_err());
    }
}
