use crate::cursor::Cursor;
use crate::{Bus, consumer_barrier, sequence_barrier};
use std::ops::Deref;
use std::sync::Arc;

pub struct EventGuard<'a, T> {
    value: *const T,
    consumer: &'a Consumer<T>,
}

impl<'a, T> Deref for EventGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // Safety: value 始终指向有效的缓冲区元素，且只读访问
        unsafe { &*self.value }
    }
}

impl<'a, T> Drop for EventGuard<'a, T> {
    fn drop(&mut self) {
        // 当事件被消费完成，消费者游标前进 1
        let id = self.consumer.id;
        let consumers = self.consumer.bus.consumers.pin();
        consumers.get(&id).unwrap().fetch_add(1);
    }
}

#[derive(Debug)]
pub struct Consumer<T> {
    bus: Arc<Bus<T>>,
    sequence: sequence_barrier::Subscriber,
    ids: Arc<consumer_barrier::ConsumerIds>,
    id: u64,
}

unsafe impl<T: Send> Send for Consumer<T> {}

impl<T> Clone for Consumer<T> {
    fn clone(&self) -> Self {
        // 为新消费者分配一个新的 ID
        let id = self.ids.next_id();

        // 复制当前消费者的游标位置
        let consumers = self.bus.consumers.pin();
        let current = consumers.get(&self.id).unwrap().relaxed();
        consumers.insert(id, Cursor::new(current).into());

        Self {
            bus: Arc::clone(&self.bus),
            ids: Arc::clone(&self.ids),
            id,
            sequence: self.sequence.clone(),
        }
    }
}

impl<T: Send + Sync + 'static> Consumer<T> {
    pub(crate) fn new(
        bus: Arc<Bus<T>>,
        ids: Arc<consumer_barrier::ConsumerIds>,
        sequence: sequence_barrier::Subscriber,
    ) -> Self {
        let id = ids.next_id();
        bus.consumers.pin().insert(id, Cursor::new(-1).into());

        Self {
            bus,
            ids,
            id,
            sequence,
        }
    }

    pub async fn next(&mut self) -> Option<EventGuard<'_, T>> {
        let consumers = self.bus.consumers.pin_owned();
        let current_seq = consumers.get(&self.id).unwrap().relaxed();

        // 等待直到 Publisher 发布了新事件
        if self.sequence.wait_until(current_seq).await.is_err() {
            return None;
        }

        // 取出下一个事件
        let value = unsafe {
            (&*self.bus.buffer.get(current_seq + 1))
                .as_ref()
                .expect("Event must exist before publish")
        };

        Some(EventGuard {
            value,
            consumer: self,
        })
    }
}

impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        // 从全局消费者列表移除
        self.bus.consumers.pin().remove(&self.id);
    }
}
