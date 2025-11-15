# Gyre: A Lightweight, High-Performance Publish-Subscribe Library for Tokio

**English** | [简体中文](./README.cn.md)

[![Crates.io](https://img.shields.io/crates/v/gyre.svg)](https://crates.io/crates/gyre)
[![Docs.rs](https://docs.rs/gyre/badge.svg)](https://docs.rs/gyre)

**Gyre** is a high-performance, Disruptor-style event publish-subscribe library built on [Tokio](https://tokio.rs/). It is designed to provide a low-latency, high-throughput multicast message channel for the Rust asynchronous ecosystem.

## Core Concepts

Gyre's design is heavily inspired by the LMAX Disruptor and is built around several core principles:

*   **Minimalism**: The API is extremely simple. Create a `Publisher` and `Consumer` pair with `gyre::channel(n)`, and you're ready to publish and receive events. No complex configurations, no macro magic.
*   **Performance-First**: The core data structure is a pre-allocated ring buffer. All coordination is done through atomic operations, avoiding the overhead of traditional locks. Critical state is cache-padded to prevent false sharing.
*   **Multicast & Independent Consumption**: An event can be broadcast to any number of consumers. Each consumer tracks its own progress independently, so a slow consumer won't block faster ones.
*   **Automatic Backpressure**: When a publisher produces faster than the slowest consumer can keep up, `publish` calls will automatically and asynchronously wait until space becomes available in the buffer. This backpressure mechanism is built-in and requires no manual management.
*   **RAII-Driven Progress Management**: `consumer.next().await` returns an `EventGuard`. The consumer's cursor only advances when this guard is dropped. This guarantees that an event is considered "consumed" only after it has been fully processed, ensuring message handling integrity at a fundamental level.

## Features

*   **Lightweight**: The core API consists of just two main structs: `Publisher` and `Consumer`.
*   **Type-Safe**: Leverages Rust's type system to ensure correctness in event delivery.
*   **High-Performance**: Lock-free design, optimized for low-latency and high-throughput scenarios.
*   **Dynamic Subscription**: Consumers can dynamically join the event bus at runtime via `publisher.subscribe()` or `consumer.clone()`.
*   **Seamless Tokio Integration**: Built entirely on Tokio, Gyre integrates naturally into any Tokio-based project.

## Quick Start

Let's create a simple single-producer, dual-consumer system.

**1. Add Dependencies:**

Add `gyre` and `tokio` to your `Cargo.toml`.

```toml
[dependencies]
gyre = "1"
tokio = { version = "1", features = ["full"] }
```

**2. Example Code:**

```rust
use gyre::{channel, Publisher, Consumer};
use tokio::join;

#[tokio::main]
async fn main() {
    // 1. Create a channel with a capacity of 4.
    let (tx, mut rx1) = channel::<i32>(4);

    // 2. Clone the first consumer to create a second one.
    //    `rx1` and `rx2` will now receive the same stream of events.
    let mut rx2 = rx1.clone();

    // Consumer 1's task
    let consumer1 = tokio::spawn(async move {
        while let Some(event) = rx1.next().await {
            println!("Consumer 1 received: {}", *event);
            // When `event` (the EventGuard) is dropped, consumer 1's cursor automatically advances.
        }
    });

    // Consumer 2's task (simulating a slightly slower consumer)
    let consumer2 = tokio::spawn(async move {
        while let Some(event) = rx2.next().await {
            println!("Consumer 2 received: {}", *event);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    });

    // Producer task
    let producer = tokio::spawn(async move {
        for i in 0..3 {
            println!("Publishing: {}", i);
            tx.publish(i).await.unwrap();
        }
        // When `tx` is dropped here, the channel is closed.
        // Consumers will finish after processing all published events.
    });

    // Wait for all tasks to complete.
    let _ = join!(producer, consumer1, consumer2);
}
```

**Expected Output:** (The order of printed lines may vary due to asynchronous scheduling.)
```text
Publishing: 0
Consumer 1 received: 0
Consumer 2 received: 0
Publishing: 1
Consumer 1 received: 1
Consumer 2 received: 1
Publishing: 2
Consumer 1 received: 2
Consumer 2 received: 2
```

## Contributing

Contributions of any kind are welcome! Whether it's submitting issues, opening pull requests, or improving documentation, we appreciate your help.

## License

This project is distributed under either the MIT license or the Apache 2.0 license, at your option:

- [MIT License](LICENSE-MIT)
- [Apache License 2.0](LICENSE-APACHE)