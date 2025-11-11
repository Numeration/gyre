use crate::cursor::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;
use tokio::sync::futures::Notified;

#[derive(Debug)]
struct SharedState {
    closed: AtomicBool,
    cursor: Cursor,
    consumer_notify: Notify,
    publisher_notify: Notify,
}

impl SharedState {
    fn new(cursor: Cursor) -> Self {
        Self {
            closed: AtomicBool::new(false),
            cursor,
            consumer_notify: Notify::new(),
            publisher_notify: Notify::new(),
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.consumer_notify.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) struct SequenceNotifier(Arc<SharedState>);

impl Drop for SequenceNotifier {
    fn drop(&mut self) {
        // 发布者关闭时，唤醒所有等待者。
        self.0.close();
    }
}

impl SequenceNotifier {
    pub(crate) fn notify(&self) {
        self.0.consumer_notify.notify_one();
    }

    pub(crate) fn waiter(&self) -> Notified<'_> {
        self.0.publisher_notify.notified()
    }

    /// 发布指定序列号（等待 Cursor 前进到 sequence）
    pub(crate) async fn publish(&self, sequence: i64) {
        // 等待直到成功推进游标
        while self
            .0
            .cursor
            .compare_exchange(sequence - 1, sequence)
            .is_err()
        {
            tokio::task::yield_now().await;
        }

        // 通知所有等待的订阅者
        self.0.consumer_notify.notify_waiters();
    }

    pub(crate) fn subscribe(&self) -> SequenceWaiter {
        SequenceWaiter(self.0.clone())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SequenceWaiter(Arc<SharedState>);

impl SequenceWaiter {
    pub(crate) fn notify(&self) {
        self.0.publisher_notify.notify_waiters();
    }

    /// 等待直到游标 >= target
    pub(crate) async fn wait_until(&self, target: i64) -> Result<(), ()> {
        loop {
            // Double check
            if self.0.cursor.relaxed() > target {
                return Ok(());
            }

            if self.0.is_closed() {
                return Err(());
            }

            let notified = self.0.consumer_notify.notified();

            if self.0.cursor.relaxed() > target {
                return Ok(());
            }

            if self.0.is_closed() {
                return Err(());
            }

            notified.await;
        }
    }
}

/// 创建一个发布/订阅对
pub(crate) fn channel(cursor: Cursor) -> (SequenceNotifier, SequenceWaiter) {
    let shared = Arc::new(SharedState::new(cursor));
    (SequenceNotifier(shared.clone()), SequenceWaiter(shared))
}
