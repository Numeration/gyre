use crossbeam_utils::CachePadded;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub struct Guard<'a>(&'a Fence);

impl<'a> Drop for Guard<'a> {
    fn drop(&mut self) {
        self.0.flag.store(false, Ordering::Release);
    }
}

pub struct OwnedGuard(Arc<Fence>);

impl Drop for OwnedGuard {
    fn drop(&mut self) {
        self.0.flag.store(false, Ordering::Release);
    }
}

#[derive(Debug, Default)]
pub struct Fence {
    flag: CachePadded<AtomicBool>,
}

impl Fence {
    pub async fn acquire(&self) -> Guard<'_> {
        while self
            .flag
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tokio::task::yield_now().await;
        }

        Guard(self)
    }

    pub async fn acquire_owned(self: &Arc<Self>) -> OwnedGuard {
        while self
            .flag
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tokio::task::yield_now().await;
        }

        OwnedGuard(Arc::clone(&self))
    }

    pub async fn until_released(&self) {
        while self.flag.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }
}
