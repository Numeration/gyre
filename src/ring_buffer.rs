use crate::sequence::Sequence;
use std::cell::UnsafeCell;

#[derive(Debug)]
pub struct RingBuffer<E> {
    slots: Box<[UnsafeCell<Option<E>>]>,
    index_mask: usize,
}

unsafe impl<E: Sync> Sync for RingBuffer<E> {}

impl<E: Send + Sync + 'static> RingBuffer<E> {
    pub(crate) fn new(capacity: usize) -> Self {
        assert!(capacity >= 2, "capacity must be at least 2");
        assert!(capacity.is_power_of_two(), "capacity must be a power of 2");

        // 初始化槽位
        let slots = (0..capacity)
            .map(|_| UnsafeCell::new(None))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Self {
            slots,
            index_mask: capacity - 1,
        }
    }

    #[inline]
    pub(crate) fn get(&self, sequence: impl Sequence) -> *mut Option<E> {
        let index = sequence.get_index_from(self.index_mask);
        // SAFETY: index 已被 mask 限制在范围内
        unsafe { self.slots.get_unchecked(index).get() }
    }

    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.slots.len()
    }
}
