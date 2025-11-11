use crossbeam_utils::CachePadded;
use std::sync::atomic::AtomicU64;

#[derive(Debug, Default)]
pub(crate) struct ConsumerIds {
    id: CachePadded<AtomicU64>,
}

impl ConsumerIds {
    pub(crate) fn next_id(&self) -> u64 {
        self.id.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}
