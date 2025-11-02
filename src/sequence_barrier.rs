use crate::cursor::Cursor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

#[derive(Debug)]
struct SharedState {
    closed: AtomicBool,
    cursor: Cursor,
    notify: Notify,
}

impl SharedState {
    fn new(cursor: Cursor) -> Self {
        Self {
            closed: AtomicBool::new(false),
            cursor,
            notify: Notify::new(),
        }
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) struct Publisher(Arc<SharedState>);

impl Drop for Publisher {
    fn drop(&mut self) {
        // 发布者关闭时，唤醒所有等待者。
        self.0.close();
    }
}

impl Publisher {
    pub(crate) fn spin_once(&self) {
        // 手动唤醒一个等待者
        self.0.notify.notify_one();
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
        self.0.notify.notify_waiters();
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Subscriber(Arc<SharedState>);

impl Subscriber {
    /// 等待直到游标 >= target
    pub(crate) async fn wait_until(&self, target: i64) -> Result<(), ()> {
        loop {
            if self.0.cursor.relaxed() > target {
                return Ok(());
            }

            if self.0.is_closed() {
                return Err(());
            }

            self.0.notify.notified().await;
        }
    }
}

/// 创建一个发布/订阅对
pub(crate) fn channel(cursor: Cursor) -> (Publisher, Subscriber) {
    let shared = Arc::new(SharedState::new(cursor));
    (Publisher(shared.clone()), Subscriber(shared))
}
