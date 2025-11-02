use crate::cursor::Cursor;
use crate::{Bus, sequence_barrier, Consumer};
use crossbeam_utils::CachePadded;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug)]
struct SequenceController {
    claim_lock: CachePadded<AtomicBool>,
    claimed: CachePadded<Cursor>,
    notifier: sequence_barrier::Publisher,
}

struct SequenceGuard<'a> (i64, &'a AtomicBool);

impl Deref for SequenceGuard<'_> {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for SequenceGuard<'_> {
    fn drop(&mut self) {
        self.1.store(false, Ordering::Release);
    }
}

impl SequenceController {
    fn new(notifier: sequence_barrier::Publisher) -> Self {
        Self {
            claim_lock: Default::default(),
            claimed: Default::default(),
            notifier,
        }
    }

    async fn acquire_sequence(&self) -> SequenceGuard<'_> {
        while self
            .claim_lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            tokio::task::yield_now().await;
        }

        SequenceGuard(self.claimed.relaxed() + 1, &self.claim_lock)
    }

    async fn wait_lock_released(&self) {
        while self.claim_lock.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    }

    async fn next_sequence(&self) -> i64 {
        self.wait_lock_released().await;
        self.claimed.fetch_add(1)
    }

    fn notify_one(&self) {
        self.notifier.spin_once();
    }

    async fn publish(&self, sequence: i64) {
        self.notifier.publish(sequence).await;
    }

    fn subscribe(&self) -> sequence_barrier::Subscriber {
        self.notifier.subscribe()
    }
}

#[derive(Debug, Clone)]
pub struct Publisher<T> {
    bus: Arc<Bus<T>>,
    controller: Arc<SequenceController>,
}

impl<T: Send + Sync + 'static> Publisher<T> {
    pub(crate) fn new(bus: Arc<Bus<T>>, notifier: sequence_barrier::Publisher) -> Self {
        Self {
            bus,
            controller: Arc::new(SequenceController::new(notifier)),
        }
    }

    /// 发布一个事件（等待消费者进度并写入 RingBuffer）
    pub async fn publish(&self, event: T) {
        let bus = &self.bus;
        let controller = &self.controller;
        let next_seq = controller.next_sequence().await;

        // 等待 ring buffer 有空位
        loop {
            controller.notify_one();

            let consumers = bus.consumers.pin_owned();
            let Some(min_seq) = consumers
                .values()
                .map(Deref::deref)
                .map(Cursor::relaxed)
                .min()
            else {
                return; // 没有消费者，不需要等待
            };

            let Ok(offset) = usize::try_from(next_seq - min_seq) else {
                drop(consumers);
                tokio::task::yield_now().await;
                continue;
            };

            if offset >= bus.buffer.capacity() {
                // 缓冲区满，等待消费者消费
                drop(consumers);
                tokio::task::yield_now().await;
                continue;
            }

            break;
        }

        // 写入数据
        unsafe {
            std::ptr::replace(bus.buffer.get(next_seq), Some(event));
        }

        // 发布
        controller.publish(next_seq).await;
    }

    pub async fn subscribe(&self) -> Consumer<T> {
        let bus = self.bus.clone();
        let controller = &self.controller;
        let next_seq = controller.acquire_sequence().await;
        let subscriber = controller.subscribe();
        Consumer::new(bus, subscriber, *next_seq)
    }
}
