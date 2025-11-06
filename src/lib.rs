use crossbeam_utils::CachePadded;
use std::sync::Arc;

mod consumer;
mod consumer_barrier;
mod cursor;
mod fence;
mod publisher;
mod ring_buffer;
mod sequence;
mod sequence_barrier;

pub use crate::consumer::*;
pub use crate::publisher::*;

use crate::cursor::Cursor;
use crate::ring_buffer::RingBuffer;

/// 核心总线结构，封装共享的环形缓冲区与消费者状态
#[derive(Debug)]
struct Bus<T> {
    capacity: usize,
    ids: consumer_barrier::ConsumerIds,
    buffer: tokio::sync::OnceCell<RingBuffer<T>>,
    consumers: papaya::HashMap<u64, CachePadded<Cursor>>,
}

impl<T> Bus<T> {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            ids: consumer_barrier::ConsumerIds::default(),
            buffer: Default::default(),
            consumers: Default::default(),
        }
    }

    #[inline]
    async fn get_buffer(&self) -> &RingBuffer<T> {
        self.buffer.get_or_init(|| async {
            RingBuffer::new(self.capacity)
        }).await
    }
}

/// 创建一个发布者/消费者通道
pub fn channel<E: Send + Sync + 'static>(capacity: usize) -> (Publisher<E>, Consumer<E>) {
    let bus = Arc::new(Bus::new(capacity));

    // 创建 sequence barrier（控制事件发布与订阅同步）
    let (tx, rx) = sequence_barrier::channel(Cursor::new(-1));

    (
        Publisher::new(Arc::clone(&bus), tx),
        Consumer::new(bus, rx, -1),
    )
}
