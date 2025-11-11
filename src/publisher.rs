use crate::cursor::Cursor;
use crate::fence::Fence;
use crate::{Bus, Consumer, fence, sequence_barrier};
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use futures::future::BoxFuture;
use pin_project::pin_project;

#[allow(dead_code)]
struct SequenceGuard<'a>(i64, fence::Guard<'a>);

impl Deref for SequenceGuard<'_> {
    type Target = i64;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[derive(Debug)]
struct SequenceController {
    claim_fence: Fence,
    claimed: Cursor,
    notifier: sequence_barrier::Publisher,
}

impl SequenceController {
    fn new(notifier: sequence_barrier::Publisher) -> Self {
        Self {
            claim_fence: Default::default(),
            claimed: Default::default(),
            notifier,
        }
    }

    async fn acquire_sequence(&self) -> SequenceGuard<'_> {
        let guard = self.claim_fence.acquire().await;
        SequenceGuard(self.claimed.relaxed(), guard)
    }
    async fn next_sequence(&self) -> i64 {
        self.claim_fence.until_released().await;
        self.claimed.fetch_add(1)
    }

    fn notify_one(&self) {
        self.notifier.notify_one();
    }

    async fn publish(&self, sequence: i64) {
        self.notifier.publish(sequence).await;
    }

    fn subscribe(&self) -> sequence_barrier::Subscriber {
        self.notifier.subscribe()
    }
}

#[pin_project]
pub struct OwnedSubscribe<T>(#[pin] BoxFuture<'static, Consumer<T>>);

impl<T> Future for OwnedSubscribe<T> {
    type Output = Consumer<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().0.poll(cx)
    }
}

#[derive(Debug)]
pub struct Publisher<T> {
    bus: Arc<Bus<T>>,
    controller: Arc<SequenceController>,
}

impl<T> Clone for Publisher<T> {
    fn clone(&self) -> Self {
        Self {
            bus: Arc::clone(&self.bus),
            controller: Arc::clone(&self.controller),
        }
    }
}

impl<T> Publisher<T> {
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
        let buffer = bus.get_buffer().await;

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
                controller.publish(next_seq).await;
                return; // 没有消费者，不需要等待
            };

            if min_seq < next_seq {
                let Ok(offset) = usize::try_from(next_seq - min_seq) else {
                    drop(consumers);
                    tokio::task::yield_now().await;
                    continue;
                };

                if offset >= buffer.capacity() {
                    // 缓冲区满，等待消费者消费
                    drop(consumers);
                    tokio::task::yield_now().await;
                    continue;
                }
            }

            break;
        }

        // 写入数据
        unsafe {
            *buffer.get(next_seq) = Some(event);
        }

        // 发布
        controller.publish(next_seq).await;
    }

    pub async fn subscribe(&self) -> Consumer<T> {
        let bus = self.bus.clone();
        let controller = &self.controller;
        let next_seq = controller.acquire_sequence().await;
        let subscriber = controller.subscribe();

        Consumer::new(bus, subscriber, *next_seq - 1)
    }

}

impl<T:  Send + Sync + 'static> Publisher<T> {

    pub fn subscribe_owned(&self) -> OwnedSubscribe<T> {
        let bus = self.bus.clone();
        let controller = self.controller.clone();

        OwnedSubscribe(Box::pin(async move {
            let next_seq = controller.acquire_sequence().await;
            let subscriber = controller.subscribe();

            Consumer::new(bus, subscriber, *next_seq - 1)
        }))
    }

}
