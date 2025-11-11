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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cursor_new_and_relaxed_load() {
        let cursor = Cursor::new(42);
        assert_eq!(cursor.relaxed(), 42);

        let default_cursor = Cursor::default();
        assert_eq!(default_cursor.relaxed(), 0);
    }

    #[test]
    fn test_cursor_fetch_add() {
        let cursor = Cursor::new(100);

        // fetch_add returns the old value
        let old_value = cursor.fetch_add(5);
        assert_eq!(old_value, 100);
        assert_eq!(cursor.relaxed(), 105);

        let old_value = cursor.fetch_add(-10);
        assert_eq!(old_value, 105);
        assert_eq!(cursor.relaxed(), 95);
    }

    #[test]
    fn test_cursor_compare_exchange_success() {
        let cursor = Cursor::new(50);

        // Successful exchange: current value is 50, new value is 51
        let result = cursor.compare_exchange(50, 51);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 50); // Returns the old value
        assert_eq!(cursor.relaxed(), 51);
    }

    #[test]
    fn test_cursor_compare_exchange_failure() {
        let cursor = Cursor::new(50);

        // Failed exchange: expected value is 49, actual value is 50
        let result = cursor.compare_exchange(49, 51);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 50); // Returns the current actual value
        assert_eq!(cursor.relaxed(), 50); // The value remains unchanged
    }

    #[test]
    fn test_cursor_equality() {
        let c1 = Cursor::new(10);
        let c2 = Cursor::new(10);
        let c3 = Cursor::new(20);

        // PartialEq<Cursor>
        assert_eq!(c1, c2);
        assert_ne!(c1, c3);

        // PartialEq<i64>
        assert_eq!(c1, 10);
        assert_ne!(c1, 11);
    }

    #[test]
    fn test_cursor_ordering() {
        let c1 = Cursor::new(10);
        let c2 = Cursor::new(20);
        let c3 = Cursor::new(10);

        // Ord / PartialOrd<Cursor>
        assert!(c1 < c2);
        assert!(c2 > c1);
        assert!(c1 <= c3);
        assert!(c1 >= c3);

        // PartialOrd<i64>
        assert!(c1 < 11);
        assert!(c2 > 15);
        assert!(c1 <= 10);
    }
}
