# Gyre: 一个为 Tokio 设计的轻量级、高性能的发布-订阅库

[English](./README.md) | **简体中文**

[![Crates.io](https://img.shields.io/crates/v/gyre.svg)](https://crates.io/crates/gyre)
[![Docs.rs](https://docs.rs/gyre/badge.svg)](https://docs.rs/gyre)

**Gyre** 是一个基于 [Tokio](https://tokio.rs/) 构建的、Disruptor 风格的高性能事件发布-订阅库。它旨在为 Rust 异步生态系统提供一个低延迟、高吞吐量的多播（multicast）消息通道。

## 核心理念

Gyre 的设计深受 LMAX Disruptor 的启发，并围绕几个核心原则构建：

*   **极简主义**: API 极其简单。通过 `gyre::channel(n)` 创建一对 `Publisher` 和 `Consumer`，然后就可以开始发布和接收事件。没有复杂的配置，没有宏魔法。
*   **性能至上**: 核心数据结构是一个预分配的环形缓冲区（Ring Buffer），所有协调都通过原子操作（Atomics）完成，避免了传统锁的开销。关键状态使用了缓存行填充（Cache Padding）来防止伪共享（False Sharing）。
*   **多播与独立消费**: 一个事件可以被广播给任意数量的消费者。每个消费者都独立地跟踪自己的消费进度，一个慢的消费者不会阻塞其他快的消费者。
*   **自动背压**: 当发布者生产速度超过最慢的消费者时，`publish` 调用会自动异步等待，直到缓冲区释放出空间。这种背压机制是内置的，无需手动管理。
*   **RAII 驱动的进度管理**: `consumer.next().await` 返回一个 `EventGuard`。当这个守卫被丢弃时，消费者的游标才会自动前进。这确保了事件只有在被完全处理后才被视为“已消费”，从根本上保证了消息处理的完整性。

## 特性

*   **轻量级**: 核心 API 仅包含 `Publisher` 和 `Consumer` 两个主要结构体。
*   **类型安全**: 利用 Rust 的类型系统确保事件传递的正确性。
*   **高性能**: 无锁设计，专为低延迟和高吞吐量场景优化。
*   **动态订阅**: 消费者可以在运行时通过 `publisher.subscribe()` 或 `consumer.clone()` 动态加入事件总线。
*   **与 Tokio 生态无缝集成**: Gyre 完全基于 Tokio 构建，可以自然地融入任何 Tokio 项目中。

## 快速上手

让我们创建一个简单的单生产者、双消费者的系统。

**1. 添加依赖:**

在你的 `Cargo.toml` 中添加 `gyre` 和 `tokio`。

```toml
[dependencies]
gyre = "1"
tokio = { version = "1", features = ["full"] }
```

**2. 示例代码:**

```rust
use gyre::{channel, Publisher, Consumer};
use tokio::join;

#[tokio::main]
async fn main() {
    // 1. 创建一个容量为 4 的通道。
    let (tx, mut rx1) = channel::<i32>(4);

    // 2. 克隆第一个消费者，创建第二个消费者。
    //    `rx1` 和 `rx2` 将会接收到所有相同的事件。
    let mut rx2 = rx1.clone();

    // 消费者 1 的任务
    let consumer1 = tokio::spawn(async move {
        while let Some(event) = rx1.next().await {
            println!("Consumer 1 received: {}", *event);
            // 当 `event` (EventGuard) 被丢弃时，消费者 1 的游标会自动前进
        }
    });

    // 消费者 2 的任务 (模拟一个稍慢的消费者)
    let consumer2 = tokio::spawn(async move {
        while let Some(event) = rx2.next().await {
            println!("Consumer 2 received: {}", *event);
            tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        }
    });

    // 发布者任务
    let producer = tokio::spawn(async move {
        for i in 0..3 {
            println!("Publishing: {}", i);
            tx.publish(i).await.unwrap();
        }
        // 当 `tx` 在这里被丢弃时，通道会关闭。
        // 消费者将在处理完所有已发布的事件后结束。
    });

    // 等待所有任务完成
    let _ = join!(producer, consumer1, consumer2);
}
```

**预期输出:** (打印顺序可能因异步调度而异)
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

## 贡献

欢迎任何形式的贡献！无论是提交 issue、发起 Pull Request 还是改进文档，我们都非常欢迎。

## 许可证

本项目采用 [Apache 2.0 许可证](./LICENSE)。