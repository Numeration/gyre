//! Defines `Bus`, the central shared state for a `gyre` channel.
//!
//! The `Bus` struct encapsulates the ring buffer (`RingBuffer`) for storing events,
//! a tracker for generating consumer IDs, and a map to track the consumption
//! progress (`Cursor`) of each individual consumer.
//!
//! It is designed to be wrapped in an `Arc` and shared among all `Publisher`
//! and `Consumer` instances, coordinating the state of the entire publish-subscribe system.

use crate::consumer_id::ConsumerIds;
use crate::cursor::Cursor;
use crate::ring_buffer::RingBuffer;

use crate::{Consumer, Publisher, sequence_barrier};
use crossbeam_utils::CachePadded;
use std::sync::Arc;

/// `Bus` is the core data structure shared by all publishers and consumers.
///
/// It contains the event buffer and the state of all consumers. This struct is not
/// intended for direct manipulation by the end-user, but is managed internally
/// through `Publisher` and `Consumer` handles.
#[derive(Debug)]
pub(crate) struct Bus<T> {
    /// Used to generate unique IDs for new `Consumer`s.
    pub(crate) ids: ConsumerIds,

    /// The underlying `RingBuffer` that actually stores events of type `T`.
    pub(crate) buffer: RingBuffer<T>,

    /// A concurrent hash map for tracking the consumption progress (cursor) of each
    /// active consumer.
    ///
    /// - Key: The consumer's unique ID (`u64`).
    /// - Value: The consumer's `Cursor`.
    ///
    /// This map is crucial for the `Publisher`'s backpressure implementation,
    /// as it needs to find the slowest consumer by querying all cursors
    /// to ensure that no unconsumed events are overwritten.
    pub(crate) consumers: papaya::HashMap<u64, CachePadded<Cursor>>,
}

impl<T> Bus<T> {
    /// Creates a new `Bus` instance.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is less than 2 or not a power of two.
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity >= 2, "capacity must be at least 2");
        assert!(capacity.is_power_of_two(), "capacity must be a power of 2");
        Self {
            ids: ConsumerIds::default(),
            buffer: RingBuffer::new(capacity),
            consumers: Default::default(),
        }
    }
}

/// Creates a new asynchronous, multi-producer, multi-consumer channel.
///
/// This is the main entry point for creating a `gyre` channel. It returns a pair of handles:
/// a `Publisher` for sending events and a `Consumer` for receiving them.
/// Additional consumers can be created via `publisher.subscribe()` or `consumer.clone()`.
///
/// # Arguments
///
/// * `capacity`: The size of the internal ring buffer. This is the maximum number of events
///   that can be in-flight. **The capacity must be a power of two and at least 2.**
///
/// # Returns
///
/// A tuple containing the `Publisher` and the initial `Consumer`.
///
/// # Panics
///
/// Panics if `capacity` is not a power of two or is less than 2.
///
/// # Example
///
/// ```
/// use gyre::channel;
/// use tokio;
///
/// #[tokio::main]
/// async fn main() {
///     let (tx, mut rx) = channel::<i32>(4);
///
///     tokio::spawn(async move {
///         tx.publish(10).await.unwrap();
///     });
///
///     let event = rx.next().await.unwrap();
///     assert_eq!(*event, 10);
/// }
/// ```
pub fn channel<E: Send + Sync + 'static>(capacity: usize) -> (Publisher<E>, Consumer<E>) {
    let bus = Arc::new(Bus::new(capacity));

    // Initialize the publisher's sequence cursor to -1, indicating no events have been published yet.
    let (tx, rx) = sequence_barrier::sequence_barrier_pair(Cursor::new(-1));

    (Publisher::new(bus.clone(), tx), Consumer::new(bus, rx, -1))
}
