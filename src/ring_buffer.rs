//! Defines `RingBuffer`, the core data storage structure for the `gyre` event system.
//!
//! `RingBuffer` is a fixed-size, `UnsafeCell`-based circular buffer.
//! It is designed to provide high-performance, lock-free data exchange for
//! multi-producer and multi-consumer concurrency scenarios.
//!
//! All safety relies on external atomic sequence numbers (`Cursor`) for coordination,
//! rather than on traditional locking mechanisms. A publisher "claims" a slot before
//! writing to it, and consumers only read from a slot after it has been confirmed as published.

use crate::sequence::Sequence;
use std::cell::UnsafeCell;

/// A fixed-size circular buffer for storing events.
///
/// `RingBuffer` internally uses an array of `UnsafeCell<Option<E>>` to store elements,
/// which allows multiple threads to concurrently read and write to different slots
/// without locks.
///
/// # Safety
///
/// The `Send` and `Sync` traits for this struct are implemented `unsafe`ly, and its
/// safety is based on the following conventions:
///
/// 1.  **External Coordination**: All read and write operations must be strictly
///     coordinated by external atomic sequence numbers (Sequences/Cursors).
///     The `RingBuffer` itself provides no synchronization mechanisms.
/// 2.  **Single Writer Principle**: For any given sequence number (i.e., a specific
///     slot in the buffer), at most one publisher can have write access at any
///     given time. This is guaranteed by the `Publisher`'s sequence claiming logic.
/// 3.  **Read-Write Barrier**: Consumers can only read a slot after the publisher
///     has finished writing and updated the globally visible publish sequence number.
///     This ensures that consumers do not read partially written data.
///
/// In short, the safety of `RingBuffer` depends on the correct implementation of the
/// higher-level `Publisher` and `Consumer`.
#[derive(Debug)]
pub struct RingBuffer<E> {
    /// The array of slots storing the events. `UnsafeCell` provides interior mutability.
    slots: Box<[UnsafeCell<Option<E>>]>,

    /// A mask used to quickly map a sequence number to an array index.
    /// Its value is `capacity - 1`.
    index_mask: usize,
}

// SAFETY: The thread safety of `RingBuffer` is guaranteed by the external atomic
// sequence number coordination mechanism.
// As long as `E` is `Send`, moving `E` between threads is safe.
// As long as `E` is `Sync`, sharing `&E` across threads is safe, provided there
// are no concurrent writes. The `gyre` architecture guarantees these preconditions.
unsafe impl<E: Send + Sync> Send for RingBuffer<E> {}
unsafe impl<E: Send + Sync> Sync for RingBuffer<E> {}

impl<E> RingBuffer<E> {
    /// Creates a new `RingBuffer` instance.
    ///
    /// # Arguments
    ///
    /// * `capacity`: The capacity of the buffer. For performance reasons, this value
    ///   must be a power of two.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is less than 2 or not a power of two.
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity >= 2, "capacity must be at least 2");
        assert!(capacity.is_power_of_two(), "capacity must be a power of 2");

        // Initialize slots to `None`
        let slots = (0..capacity)
            .map(|_| UnsafeCell::new(None))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            slots,
            index_mask: capacity - 1,
        }
    }

    /// Gets a mutable pointer to a slot based on its sequence number.
    ///
    /// This method returns a raw pointer, and the caller must guarantee that
    /// access through this pointer is safe.
    ///
    /// # Arguments
    ///
    /// * `sequence`: The sequence number of the event to access.
    ///
    /// # Returns
    ///
    /// A mutable pointer to an `Option<E>`.
    ///
    /// # Safety
    ///
    /// Calling this method is safe, but dereferencing the returned pointer is `unsafe`
    /// and must satisfy the following conditions:
    /// - **For writing**: The caller must have exclusive write access for this `sequence`.
    /// - **For reading**: The caller must ensure that the write operation for this `sequence`
    ///   has completed and that the relevant memory operations are visible to the current
    ///   thread (typically achieved via a memory barrier).
    ///
    /// The use of `get_unchecked` is safe because `index_mask` guarantees that the
    /// calculated index will always be within the bounds of `slots`.
    #[inline]
    pub(crate) fn get(&self, sequence: impl Sequence) -> *mut Option<E> {
        let index = sequence.get_index_from(self.index_mask);
        // SAFETY: `index` is guaranteed by `index_mask` to be within the valid range of `slots`.
        unsafe { self.slots.get_unchecked(index).get() }
    }

    /// Returns the total capacity of the buffer.
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_creation_and_capacity() {
        // Verify capacity must be a power of two
        RingBuffer::<u32>::new(4);
        RingBuffer::<u32>::new(16);
        RingBuffer::<u32>::new(1024);

        // Verify capacity() returns the correct value
        let rb = RingBuffer::<u32>::new(8);
        assert_eq!(rb.capacity(), 8);

        // Verify minimum capacity is 2
        RingBuffer::<u32>::new(2);
    }

    #[test]
    #[should_panic(expected = "capacity must be a power of 2")]
    fn test_ring_buffer_invalid_capacity_not_power_of_two() {
        RingBuffer::<u32>::new(3);
    }

    #[test]
    #[should_panic(expected = "capacity must be at least 2")]
    fn test_ring_buffer_invalid_capacity_too_small() {
        RingBuffer::<u32>::new(1);
    }

    #[test]
    fn test_ring_buffer_indexing_logic() {
        // Capacity of 4
        let rb = RingBuffer::<u32>::new(4);
        assert_eq!(rb.index_mask, 3);

        // Sequence 0 -> Index 0
        assert_eq!(0.get_index_from(rb.index_mask), 0);
        // Sequence 3 -> Index 3
        assert_eq!(3.get_index_from(rb.index_mask), 3);
        // Sequence 4 -> Index 0
        assert_eq!(4.get_index_from(rb.index_mask), 0);
        // Sequence 7 -> Index 3
        assert_eq!(7.get_index_from(rb.index_mask), 3);
        // Sequence 8 -> Index 0
        assert_eq!(8.get_index_from(rb.index_mask), 0);

        // Negative sequences (for initial cursor at -1)
        assert_eq!((-1).get_index_from(rb.index_mask), 3);
        assert_eq!((-4).get_index_from(rb.index_mask), 0);
    }

    #[test]
    fn test_ring_buffer_write_and_read() {
        let rb = RingBuffer::<u32>::new(4);

        // Write to sequences 0, 1, 2, 3
        unsafe {
            *rb.get(0) = Some(10);
            *rb.get(1) = Some(20);
            *rb.get(2) = Some(30);
            *rb.get(3) = Some(40);
        }

        // Verify reads
        unsafe {
            assert_eq!(*rb.get(0), Some(10));
            assert_eq!(*rb.get(3), Some(40));
        }

        // Wrap around and write to sequence 4 (should overwrite the slot for sequence 0)
        unsafe {
            *rb.get(4) = Some(50);
        }

        // Verify read for sequence 4
        unsafe {
            assert_eq!(*rb.get(4), Some(50));
        }

        // Verify the slot for sequence 0 has been overwritten (by accessing index 0)
        assert_eq!(
            0.get_index_from(rb.index_mask),
            4.get_index_from(rb.index_mask)
        );
        unsafe {
            assert_eq!(*rb.get(0), Some(50));
        }
    }

    #[test]
    fn test_ring_buffer_stores_initial_none() {
        let rb = RingBuffer::<String>::new(4);

        // Ensure all slots are initially None
        for i in 0..rb.capacity() {
            let ptr = rb.get(i as i64);
            unsafe {
                assert!((*ptr).is_none());
            }
        }
    }
}
