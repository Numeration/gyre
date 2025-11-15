//! Defines the `Consumer` side of a `gyre` channel and the `EventGuard` used for accessing events.
//!
//! A `Consumer` is responsible for asynchronously reading events from the shared `Bus`.
//! Each `Consumer` maintains its own independent consumption progress (cursor),
//! allowing multiple consumers to process the same event stream in parallel at
//! different rates.
//!
//! A core design principle of this module is the use of `EventGuard` and `OwnedEventGuard`.
//! They are RAII-style smart pointers that automatically advance the corresponding
//! consumer's cursor when they are `drop`ped. This ensures that a buffer slot is
//! only marked as reusable after the event has been fully processed, which
//! simplifies code and enhances safety.

use crate::cursor::Cursor;
use crate::sequence_barrier::SequenceWaiter;
use crate::{Bus, fence};
use std::ops::Deref;
use std::sync::Arc;

/// An RAII guard representing a borrowed event from the channel that is currently being processed.
///
/// When `consumer.next().await` returns `Some(event)`, you get an `EventGuard`.
/// It provides read-only access to the underlying event data `T` through the `Deref` trait.
///
/// **Most important feature**: When an `EventGuard`'s lifetime ends (i.e., it is `drop`ped),
/// it automatically notifies its parent `Consumer` to advance its consumption cursor by 1.
/// This signals that the event has been successfully processed, and the corresponding slot
/// in the ring buffer can now be reused by a `Publisher`.
///
/// ```rust,ignore
/// let event_guard = consumer.next().await.unwrap();
/// println!("Processing: {}", *event_guard);
/// // Here, `event_guard` is dropped, and the consumer's cursor advances automatically.
/// ```
pub struct EventGuard<'a, T> {
    value: *const T,
    consumer: &'a Consumer<T>,
    _guard: fence::Guard<'a>,
}

// SAFETY: An `EventGuard` is essentially a read-only reference to `T`, where `T` is
// constrained by `Sync + Send`. The `Consumer` reference it holds also guarantees
// its lifetime. Therefore, it is thread-safe.
unsafe impl<T: Sync + Send> Sync for EventGuard<'_, T> {}
unsafe impl<T: Sync + Send> Send for EventGuard<'_, T> {}

impl<'a, T> Deref for EventGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `value` always points to a valid, initialized element in the ring buffer.
        // This is guaranteed by the sequence number coordination between the `Publisher`
        // and `Consumer`. The access is read-only, so there are no data races.
        unsafe { &*self.value }
    }
}

impl<'a, T> Drop for EventGuard<'a, T> {
    fn drop(&mut self) {
        // When event processing is complete (the guard is dropped), the consumer's cursor is advanced by 1.
        self.consumer.advance()
    }
}

/// An owned version of `EventGuard`, used for `Consumer`s wrapped in an `Arc`.
///
/// When you need to share a single consumption stream across multiple asynchronous tasks,
/// you wrap the `Consumer` in an `Arc<Consumer>` and use the `.next_owned()` method.
/// This method returns an `OwnedEventGuard`.
///
/// Its behavior is identical to `EventGuard`: when it is `drop`ped, it automatically
/// advances the consumer's cursor.
pub struct OwnedEventGuard<T> {
    value: *const T,
    consumer: Arc<Consumer<T>>,
    _guard: fence::OwnedGuard,
}

// SAFETY: Same reasoning as `EventGuard`. The `Arc<Consumer>` guarantees the `Consumer`'s lifetime.
unsafe impl<T: Sync + Send> Sync for OwnedEventGuard<T> {}
unsafe impl<T: Sync + Send> Send for OwnedEventGuard<T> {}

impl<T> Deref for OwnedEventGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: Same reasoning as `EventGuard::deref`.
        unsafe { &*self.value }
    }
}

impl<T> Drop for OwnedEventGuard<T> {
    fn drop(&mut self) {
        // When event processing is complete, the consumer's cursor is advanced by 1.
        self.consumer.advance()
    }
}

/// The receiving end of a `gyre` channel.
///
/// `Consumer` reads events from the shared ring buffer. Each `Consumer` instance
/// maintains its own independent consumption progress, so a slow consumer will not
/// block others.
///
/// `Consumer` is `Send` but not `Sync`. This means you can move a `Consumer` into
/// another task, but you cannot share it across multiple tasks and call its `.next()`
/// method concurrently (as it requires `&mut self`). If you need to drive the same
/// consumption stream from multiple tasks, wrap it in an `Arc<Consumer>` and use `.next_owned()`.
#[derive(Debug)]
pub struct Consumer<T> {
    bus: Arc<Bus<T>>,
    sequence: SequenceWaiter,
    id: u64,
    fence: Arc<fence::Fence>,
}

// SAFETY: The `Consumer`'s internal state is synchronized via `Arc` and atomic
// operations, making it safe to move between threads.
unsafe impl<T: Sync + Send> Send for Consumer<T> {}
unsafe impl<T: Sync + Send> Sync for Consumer<T> {}

impl<T> Clone for Consumer<T> {
    /// Creates a new `Consumer` that starts consuming from the exact same position as the original.
    ///
    /// The cloned `Consumer` gets a new unique ID. Specifically, the new consumer's
    /// cursor is set to the sequence number of the **last event successfully processed**
    /// by the original consumer. Therefore, both will start waiting for the same next
    /// event. Afterward, the two `Consumer`s will advance their progress independently.
    ///
    /// This is very useful for creating multiple parallel workers to process the same event stream.
    ///
    /// # Contrast with `publisher.subscribe()`
    ///
    /// `subscribe()` creates a `Consumer` that starts receiving **future** new events,
    /// whereas `clone()` creates a `Consumer` that starts from the **current** position.
    fn clone(&self) -> Self {
        // Assign a new ID for the new consumer
        let id = self.bus.ids.next_id();

        // Copy the cursor position of the current consumer
        let consumers = self.bus.consumers.pin();
        let current = consumers.get(&self.id).unwrap().relaxed();
        consumers.insert(id, Cursor::new(current).into());

        Self {
            bus: Arc::clone(&self.bus),
            id,
            sequence: self.sequence.clone(),
            fence: Default::default(),
        }
    }
}

impl<T> Consumer<T> {
    /// Creates a new `Consumer` instance. For internal `gyre` use only.
    pub(crate) fn new(bus: Arc<Bus<T>>, sequence: SequenceWaiter, init_cursor: i64) -> Self {
        let id = bus.ids.next_id();
        bus.consumers
            .pin()
            .insert(id, Cursor::new(init_cursor).into());
        let fence = Default::default();

        Self {
            bus,
            id,
            sequence,
            fence,
        }
    }

    /// Atomically advances this consumer's cursor by 1 and notifies any waiting `Publisher`.
    ///
    /// This method is called automatically by the `drop` implementations of `EventGuard`
    /// and `OwnedEventGuard`.
    fn advance(&self) {
        let id = self.id;
        let consumers = self.bus.consumers.pin();
        if let Some(cursor) = consumers.get(&id) {
            cursor.fetch_add(1);
        }
        self.sequence.notify();
    }

    /// Internal method: waits for the next event to become available and returns a raw pointer to it.
    async fn take_event(&self) -> Option<&T> {
        let consumers = self.bus.consumers.pin_owned();
        let current_seq = consumers.get(&self.id).unwrap().relaxed();

        // Wait until the Publisher has published a new event
        if self.sequence.wait_until(current_seq).await.is_err() {
            return None;
        }

        // Retrieve the next event
        let buffer = &self.bus.buffer;
        let value = unsafe {
            (&*buffer.get(current_seq + 1))
                .as_ref()
                .expect("Event must exist before publish")
        };

        Some(value)
    }

    /// Asynchronously waits for and retrieves the next event.
    ///
    /// This method will wait until a new event has been published by a `Publisher`.
    ///
    /// # Returns
    ///
    /// * `Some(EventGuard)`: If an event was successfully received.
    /// * `None`: If the channel is closed (i.e., all `Publisher`s have been dropped) and
    ///   there are no more events to consume in the buffer.
    ///
    /// # Note on Concurrency
    ///
    /// This method requires `&mut self`, ensuring that event consumption for a single
    /// `Consumer` instance is strictly ordered.
    ///
    /// # Cancellation Safety
    ///
    /// This method **is** cancellation safe. It only waits for a new event to become
    /// available but does not modify the consumer's state (i.e., its consumption cursor).
    /// The cursor is only advanced when the returned `EventGuard` is dropped.
    ///
    /// If the future from `next()` is cancelled, no `EventGuard` is returned, the cursor
    /// is not advanced, and the consumer's state remains consistent. The next call to
    /// `next()` will simply attempt to receive the same event again. This makes it safe
    /// to use in `tokio::select!`, timeouts, etc.
    pub async fn next(&mut self) -> Option<EventGuard<'_, T>> {
        let _guard = self.fence.acquire().await;
        let value = self.take_event().await?;

        Some(EventGuard {
            value,
            consumer: self,
            _guard,
        })
    }

    /// A variant of `next` for use with `Consumer`s wrapped in an `Arc`.
    ///
    /// Use this method when you need to share a single `Consumer` among multiple tasks.
    ///
    /// # Example
    /// ```rust,ignore
    /// use std::sync::Arc;
    /// let consumer = Arc::new(consumer);
    ///
    /// for _ in 0..2 {
    ///     let consumer_clone = consumer.clone();
    ///     tokio::spawn(async move {
    ///         if let Some(event) = consumer_clone.next_owned().await {
    ///             // ... process event
    ///         }
    ///     });
    /// }
    /// ```
    ///
    /// # Cancellation Safety
    ///
    /// This method **is** cancellation safe. It only waits for a new event to become
    /// available but does not modify the consumer's state (i.e., its consumption cursor).
    /// The cursor is only advanced when the returned `OwnedEventGuard` is dropped.
    ///
    /// If the future from `next_owned()` is cancelled, no `OwnedEventGuard` is returned,
    /// the cursor is not advanced, and the consumer's state remains consistent. The next
    /// call to `next_owned()` will simply attempt to receive the same event again.
    pub async fn next_owned(self: &Arc<Self>) -> Option<OwnedEventGuard<T>> {
        let _guard = self.fence.acquire_owned().await;
        let value = self.take_event().await?;

        Some(OwnedEventGuard {
            value,
            consumer: self.clone(),
            _guard,
        })
    }
}

impl<T> Drop for Consumer<T> {
    /// When a `Consumer` is dropped, it removes itself from the `Bus`'s tracking list.
    ///
    /// This is crucial for the backpressure mechanism, as the `Publisher` needs to know
    /// which consumers are active to avoid waiting indefinitely for a slow consumer
    /// that has already been dropped.
    fn drop(&mut self) {
        // Remove from the global list of consumers
        self.bus.consumers.pin().remove(&self.id);
        self.sequence.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Publisher, sequence_barrier};
    use tokio::time::{Duration, timeout};

    // Helper function to create a test environment and expose the Bus
    pub fn test_channel<E: Send + Sync + 'static>(
        capacity: usize,
    ) -> (Publisher<E>, Consumer<E>, Arc<Bus<E>>) {
        let bus = Arc::new(Bus::new(capacity));

        let (tx, rx) = sequence_barrier::sequence_barrier_pair(Cursor::new(-1));

        (
            Publisher::new(bus.clone(), tx),
            Consumer::new(bus.clone(), rx, -1),
            bus,
        )
    }

    // Helper function: get the current cursor value for a consumer with a specific ID
    fn get_consumer_cursor<T>(bus: &Arc<Bus<T>>, id: u64) -> i64 {
        bus.consumers
            .pin()
            .get(&id)
            .map(|c| c.relaxed())
            .unwrap_or(-999)
    }

    #[tokio::test]
    async fn test_consumer_drop_removes_from_bus() {
        let (_tx, rx, bus) = test_channel::<()>(4);
        let id = rx.id;

        // Confirm consumer is in the Bus
        assert!(bus.consumers.pin().contains_key(&id));

        // Drop the consumer
        drop(rx);

        // Confirm consumer has been removed from the Bus
        assert!(!bus.consumers.pin().contains_key(&id));
    }

    #[tokio::test]
    async fn test_raii_cursor_advancement() {
        let (tx, rx, bus) = test_channel::<u32>(4);
        let id = rx.id;
        let mut rx = rx;

        // Initial cursor position
        assert_eq!(get_consumer_cursor(&bus, id), -1);

        // Publish event 10
        tx.publish(10).await.unwrap();

        // 1. Get the event, but don't release the EventGuard
        let guard1 = rx.next().await.unwrap();
        assert_eq!(*guard1, 10);

        // Confirm the cursor has not advanced (still -1)
        assert_eq!(get_consumer_cursor(&bus, id), -1);

        // 2. Release the EventGuard, cursor advances
        drop(guard1);

        // Confirm cursor has advanced to 0 (consumed sequence 0)
        assert_eq!(get_consumer_cursor(&bus, id), 0);

        // 3. Publish event 20
        tx.publish(20).await.unwrap();

        // 4. Get event 20 and release it immediately
        let guard2 = rx.next().await.unwrap();
        assert_eq!(*guard2, 20);
        drop(guard2);

        // Confirm cursor has advanced to 1 (consumed sequence 1)
        assert_eq!(get_consumer_cursor(&bus, id), 1);
    }

    #[tokio::test]
    async fn test_consumer_cloning_and_multicast() {
        let (tx, rx1, bus) = test_channel::<i32>(4);
        let id1 = rx1.id;
        let mut rx1 = rx1;

        // 1. Publish initial event
        tx.publish(10).await.unwrap();

        // rx1 consumes event 10 (but doesn't drop the guard yet)
        let mut rx2 = rx1.clone();
        let g1 = rx1.next().await.unwrap();
        assert_eq!(*g1, 10);

        // 2. Clone consumer to create rx2
        let id2 = rx2.id;

        // Confirm rx2's cursor is the same as rx1's (-1)
        assert_eq!(get_consumer_cursor(&bus, id1), -1);
        assert_eq!(get_consumer_cursor(&bus, id2), -1);

        // 3. Publish events 20, 30
        tx.publish(20).await.unwrap();
        tx.publish(30).await.unwrap();

        // 4. rx2 independently consumes 10, 20
        let g2_1 = rx2.next().await.unwrap();
        assert_eq!(*g2_1, 10);
        drop(g2_1); // rx2 cursor -> 0

        let g2_2 = rx2.next().await.unwrap();
        assert_eq!(*g2_2, 20);
        drop(g2_2); // rx2 cursor -> 1

        // 5. rx1 continues consuming 20
        drop(g1); // rx1 cursor -> 0 (for consuming 10)

        let g1_2 = rx1.next().await.unwrap();
        assert_eq!(*g1_2, 20);
        drop(g1_2); // rx1 cursor -> 1

        // 6. Verify that both cursors advanced independently
        assert_eq!(get_consumer_cursor(&bus, id1), 1);
        assert_eq!(get_consumer_cursor(&bus, id2), 1);

        // Ensure both rx1 and rx2 are still in the bus
        assert!(bus.consumers.pin().contains_key(&id1));
        assert!(bus.consumers.pin().contains_key(&id2));
    }

    #[tokio::test]
    async fn test_exclusive_access_via_fence() {
        let (tx, rx, _) = test_channel::<u32>(4);
        let rx_arc = Arc::new(rx);

        tx.publish(100).await.unwrap();
        tx.publish(200).await.unwrap();

        let rx1_clone = rx_arc.clone();
        let rx1_task = tokio::spawn(async move {
            // Task 1 successfully gets event 100 and holds the EventGuard (and thus the Fence)
            let guard = rx1_clone
                .next_owned()
                .await
                .expect("Task 1 should get event");

            // Hold it for 100ms
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Confirm the value
            assert_eq!(*guard, 100);
        });

        // Wait for Task 1 to acquire the lock
        tokio::task::yield_now().await;

        let rx2_clone = rx_arc.clone();
        let mut rx2_task = tokio::spawn(async move {
            // Task 2 attempts to get the next event but should be blocked by the Fence
            let guard = rx2_clone
                .next_owned()
                .await
                .expect("Task 2 should eventually get event");
            assert_eq!(*guard, 200); // Should get 200
        });

        // Verify that Task 2 does not complete within 50ms (it's blocked)
        assert!(
            timeout(Duration::from_millis(50), &mut rx2_task)
                .await
                .is_err(),
            "Task 2 should be blocked by the fence held by Task 1"
        );

        // Wait for Task 1 to complete and release the Fence
        rx1_task.await.unwrap();

        // Task 2 should now complete quickly
        timeout(Duration::from_millis(50), rx2_task)
            .await
            .expect("Task 2 should be unblocked and finish")
            .unwrap();
    }

    #[tokio::test]
    async fn test_next_owned_consumption() {
        let (tx, rx, bus) = test_channel::<i32>(4);
        let rx_id = rx.id;
        let rx_arc = Arc::new(rx);

        // Publish an event
        tx.publish(555).await.unwrap();

        // 1. Get event using next_owned
        let owned_guard = rx_arc.next_owned().await.unwrap();
        assert_eq!(*owned_guard, 555);

        // 2. Confirm cursor has not yet advanced
        assert_eq!(get_consumer_cursor(&bus, rx_id), -1);

        // 3. Release the OwnedEventGuard
        drop(owned_guard);

        // 4. Confirm cursor has advanced
        assert_eq!(get_consumer_cursor(&bus, rx_id), 0);

        // 5. Verify next_owned again (ensuring Arc ref counts are correct)
        tx.publish(666).await.unwrap();
        let owned_guard_2 = rx_arc.next_owned().await.unwrap();
        assert_eq!(*owned_guard_2, 666);
    }
}
