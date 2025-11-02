use crossbeam_utils::CachePadded;
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicI64, Ordering};

#[derive(Debug, Default)]
pub(crate) struct Cursor {
    value: CachePadded<AtomicI64>,
}

impl Cursor {
    pub(crate) fn new(val: i64) -> Self {
        Self {
            value: CachePadded::new(AtomicI64::new(val)),
        }
    }

    #[inline]
    pub(crate) fn fetch_add(&self, delta: i64) -> i64 {
        self.value.fetch_add(delta, Ordering::AcqRel)
    }

    #[inline]
    pub(crate) fn compare_exchange(&self, current: i64, next: i64) -> Result<i64, i64> {
        self.value
            .compare_exchange(current, next, Ordering::AcqRel, Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn store(&self, val: i64) {
        self.value.store(val, Ordering::Release);
    }

    #[inline]
    pub(crate) fn relaxed(&self) -> i64 {
        self.value.load(Ordering::Relaxed)
    }
}

// Eq / PartialEq
impl PartialEq for Cursor {
    fn eq(&self, other: &Self) -> bool {
        self.relaxed() == other.relaxed()
    }
}

impl PartialEq<i64> for Cursor {
    fn eq(&self, other: &i64) -> bool {
        self.relaxed() == *other
    }
}

impl Eq for Cursor {}

// Ord / PartialOrd
impl Ord for Cursor {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        self.relaxed().cmp(&other.relaxed())
    }
}

impl PartialOrd for Cursor {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl PartialOrd<i64> for Cursor {
    fn partial_cmp(&self, other: &i64) -> Option<CmpOrdering> {
        Some(self.relaxed().cmp(other))
    }
}
