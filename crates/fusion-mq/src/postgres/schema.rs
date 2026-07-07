//! `event_queue` 表 schema 元数据（仅文档；实际 DDL 由部署脚本灌入 `hylx_mq`）。
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS event_queue (
//!   id              UUID PRIMARY KEY DEFAULT uuidv7(),
//!   event_type      VARCHAR(100) NOT NULL,
//!   source_service  VARCHAR(50) NOT NULL,
//!   target_service  VARCHAR(50) NOT NULL,
//!   payload         JSONB NOT NULL,
//!   status          VARCHAR(20) NOT NULL DEFAULT 'pending'
//!                   CHECK (status IN ('pending', 'processing', 'completed', 'failed')),
//!   retry_count     INT NOT NULL DEFAULT 0,
//!   max_retries     INT NOT NULL DEFAULT 3,
//!   error_message   TEXT,
//!   created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
//!   updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
//!   processed_at    TIMESTAMPTZ
//! );
//!
//! CREATE INDEX IF NOT EXISTS idx_event_queue_poll
//!   ON event_queue (target_service, status, created_at)
//!   WHERE status = 'pending';
//!
//! CREATE INDEX IF NOT EXISTS idx_event_queue_zombie
//!   ON event_queue (target_service, status, updated_at)
//!   WHERE status = 'processing';
//! ```
//!
//! ## 与历史 schema 的差异
//!
//! 老 `hylx_careos.event_queue`（contracts/schemas/schema.sql 2245-2262 行）
//! **缺 `updated_at` 列**，但 `hylx-careos` TaskEventConsumer 的 zombie reaper
//! SQL 仍引用 `updated_at` —— 历史 bug，新 schema 修补。

/// 默认表名。
pub const DEFAULT_TABLE_NAME: &str = "event_queue";
