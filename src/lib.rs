//! # gyre
//!
//! `gyre` 是一个基于 Tokio 异步运行时的高性能、Disruptor 风格的事件分发库。
//! 它提供了一个多生产者、多消费者的环形缓冲区通道，旨在实现低延迟和高吞吐量的事件通信。
//!
//! ## 核心特性
//!
//! * **Disruptor 风格：** 使用原子操作序列号（Sequence）和环形缓冲区（Ring Buffer）进行协调，避免传统锁的开销。
//! * **异步背压（Backpressure）：** 当环形缓冲区满时，发布者会自动异步等待，直到消费者释放出空间。
//! * **多播（Multicast）：** 允许任意数量的消费者独立地、并行地消费事件流。
//! * **动态订阅：** 消费者可以在任何时间点加入事件总线，并从加入的下一刻开始接收事件。
//! * **RAII 驱动的游标推进：** 消费者游标仅在事件被安全处理并丢弃 `EventGuard` 后自动推进，确保了事件处理的完整性和安全性。
//!
//! ## 如何使用
//!
//! 通过 `channel` 函数创建一个发布者 (`Publisher`) 和一个初始消费者 (`Consumer`)。
//!
//! ```rust
//! use gyre::{channel, Publisher, Consumer};
//! use tokio::join;
//!
//! #[tokio::main]
//! async fn main() {
//!     // 创建一个容量为 4 的通道
//!     let (tx, mut rx) = channel::<i32>(4);
//!
//!     // 消费者 1
//!     let consumer1 = tokio::spawn(async move {
//!         while let Some(s) = rx.next().await {
//!             println!("Consumer 1 received: {}", *s);
//!         }
//!     });
//!
//!     // 消费者 2 (动态订阅)
//!     let mut rx_sub = tx.subscribe().await;
//!     let consumer2 = tokio::spawn(async move {
//!         while let Some(s) = rx_sub.next().await {
//!             // 注意：Consumer 2 仅接收订阅之后的事件
//!             println!("Consumer 2 received: {}", *s);
//!         }
//!     });
//!
//!     // 发布者任务
//!     let publisher = tokio::spawn(async move {
//!         for i in 0..10 {
//!             // publish 会在缓冲区满时自动等待
//!             if let Err(_) = tx.publish(i).await {
//!                 // 如果没有消费者，事件可能会被丢弃
//!                 println!("Event {} published, but no active consumer.", i);
//!             }
//!             tokio::task::yield_now().await;
//!         }
//!     });
//!
//!     let _ = join!(publisher, consumer1, consumer2);
//! }
//! ```

mod bus;
mod consumer;
mod consumer_id;
mod cursor;
mod fence;
mod publisher;
mod ring_buffer;
mod sequence;
mod sequence_barrier;

pub use bus::*;
pub use consumer::*;
pub use publisher::*;
