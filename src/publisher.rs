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
use tokio::sync::Notify;
use tokio::sync::futures::Notified;

/// An RAII guard that represents an exclusive claim on the next available publish slot.
///
/// This guard is the cornerstone of the publisher's concurrency and cancellation safety.
/// It is acquired by calling `SequenceController::claim_slot()`. While the guard is held,
/// it guarantees that no other publisher can attempt to publish an event, ensuring strict
/// serialization of publications.
///
/// The guard holds the sequence number for the slot it has claimed.
/// When it is dropped, it automatically releases the underlying lock and notifies the next
/// waiting publisher task that it can now attempt to claim a slot.
#[allow(dead_code)]
struct ClaimGuard<'a>(i64, fence::Guard<'a>, &'a SequenceController);

impl Deref for ClaimGuard<'_> {
    type Target = i64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        self.2.publisher_notify.notify_one();
    }
}

/// Manages the publisher-side sequencing and synchronization.
///
/// This controller is the core of the multi-producer and cancellation safety logic.
/// It ensures that multiple publishers coordinate to publish events in a strict,
/// sequential order, even under contention.
///
/// The main mechanism is a "turnstile" or "baton pass" system implemented with an
/// asynchronous lock (`claim_fence`) and an async condition variable (`publisher_notify`).
///
/// 1. A publisher task calls `claim_slot()` to request permission to publish.
/// 2. It waits asynchronously until it can acquire the `claim_fence` lock. This
///    serializes all publishers, allowing only one to proceed at a time.
/// 3. Upon acquiring the lock, it receives a `ClaimGuard`. This guard represents
///    the exclusive right to perform the next publish operation.
/// 4. When the `ClaimGuard` is dropped (either on successful publish or cancellation),
///    it releases the `claim_fence` and notifies `publisher_notify`.
/// 5. This notification wakes up the next publisher task waiting in `claim_slot()`,
///    allowing it to take its turn.
#[derive(Debug)]
struct SequenceController {
    /// An asynchronous mutex that ensures only one publisher can hold a `ClaimGuard`
    /// at any given time. This is the primary mechanism for serializing publish operations.
    claim_fence: Fence,
    /// An async condition variable that creates a fair wait queue for publishers.
    /// When a `ClaimGuard` is dropped, this is notified to wake up the next task
    /// waiting to acquire the `claim_fence`.
    publisher_notify: Notify,
    /// The link to the `SequenceBarrier`, used to communicate with consumers. It provides
    /// the globally visible cursor and the means to notify consumers of new events.
    notifier: SequenceNotifier,
}

impl SequenceController {
    fn new(notifier: SequenceNotifier) -> Self {
        Self {
            claim_fence: Default::default(),
            publisher_notify: Notify::new(),
            notifier,
        }
    }

    /// Asynchronously acquires an exclusive lock for publishing the next event.
    ///
    /// This method ensures that only one publisher can be in the process of
    /// preparing and committing a publish operation at any given time. It waits
    /// until the `claim_fence` lock is available.
    ///
    /// Upon acquiring the lock, it calculates the next sequence number by reading
    /// the current globally visible cursor and adding 1. It then returns a `ClaimGuard`
    /// which holds the lock and the calculated sequence number.
    ///
    /// The lock is released automatically when the returned `ClaimGuard` is dropped.
    /// This mechanism is critical for ensuring that `Publisher::publish` is
    /// cancellation-safe.
    async fn claim_slot(&self) -> ClaimGuard<'_> {
        loop {
            let notified = self.publisher_notify.notified();

            if let Some(guard) = self.claim_fence.try_acquire() {
                return ClaimGuard(self.notifier.cursor() + 1, guard, self);
            }

            notified.await;
        }
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
    /// This method first claims an exclusive slot in the ring buffer, then waits if
    /// necessary for the slowest consumer to advance (asynchronous backpressure).
    /// Once a slot is confirmed to be free, it writes the event and makes it visible
    /// to all consumers.
    ///
    /// # Arguments
    ///
    /// * `event`: The event to be published.
    ///
    /// # Returns
    ///
    /// * `Ok(())`: If the event was successfully published.
    /// * `Err(T)`: If there are currently no active `Consumer`s. In this case, the
    ///   event is not published and is returned to the caller.
    ///
    /// # Cancellation Safety
    ///
    /// This method **is** cancellation safe. A publisher task first acquires a temporary,
    /// exclusive lock to "claim" a slot. If the future is cancelled at any `await`
    /// point (e.g., while waiting for backpressure), the lock is automatically
    /// released, and the global state remains consistent. No sequence number "gap"
    /// is created.
    ///
    /// The next publisher will simply acquire the lock and attempt to publish to the
    /// same slot. It is safe to use this method in `tokio::select!`, timeouts, etc.
    pub async fn publish(&self, event: T) -> Result<(), T> {
        let bus = &self.bus;
        let controller = &self.controller;
        let buffer = &self.bus.buffer;

        let next_seq_guard = controller.claim_slot().await;
        let next_seq = *next_seq_guard;

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

        unsafe {
            *buffer.get(next_seq) = Some(event);
        }

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
        let next_seq = controller.claim_slot().await;
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
