//! fusion-mq: 通用消息队列抽象（producer/consumer trait + Postgres provider）。
//!
//! ## 设计目标
//!
//! - 业务侧仅依赖 [`EventProducer`] / [`EventConsumer`] trait，与具体 broker 解耦
//! - [`MessageQueuePlugin`] 在 `Application` 启动时读取 `[fusion.mq]` 配置，
//!   构造 provider 并以 [`EventProducerHandle`] / [`EventConsumerHandle`]
//!   newtype 注册到 Component 注册表，方便业务侧 `app.component::<...>()`
//! - 当前内置 Postgres provider（轮询 `event_queue` 表 + `FOR UPDATE SKIP LOCKED`）
//!   未来可扩展 Kafka / Redis 等
//!
//! ## 与 `fusion-db` 的关系
//!
//! `fusion-mq` 不依赖 [`fusion_db`](https://docs.rs/fusion-db) —— Postgres provider
//! 内部独立持 `sqlx::PgPool`，不通过 `ModelManager`。原因：
//! 1. `event_queue` 表本身无 tenant_id / RLS，不需要 `SET LOCAL` GUC 链路
//! 2. 解耦后未来可指向独立的 `hylx_mq` 数据库实例（拆库阶段 1 目标）
//! 3. trait 抽象层允许未来切换非-SQL provider 而无需触碰 fusion-db

pub mod config;
pub mod consumer;
pub mod error;
pub mod plugin;
pub mod producer;
pub mod types;

#[cfg(feature = "with-postgres")]
pub mod postgres;

pub use config::{MqConfig, MqProviderKind, PostgresMqConfig};
pub use consumer::{EventConsumer, EventConsumerHandle, RetryDecision};
pub use error::MqError;
pub use plugin::MessageQueuePlugin;
pub use producer::{EventProducer, EventProducerHandle};
pub use types::{ClaimedEvent, EventId, EventStatus, PublishEvent};
