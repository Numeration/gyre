use crate::cursor::Cursor;
use crate::{Bus, fence, sequence_barrier};
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

pub struct EventGuard<'a, T> {
    value: *const T,
    consumer: &'a Consumer<T>,
    _guard: fence::Guard<'a>,
}

unsafe impl<T: Sync> Sync for EventGuard<'_, T> {}

unsafe impl<T> Send for EventGuard<'_, T> {}

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
        if let Some(cursor) = consumers.get(&id) {
            cursor.fetch_add(1);
        }
    }
}

pub struct OwnedGuard<T> {
    id: u64,
    bus: Arc<Bus<T>>,
    _guard: fence::OwnedGuard,
}

impl<T> Drop for OwnedGuard<T> {
    fn drop(&mut self) {
        // 当事件被消费完成，消费者游标前进 1
        let id = self.id;
        let consumers = self.bus.consumers.pin();
        if let Some(cursor) = consumers.get(&id) {
            cursor.fetch_add(1);
        }
    }
}

pub struct OwnedEventGuard<T> {
    value: T,
    guard: OwnedGuard<T>,
}

impl<T> OwnedEventGuard<T> {
    pub fn take(self) -> (OwnedGuard<T>, T) {
        (self.guard, self.value)
    }
}

impl<T> Deref for OwnedEventGuard<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for OwnedEventGuard<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

#[derive(Debug)]
pub struct Consumer<T> {
    bus: Arc<Bus<T>>,
    sequence: sequence_barrier::Subscriber,
    id: u64,
    fence: Arc<fence::Fence>,
}

unsafe impl<T: Send> Send for Consumer<T> {}

impl<T> Clone for Consumer<T> {
    fn clone(&self) -> Self {
        // 为新消费者分配一个新的 ID
        let id = self.bus.ids.next_id();

        // 复制当前消费者的游标位置
        let consumers = self.bus.consumers.pin();
        let current = consumers.get(&self.id).unwrap().relaxed();
        consumers.insert(id, Cursor::new(current).into());

        Self {
            bus: Arc::clone(&self.bus),
            id,
            sequence: self.sequence.clone(),
            fence: Default::default(),
        }
    }
}

impl<T: Send + Sync + 'static> Consumer<T> {
    pub(crate) fn new(
        bus: Arc<Bus<T>>,
        sequence: sequence_barrier::Subscriber,
        init_cursor: i64,
    ) -> Self {
        let id = bus.ids.next_id();
        bus.consumers
            .pin()
            .insert(id, Cursor::new(init_cursor).into());
        let fence = Default::default();

        Self {
            bus,
            id,
            sequence,
            fence,
        }
    }

    async fn take_event(&self) -> Option<&T> {
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

        Some(value)
    }

    pub async fn next(&mut self) -> Option<EventGuard<'_, T>> {
        let _guard = self.fence.acquire().await;
        let value = self.take_event().await?;

        Some(EventGuard {
            value,
            consumer: self,
            _guard,
        })
    }
}

impl<T: Clone + Send + Sync + 'static> Consumer<T> {
    pub async fn next_owned(&mut self) -> Option<OwnedEventGuard<T>> {
        let _guard = self.fence.acquire_owned().await;
        let value = self.take_event().await?;

        Some(OwnedEventGuard {
            value: T::clone(&value),
            guard: OwnedGuard {
                id: self.id,
                bus: Arc::clone(&self.bus),
                _guard,
            },
        })
    }
}

impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        // 从全局消费者列表移除
        self.bus.consumers.pin().remove(&self.id);
    }
}
