use crate::consumer_id::ConsumerIds;
use crate::cursor::Cursor;
use crate::ring_buffer::RingBuffer;

use crate::{Consumer, Publisher, sequence_barrier};
use crossbeam_utils::CachePadded;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) struct Bus<T> {
    pub(crate) ids: ConsumerIds,
    pub(crate) buffer: RingBuffer<T>,
    pub(crate) consumers: papaya::HashMap<u64, CachePadded<Cursor>>,
}

impl<T> Bus<T> {
    fn new(capacity: usize) -> Self {
        assert!(capacity >= 2, "capacity must be at least 2");
        assert!(capacity.is_power_of_two(), "capacity must be a power of 2");
        Self {
            ids: ConsumerIds::default(),
            buffer: RingBuffer::new(capacity),
            consumers: Default::default(),
        }
    }
}

pub fn channel<E: Send + Sync + 'static>(capacity: usize) -> (Publisher<E>, Consumer<E>) {
    let bus = Arc::new(Bus::new(capacity));

    let (tx, rx) = sequence_barrier::sequence_barrier_pair(Cursor::new(-1));

    (
        Publisher::new(Arc::clone(&bus), tx),
        Consumer::new(bus, rx, -1),
    )
}
