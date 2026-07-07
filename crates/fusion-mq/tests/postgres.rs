//! Postgres provider 集成测。
//!
//! 默认跳过 —— 设置 `FUSION_MQ_TEST_URL=postgres://...` 后运行：
//!
//! ```bash
//! FUSION_MQ_TEST_URL=postgres://hylx:hylx_dev@localhost:55432/hylx_careos \
//!   cargo test -p fusion-mq --test postgres -- --nocapture
//! ```
//!
//! 每个 case 使用唯一表名（`mq_test_<uuid>`）避免并发污染，结束 DROP。
#![cfg(feature = "with-postgres")]

use fusion_mq::postgres::PostgresEventQueueProvider;
use fusion_mq::{EventConsumer, EventProducer, PublishEvent, RetryDecision};
use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use uuid::Uuid;

const ENV_VAR: &str = "FUSION_MQ_TEST_URL";

async fn connect_pool() -> Option<PgPool> {
  let url = std::env::var(ENV_VAR).ok()?;
  let pool = PgPoolOptions::new()
    .max_connections(5)
    .acquire_timeout(Duration::from_secs(10))
    .connect(&url)
    .await
    .expect("connect to test DB");
  Some(pool)
}

async fn setup_table(pool: &PgPool) -> String {
  // 唯一表名（防并发 / 不污染主表）
  let table = format!("mq_test_{}", Uuid::new_v4().simple());
  let ddl = format!(
    "CREATE TABLE {table} (
       id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
       event_type      VARCHAR(100) NOT NULL,
       source_service  VARCHAR(50) NOT NULL,
       target_service  VARCHAR(50) NOT NULL,
       payload         JSONB NOT NULL,
       status          VARCHAR(20) NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending', 'processing', 'completed', 'failed')),
       retry_count     INT NOT NULL DEFAULT 0,
       max_retries     INT NOT NULL DEFAULT 3,
       error_message   TEXT,
       created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
       updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
       processed_at    TIMESTAMPTZ
     )"
  );
  sqlx::query(&ddl).execute(pool).await.expect("create test table");
  table
}

async fn drop_table(pool: &PgPool, table: &str) {
  let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {table}")).execute(pool).await;
}

#[tokio::test]
async fn publish_then_claim_then_complete() {
  let Some(pool) = connect_pool().await else {
    eprintln!("SKIP: ${ENV_VAR} not set");
    return;
  };
  let table = setup_table(&pool).await;

  let provider = PostgresEventQueueProvider::from_pool(pool.clone(), table.clone()).expect("provider construct");

  // 发布 3 个事件
  for i in 0..3 {
    provider
      .publish(PublishEvent::new("test.event", "src-svc", "dst-svc", json!({ "n": i })))
      .await
      .expect("publish");
  }

  // claim 2 个
  let claimed = provider.claim_pending("dst-svc", 2).await.expect("claim");
  assert_eq!(claimed.len(), 2);
  for c in &claimed {
    assert_eq!(c.event_type, "test.event");
    assert_eq!(c.retry_count, 0);
    assert_eq!(c.max_retries, 3);
  }

  // 第二次 claim 1 个（剩余）
  let claimed2 = provider.claim_pending("dst-svc", 10).await.expect("claim2");
  assert_eq!(claimed2.len(), 1);

  // mark 第 1 个 completed
  provider.mark_processed(claimed[0].id).await.expect("mark processed");

  // 验证 DB 状态
  let row: (String,) = sqlx::query_as(&format!("SELECT status FROM {table} WHERE id = $1"))
    .bind(claimed[0].id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
  assert_eq!(row.0, "completed");

  drop_table(&pool, &table).await;
}

#[tokio::test]
async fn mark_failed_retry_vs_dead() {
  let Some(pool) = connect_pool().await else {
    eprintln!("SKIP: ${ENV_VAR} not set");
    return;
  };
  let table = setup_table(&pool).await;

  let provider = PostgresEventQueueProvider::from_pool(pool.clone(), table.clone()).unwrap();

  let id = provider.publish(PublishEvent::new("evt", "src", "dst", json!({}))).await.unwrap();
  let claimed = provider.claim_pending("dst", 1).await.unwrap();
  assert_eq!(claimed.len(), 1);

  // RetryDecision::Retry → 回 pending + retry_count +1
  provider.mark_failed(id, "boom", RetryDecision::Retry).await.unwrap();
  let row: (String, i32) = sqlx::query_as(&format!("SELECT status, retry_count FROM {table} WHERE id = $1"))
    .bind(id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
  assert_eq!(row.0, "pending");
  assert_eq!(row.1, 1);

  // 再 claim + RetryDecision::Dead → failed
  let claimed = provider.claim_pending("dst", 1).await.unwrap();
  assert_eq!(claimed.len(), 1);
  provider.mark_failed(id, "fatal", RetryDecision::Dead).await.unwrap();
  let row: (String, i32) = sqlx::query_as(&format!("SELECT status, retry_count FROM {table} WHERE id = $1"))
    .bind(id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
  assert_eq!(row.0, "failed");
  assert_eq!(row.1, 2);

  drop_table(&pool, &table).await;
}

#[tokio::test]
async fn concurrent_claim_does_not_duplicate() {
  let Some(pool) = connect_pool().await else {
    eprintln!("SKIP: ${ENV_VAR} not set");
    return;
  };
  let table = setup_table(&pool).await;

  let provider = PostgresEventQueueProvider::from_pool(pool.clone(), table.clone()).unwrap();
  // 发布 20 个事件
  for i in 0..20 {
    provider.publish(PublishEvent::new("evt", "src", "dst", json!({ "i": i }))).await.unwrap();
  }

  // 4 个 worker 并发 claim 各 10
  let mut handles = vec![];
  for _ in 0..4 {
    let p = provider.clone();
    handles.push(tokio::spawn(async move { p.claim_pending("dst", 10).await.unwrap() }));
  }
  let mut all_ids = vec![];
  for h in handles {
    let claimed = h.await.unwrap();
    all_ids.extend(claimed.into_iter().map(|c| c.id));
  }
  // 不重复 + 不丢失
  let unique: std::collections::HashSet<_> = all_ids.iter().copied().collect();
  assert_eq!(unique.len(), all_ids.len(), "duplicate claim detected");
  assert_eq!(all_ids.len(), 20, "missed events");

  drop_table(&pool, &table).await;
}

#[tokio::test]
async fn reap_zombie_resets_stuck_processing() {
  let Some(pool) = connect_pool().await else {
    eprintln!("SKIP: ${ENV_VAR} not set");
    return;
  };
  let table = setup_table(&pool).await;

  let provider = PostgresEventQueueProvider::from_pool(pool.clone(), table.clone()).unwrap();
  let id = provider.publish(PublishEvent::new("evt", "src", "dst", json!({}))).await.unwrap();
  provider.claim_pending("dst", 1).await.unwrap();

  // 手动把 updated_at 拉回 10 分钟前模拟卡死
  sqlx::query(&format!("UPDATE {table} SET updated_at = now() - INTERVAL '10 minutes' WHERE id = $1"))
    .bind(id.0)
    .execute(&pool)
    .await
    .unwrap();

  let reaped = provider.reap_zombie("dst", Duration::from_secs(300)).await.unwrap();
  assert_eq!(reaped, 1);

  let row: (String,) = sqlx::query_as(&format!("SELECT status FROM {table} WHERE id = $1"))
    .bind(id.0)
    .fetch_one(&pool)
    .await
    .unwrap();
  assert_eq!(row.0, "pending");

  drop_table(&pool, &table).await;
}

#[tokio::test]
async fn target_service_isolation() {
  let Some(pool) = connect_pool().await else {
    eprintln!("SKIP: ${ENV_VAR} not set");
    return;
  };
  let table = setup_table(&pool).await;

  let provider = PostgresEventQueueProvider::from_pool(pool.clone(), table.clone()).unwrap();
  provider.publish(PublishEvent::new("a", "src", "svc-a", json!({}))).await.unwrap();
  provider.publish(PublishEvent::new("b", "src", "svc-b", json!({}))).await.unwrap();

  let claimed_a = provider.claim_pending("svc-a", 10).await.unwrap();
  let claimed_b = provider.claim_pending("svc-b", 10).await.unwrap();
  assert_eq!(claimed_a.len(), 1);
  assert_eq!(claimed_b.len(), 1);
  assert_eq!(claimed_a[0].event_type, "a");
  assert_eq!(claimed_b[0].event_type, "b");

  drop_table(&pool, &table).await;
}
