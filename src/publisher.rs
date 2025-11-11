use crate::cursor::Cursor;
use crate::fence::Fence;
use crate::sequence_barrier::{SequenceNotifier, SequenceWaiter};
use crate::{Bus, Consumer, fence};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::futures::Notified;

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
    notifier: SequenceNotifier,
}

impl SequenceController {
    fn new(notifier: SequenceNotifier) -> Self {
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
        self.notifier.notify();
    }

    async fn publish(&self, sequence: i64) {
        self.notifier.publish(sequence).await;
    }

    fn consumer_progress_waiter(&self) -> Notified<'_> {
        self.notifier.waiter()
    }

    fn subscriber_waiter(&self) -> SequenceWaiter {
        self.notifier.subscribe()
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
    pub(crate) fn new(bus: Arc<Bus<T>>, notifier: SequenceNotifier) -> Self {
        Self {
            bus,
            controller: Arc::new(SequenceController::new(notifier)),
        }
    }

    /// 发布一个事件（等待消费者进度并写入 RingBuffer）
    pub async fn publish(&self, event: T) -> Result<(), T> {
        let bus = &self.bus;
        let controller = &self.controller;
        let next_seq = controller.next_sequence().await;
        let buffer = &self.bus.buffer;

        // 等待 ring buffer 有空位
        loop {
            let waiter = controller.consumer_progress_waiter();

            let consumers = bus.consumers.pin_owned();
            let Some(min_seq) = consumers
                .values()
                .map(Deref::deref)
                .map(Cursor::relaxed)
                .min()
            else {
                controller.publish(next_seq).await;
                return Err(event); // 没有消费者，不需要等待
            };

            if min_seq < next_seq {
                let Ok(offset) = usize::try_from(next_seq - min_seq) else {
                    drop(consumers);
                    controller.notify_one();
                    waiter.await;
                    continue;
                };

                if offset >= buffer.capacity() {
                    // 缓冲区满，等待消费者消费
                    drop(consumers);
                    controller.notify_one();
                    waiter.await;
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
        Ok(())
    }

    pub async fn subscribe(&self) -> Consumer<T> {
        let bus = self.bus.clone();
        let controller = &self.controller;
        let next_seq = controller.acquire_sequence().await;
        let subscriber = controller.subscriber_waiter();

        Consumer::new(bus, subscriber, *next_seq - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel;
    use tokio::time::{timeout, Duration};

    /// 测试基础的发布和消费流程。
    #[tokio::test]
    async fn test_basic_publish_and_consume() {
        let (tx, mut rx) = channel::<i32>(4);

        // 1. 发布事件
        tx.publish(100).await.unwrap();
        tx.publish(200).await.unwrap();

        // 2. 消费事件 1
        let guard1 = rx.next().await.unwrap();
        assert_eq!(*guard1, 100);
        // guard1 drop 时游标前进
        drop(guard1);

        // 3. 消费事件 2
        let guard2 = rx.next().await.unwrap();
        assert_eq!(*guard2, 200);
    }

    /// 测试发布者在没有消费者时，事件被丢弃。
    #[tokio::test]
    async fn test_publish_no_consumer() {
        let (tx, rx) = channel::<i32>(4);

        // 丢弃初始消费者
        drop(rx);

        // 发布者应返回 Err(T)，表示事件无法发布
        let result = tx.publish(999).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), 999);
    }

    /// 测试背压机制：当 RingBuffer 满时，Publisher 会阻塞。
    #[tokio::test]
    async fn test_backpressure_blocking() {
        // 容量为 2 的 RingBuffer
        let (tx, mut rx) = channel::<u32>(2);

        // 1. 发布 0, 1 (填满缓冲区)
        tx.publish(0).await.unwrap();
        tx.publish(1).await.unwrap();

        // 2. 尝试发布 2 (缓冲区已满，发布者应该阻塞)
        let tx_clone = tx.clone();
        let mut publish_task = tokio::spawn(async move {
            tx_clone.publish(2).await.unwrap();
        });

        // 确保发布任务在 10ms 内没有完成（即被阻塞）
        assert!(
            timeout(Duration::from_millis(10), &mut publish_task)
                .await
                .is_err(),
            "Publisher should be blocked due to backpressure"
        );

        // 3. 消费者消费一个事件 (释放一个槽位)
        let g0 = rx.next().await.unwrap();
        assert_eq!(*g0, 0);
        drop(g0); // 游标前进

        // 4. 此时，发布任务应该立即被唤醒并完成
        timeout(Duration::from_millis(10), publish_task)
            .await
            .expect("Publisher should be unblocked")
            .unwrap();

        // 5. 验证事件 2 已发布
        let g2 = rx.next().await.unwrap();
        assert_eq!(*g2, 2);
    }

    /// 测试动态订阅功能。
    #[tokio::test]
    async fn test_dynamic_subscribe() {
        let (tx, mut rx1) = channel::<String>(4);

        // 1. rx1 消费初始事件
        tx.publish("A".to_string()).await.unwrap();
        let g_a = rx1.next().await.unwrap();
        assert_eq!(*g_a, "A");
        // 此时全局游标是 0，rx1 游标是 -1

        // 2. 动态创建订阅者 rx2
        let mut rx2 = tx.subscribe().await;

        // 3. 发布事件 B 和 C
        tx.publish("B".to_string()).await.unwrap();
        tx.publish("C".to_string()).await.unwrap();

        // 4. 验证 rx1 收到所有事件
        // rx1 应该先消费 A (Drop 之后)，再消费 B, C
        drop(g_a); // 游标前进

        let g_b1 = rx1.next().await.unwrap();
        assert_eq!(*g_b1, "B");
        drop(g_b1);

        let g_c1 = rx1.next().await.unwrap();
        assert_eq!(*g_c1, "C");
        drop(g_c1);

        // 5. 验证 rx2 仅收到订阅之后的事件 B 和 C
        // rx2 初始化时游标位置应该在 B 之前。
        let g_b2 = rx2.next().await.unwrap();
        assert_eq!(*g_b2, "B");
        drop(g_b2);

        let g_c2 = rx2.next().await.unwrap();
        assert_eq!(*g_c2, "C");
        drop(g_c2);

        // 6. 验证 rx2 无法获取更多事件
        let result = timeout(Duration::from_millis(10), rx2.next()).await;
        assert!(result.is_err());
    }
}