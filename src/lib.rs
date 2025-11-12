//! # Gyre
//!
//! `Gyre` is a high-performance, Disruptor-style event dispatch library built on the Tokio
//! asynchronous runtime. It provides a multi-producer, multi-consumer (MPMC) ring buffer
//! channel designed for low-latency and high-throughput scenarios.
//!
//! ## Core Concepts
//!
//! `Gyre`'s design is heavily inspired by the [LMAX Disruptor](https://lmax-exchange.github.io/disruptor/)
//! and [disruptor-rs](https://github.com/nicholassm/disruptor-rs). Its core revolves around a
//! pre-allocated ring buffer and a series of sequence numbers/cursors.
//!
//! *   **Ring Buffer:** All events are stored in this fixed-size buffer. Data is overwritten
//!     rather than removed.
//! *   **Publisher:**
//!     *   Responsible for writing events into the ring buffer.
//!     *   Coordinates with other publishers to serialize access to the ring buffer,
//!       ensuring events are published in a strict sequence.
//!     *   Before committing a write, it checks the cursor of the slowest consumer to
//!       ensure it doesn't overwrite any unprocessed events, thus achieving **backpressure**.
//! *   **Consumer:**
//!     *   Each consumer independently reads events from the ring buffer.
//!     *   Each consumer maintains its own cursor to track which event it has processed up to.
//!     *   Consumers do not interfere with each other and can consume in parallel at different rates.
//! *   **EventGuard:**
//!     *   `consumer.next().await` returns an `EventGuard`, which is an RAII-style smart pointer.
//!     *   Holding an `EventGuard` signifies that you are currently processing the event.
//!     *   When the `EventGuard` is dropped, the corresponding consumer's cursor is automatically
//!       advanced, marking the event as successfully processed. This design ensures the integrity
//!       of event handling.
//!
//! ## Key Features
//!
//! *   **Disruptor-style:** Utilizes atomic sequence numbers and cursors for coordination,
//!     avoiding the overhead of traditional locks for high performance.
//! *   **Asynchronous Backpressure:** When the ring buffer is full (i.e., the publisher is a
//!     full buffer's length ahead of the slowest consumer), the publisher will wait asynchronously
//!     until consumers advance and free up space.
//! *   **Multicast:** An event can be broadcast to all current consumers. Each consumer receives
//!     a complete copy of the event stream.
//! *   **Independent Consumption Progress:** Consumers track their progress independently. A slow
//!     consumer does not block faster ones.
//! *   **Dynamic Subscription and Cloning:**
//!     *   `publisher.subscribe().await`: Creates a new consumer at any time, which will start
//!       receiving events published **after** the subscription operation completes.
//!     *   `consumer.clone()`: Clones an existing consumer. The new consumer starts at the
//!       **exact same consumption position** as the original and then proceeds independently.
//! *   **RAII-driven Progress Updates:** The consumer's cursor is only advanced automatically
//!     after the `EventGuard` is safely handled and dropped, leading to concise and less
//!     error-prone code.
//! *   **Cancellation Safe:** Core asynchronous operations like `publish`, `subscribe`,
//!     and `next` are cancellation safe, making them robust and easy to integrate with
//!     constructs like `tokio::select!` and timeouts.
//!
//! ## Usage
//!
//! Create a `Publisher` and an initial `Consumer` using the `channel` function.
//!
//! ```rust
//! use gyre::{channel, Publisher, Consumer};
//! use tokio::join;
//!
//! #[tokio::main]
//! async fn main() {
//!     // 1. Create a channel with a capacity of 4.
//!     let (tx, mut rx1) = channel::<i32>(4);
//!
//!     // 2. Clone the first consumer to create a second one.
//!     //    `rx1` and `rx2` will now receive the same stream of events.
//!     let mut rx2 = rx1.clone();
//!
//!     // Consumer 1's task
//!     let consumer1 = tokio::spawn(async move {
//!         while let Some(event) = rx1.next().await {
//!             println!("Consumer 1 received: {}", *event);
//!             // When `event` (the EventGuard) is dropped, consumer 1's cursor automatically advances.
//!         }
//!     });
//!
//!     // Consumer 2's task
//!     let consumer2 = tokio::spawn(async move {
//!         while let Some(event) = rx2.next().await {
//!             println!("Consumer 2 received: {}", *event);
//!             // Simulate a slow consumer.
//!             tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
//!         }
//!     });
//!
//!     // Producer task
//!     let producer = tokio::spawn(async move {
//!         for i in 0..3 {
//!             println!("Publishing: {}", i);
//!             tx.publish(i).await.unwrap();
//!         }
//!         // When `tx` is dropped here, the channel is closed.
//!         // Consumers will finish after processing all published events.
//!     });
//!
//!     // Wait for all tasks to complete.
//!     let _ = join!(producer, consumer1, consumer2);
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
