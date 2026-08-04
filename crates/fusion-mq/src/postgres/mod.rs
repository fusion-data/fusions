//! Postgres `event_queue` provider —— `EventProducer` + `EventConsumer` 双实现。
//!
//! 持有独立 `sqlx::PgPool`，不通过 `fusion-db` 的 `ModelManager` —— 见 crate 顶层 doc。
//!
//! `event_queue` 表 schema 见 [`schema`] 模块文档；实际 DDL 由部署脚本灌入
//! `hylx_mq` 数据库。

use crate::{
  ClaimedEvent, EventConsumer, EventId, EventProducer, MqError, PublishEvent, RetryDecision, config::PostgresMqConfig,
};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use sqlx::Row;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use uuid::Uuid;

pub mod schema;

/// Postgres `event_queue` provider。`Clone` 廉价（内部 `PgPool` 是 `Arc<PgPoolInner>`）。
#[derive(Clone)]
pub struct PostgresEventQueueProvider {
  pool: PgPool,
  table_name: String,
}

impl PostgresEventQueueProvider {
  /// 按 [`PostgresMqConfig`] 连接 PG 并构造 provider。
  pub async fn connect(config: &PostgresMqConfig) -> Result<Self, MqError> {
    let url = config
      .url
      .as_deref()
      .ok_or_else(|| MqError::ConfigInvalid("fusion.mq.postgres.url is required".into()))?;
    let pool = PgPoolOptions::new()
      .max_connections(config.max_connections)
      .min_connections(config.min_connections)
      .acquire_timeout(config.acquire_timeout())
      .idle_timeout(Some(config.idle_timeout()))
      .connect(url)
      .await
      .map_err(|e| MqError::ConnectionFailed(e.to_string()))?;
    Self::from_pool(pool, &config.table_name)
  }

  /// 从已有 PgPool 构造（集成测复用现有 dev DB 用）。
  pub fn from_pool(pool: PgPool, table_name: impl Into<String>) -> Result<Self, MqError> {
    let table_name = table_name.into();
    validate_table_ident(&table_name)?;
    Ok(Self { pool, table_name })
  }

  pub fn pool(&self) -> &PgPool {
    &self.pool
  }

  pub fn table_name(&self) -> &str {
    &self.table_name
  }
}

/// 校验表名标识符（防 SQL 注入 —— 表名以字符串插值进 SQL）。
fn validate_table_ident(name: &str) -> Result<(), MqError> {
  if name.is_empty() {
    return Err(MqError::ConfigInvalid("table name must not be empty".into()));
  }
  if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
    return Err(MqError::ConfigInvalid(format!("invalid table name: {name} (only [a-zA-Z0-9_] allowed)")));
  }
  Ok(())
}

#[async_trait]
impl EventProducer for PostgresEventQueueProvider {
  async fn publish(&self, event: PublishEvent) -> Result<EventId, MqError> {
    let max_retries = event.max_retries.unwrap_or(3) as i32;
    let sql = format!(
      "INSERT INTO {} (event_type, source_service, target_service, payload, max_retries) \
       VALUES ($1, $2, $3, $4, $5) RETURNING id",
      self.table_name
    );
    let id: Uuid = sqlx::query_scalar(&sql)
      .bind(&event.event_type)
      .bind(&event.source_service)
      .bind(&event.target_service)
      .bind(&event.payload)
      .bind(max_retries)
      .fetch_one(&self.pool)
      .await
      .map_err(|e| MqError::PublishFailed(e.to_string()))?;
    Ok(EventId(id))
  }
}

#[async_trait]
impl EventConsumer for PostgresEventQueueProvider {
  async fn claim_pending(&self, target_service: &str, batch_size: u32) -> Result<Vec<ClaimedEvent>, MqError> {
    let mut tx = self.pool.begin().await.map_err(|e| MqError::ClaimFailed(e.to_string()))?;

    // 1. SELECT FOR UPDATE SKIP LOCKED —— 锁定批量待领事件
    // event_queue.status 落 SMALLINT（形态 C：pending=1/processing=2/completed=3/failed=4）。
    let select_sql = format!(
      "SELECT id, event_type, payload, retry_count, max_retries, created_at \
       FROM {} \
       WHERE target_service = $1 AND status = 1 AND retry_count < max_retries \
       ORDER BY created_at \
       FOR UPDATE SKIP LOCKED LIMIT $2",
      self.table_name
    );
    let rows = sqlx::query(&select_sql)
      .bind(target_service)
      .bind(batch_size as i64)
      .fetch_all(&mut *tx)
      .await
      .map_err(|e| MqError::ClaimFailed(e.to_string()))?;

    if rows.is_empty() {
      tx.commit().await.map_err(|e| MqError::ClaimFailed(e.to_string()))?;
      return Ok(vec![]);
    }

    // 2. UPDATE 同一事务内置 processing=2
    let ids: Vec<Uuid> = rows.iter().map(|r| r.get::<Uuid, _>("id")).collect();
    let update_sql =
      format!("UPDATE {} SET status = 2, updated_at = now() WHERE id = ANY($1)", self.table_name);
    sqlx::query(&update_sql)
      .bind(&ids)
      .execute(&mut *tx)
      .await
      .map_err(|e| MqError::ClaimFailed(e.to_string()))?;

    tx.commit().await.map_err(|e| MqError::ClaimFailed(e.to_string()))?;

    let claimed = rows
      .into_iter()
      .map(|r| ClaimedEvent {
        id: EventId(r.get("id")),
        event_type: r.get("event_type"),
        payload: r.get("payload"),
        retry_count: r.get::<i32, _>("retry_count") as u32,
        max_retries: r.get::<i32, _>("max_retries") as u32,
        created_at: r.get::<DateTime<Utc>, _>("created_at"),
      })
      .collect();
    Ok(claimed)
  }

  async fn mark_processed(&self, event_id: EventId) -> Result<(), MqError> {
    let sql = format!(
      "UPDATE {} SET status = 3, processed_at = now(), updated_at = now() WHERE id = $1",
      self.table_name
    );
    let res = sqlx::query(&sql)
      .bind(event_id.0)
      .execute(&self.pool)
      .await
      .map_err(|e| MqError::AckFailed(e.to_string()))?;
    if res.rows_affected() == 0 {
      return Err(MqError::NotFound(event_id.to_string()));
    }
    Ok(())
  }

  async fn mark_failed(&self, event_id: EventId, error: &str, decision: RetryDecision) -> Result<(), MqError> {
    // next_status 落 SMALLINT：pending=1 / failed=4。
    let next_status: i16 = match decision {
      RetryDecision::Retry => 1,
      RetryDecision::Dead => 4,
    };
    // 兜底：即使 caller 给 Retry，若本次 +1 后已耗尽重试次数，强制置 failed=4，
    // 防止失败事件无限回 pending 重投。仅在重试未耗尽时才用 caller 给的状态。
    let sql = format!(
      "UPDATE {} \
       SET status = CASE WHEN retry_count + 1 >= max_retries THEN 4 ELSE $2 END, \
           error_message = $3, retry_count = retry_count + 1, updated_at = now() \
       WHERE id = $1",
      self.table_name
    );
    let res = sqlx::query(&sql)
      .bind(event_id.0)
      .bind(next_status)
      .bind(error)
      .execute(&self.pool)
      .await
      .map_err(|e| MqError::AckFailed(e.to_string()))?;
    if res.rows_affected() == 0 {
      return Err(MqError::NotFound(event_id.to_string()));
    }
    Ok(())
  }

  async fn reap_zombie(&self, target_service: &str, stuck_after: Duration) -> Result<u64, MqError> {
    let interval_secs = stuck_after.as_secs() as i32;
    // 已耗尽重试的 zombie 直接判 failed=4，否则回 pending=1 等待重领。
    // 防止耗尽重试的事件被反复 reap → 重领 → 再卡死。
    let sql = format!(
      "UPDATE {} \
       SET status = CASE WHEN retry_count >= max_retries THEN 4 ELSE 1 END, \
           updated_at = now() \
       WHERE target_service = $1 \
         AND status = 2 \
         AND updated_at < now() - make_interval(secs => $2)",
      self.table_name
    );
    let res = sqlx::query(&sql)
      .bind(target_service)
      .bind(interval_secs)
      .execute(&self.pool)
      .await
      .map_err(|e| MqError::AckFailed(e.to_string()))?;
    Ok(res.rows_affected())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn table_ident_validation() {
    assert!(validate_table_ident("event_queue").is_ok());
    assert!(validate_table_ident("Test123_x").is_ok());
    assert!(validate_table_ident("").is_err());
    assert!(validate_table_ident("foo;DROP TABLE x").is_err());
    assert!(validate_table_ident("a b").is_err());
    assert!(validate_table_ident("\"x\"").is_err());
  }
}
