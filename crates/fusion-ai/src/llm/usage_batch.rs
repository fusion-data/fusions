//! 计量事件批量落库管道（通用件）—— [`super::metered`] 捕获缝的有界通道 + 批量写出实现。
//!
//! [`metered`](super::metered) 只定捕获缝（`AiUsageSink` trait + `NoopUsageSink`）；本模块补上
//! 「批量、有界重试、best-effort durability」的通用管道：**DB 写函数由消费方注入**
//! （`run_usage_writer` 的 `write_batch` 闭包），本模块不依赖任何数据库面。
//!
//! ## 数据流
//!
//! ```text
//! MeteredLlmProvider.record(ev)
//!   └── BatchSink::record → try_send（非阻塞）→ 有界 mpsc
//!         └── run_usage_writer：收一条 → 贪婪补满批 → write_with_retry
//!               ├── DB 写失败 → 有界重试 → log + drop（不重入队——持续失败的库不能让队列无界增长）
//!               ├── 批写 panic → catch_unwind 恢复（loop 存活，rx 不丢）
//!               └── 通道关闭且排空 → 退出（优雅关机依赖的不变量）
//! ```
//!
//! ## durability：best-effort，非零丢失
//!
//! 通道满 / 写重试耗尽 / 批写 panic 三类丢失点有进程内计数（[`UsageMetrics`]）；
//! 非优雅退出（进程被 kill，队列内容随进程消失）不可观测、无计数。零丢失需要
//! 事务化 outbox，不属本模块；MUST NOT 把本管道描述为可计费级精确。
//!
//! ## 消费方装配
//!
//! [`spawn_usage_batch_writer`] 一步产出 sink + writer 任务；关机协议 = **drop 全部
//! `AiUsageSink` clone 之后 await writer JoinHandle**（通道关闭即排空退出）。

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::FutureExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::metered::{AiUsageEvent, AiUsageSink};

/// 单批最大事件数（贪婪补满一批的上限）。
const MAX_BATCH: usize = 64;
/// 每批写出的重试上限（含首次）；耗尽 → log + drop。
const MAX_WRITE_ATTEMPTS: u32 = 3;
/// 写失败重试退避：首次 50ms，其后 200ms（与 hetu-ai 原文口径一致）。
const RETRY_BACKOFF_INITIAL: Duration = Duration::from_millis(50);
const RETRY_BACKOFF_SUBSEQUENT: Duration = Duration::from_millis(200);
/// 丢弃告警洪泛钳制：首条 + 其后每 1000 条 warn 一次（持续满通道不逐条刷日志）。
const DROP_WARN_INTERVAL: u64 = 1000;

/// best-effort durability 丢失点计数（进程内共享，producer 与 consumer 两侧共用）。
#[derive(Debug, Default)]
pub struct UsageMetrics {
  dropped: AtomicU64,
  write_failed: AtomicU64,
  worker_restart: AtomicU64,
}

impl UsageMetrics {
  /// 计数并返回丢弃累计值（调用方据此做告警节流）。
  pub fn inc_dropped(&self) -> u64 {
    self.dropped.fetch_add(1, Ordering::Relaxed) + 1
  }
  pub fn inc_write_failed(&self, n: u64) {
    self.write_failed.fetch_add(n, Ordering::Relaxed);
  }
  pub fn inc_worker_restart(&self) {
    self.worker_restart.fetch_add(1, Ordering::Relaxed);
  }
  pub fn dropped(&self) -> u64 {
    self.dropped.load(Ordering::Relaxed)
  }
  pub fn write_failed(&self) -> u64 {
    self.write_failed.load(Ordering::Relaxed)
  }
  pub fn worker_restart(&self) -> u64 {
    self.worker_restart.load(Ordering::Relaxed)
  }
}

/// 有界通道 sink：`record` = `try_send`，**绝不**阻塞调用方；通道满 / 已关 → drop + 计数。
pub struct BatchSink {
  tx: mpsc::Sender<AiUsageEvent>,
  metrics: Arc<UsageMetrics>,
}

impl BatchSink {
  pub fn new(tx: mpsc::Sender<AiUsageEvent>, metrics: Arc<UsageMetrics>) -> Self {
    Self { tx, metrics }
  }
}

impl AiUsageSink for BatchSink {
  fn record(&self, ev: AiUsageEvent) {
    if let Err(e) = self.tx.try_send(ev) {
      let dropped_total = self.metrics.inc_dropped();
      // 洪泛钳制：首条即告警（运维第一时间可见），其后每 1000 条一次。
      if dropped_total == 1 || dropped_total.is_multiple_of(DROP_WARN_INTERVAL) {
        let reason = match e {
          mpsc::error::TrySendError::Full(_) => "full",
          mpsc::error::TrySendError::Closed(_) => "closed",
        };
        tracing::warn!(
          metric = "ai_usage_dropped_total",
          reason,
          dropped_total,
          "ai usage event dropped (best-effort durability)"
        );
      }
    }
  }
}

/// 装配产物：sink（业务侧持有 / clone）+ metrics（可观测）+ writer 任务句柄。
///
/// 关机协议：drop 全部 sink clone（通道 sender 归零）→ writer 排空退出 → await `writer`。
///
/// `#[non_exhaustive]`：字段集（如未来补 graceful shutdown 句柄）对下游是公共面，
/// 加字段 MUST NOT 成为下游字面量构造破坏（消费方只做字段访问，不受影响）。
#[non_exhaustive]
pub struct UsageBatchPipeline {
  pub sink: Arc<dyn AiUsageSink>,
  pub metrics: Arc<UsageMetrics>,
  pub writer: JoinHandle<()>,
}

/// 一步装配：建通道 + sink + spawn writer（`tokio::spawn`）。
///
/// `write_batch` 收一批事件、返回写结果（`Err(String)` = 本批失败，进入重试）；
/// 闭包内自行持有 DB 句柄（clone 进闭包）。
pub fn spawn_usage_batch_writer<F, Fut>(channel_capacity: usize, write_batch: F) -> UsageBatchPipeline
where
  F: Fn(Vec<AiUsageEvent>) -> Fut + Send + Sync + 'static,
  Fut: Future<Output = Result<(), String>> + Send + 'static,
{
  let metrics = Arc::new(UsageMetrics::default());
  let (tx, rx) = mpsc::channel::<AiUsageEvent>(channel_capacity.max(1));
  let sink: Arc<dyn AiUsageSink> = Arc::new(BatchSink::new(tx, metrics.clone()));
  let writer = tokio::spawn(run_usage_writer(rx, metrics.clone(), write_batch));
  UsageBatchPipeline { sink, metrics, writer }
}

/// writer 主循环：阻塞收一条 → 贪婪补满一批 → 写。通道关闭且排空 → 退出。
///
/// `write_batch` 泛型注入，测试可替换为失败 / panic 注入。
pub async fn run_usage_writer<F, Fut>(mut rx: mpsc::Receiver<AiUsageEvent>, metrics: Arc<UsageMetrics>, write_batch: F)
where
  F: Fn(Vec<AiUsageEvent>) -> Fut + Sync,
  Fut: Future<Output = Result<(), String>>,
{
  loop {
    let Some(first) = rx.recv().await else {
      // 全部 sender 已 drop 且队列已空 → 排空完成。
      tracing::info!("[usage_batch] writer drained channel; exiting");
      break;
    };
    let mut batch = vec![first];
    while batch.len() < MAX_BATCH {
      match rx.try_recv() {
        Ok(ev) => batch.push(ev),
        Err(_) => break,
      }
    }
    write_with_retry(&write_batch, batch, &metrics).await;
  }
}

/// 批写 + 有界重试 + panic 恢复。成功 / 重试耗尽 drop / panic 恢复后返回——
/// 永不向上传播错误，loop 对下一批始终存活。
async fn write_with_retry<F, Fut>(write_batch: &F, batch: Vec<AiUsageEvent>, metrics: &UsageMetrics)
where
  F: Fn(Vec<AiUsageEvent>) -> Fut,
  Fut: Future<Output = Result<(), String>>,
{
  let mut attempt: u32 = 0;
  loop {
    attempt += 1;
    // catch_unwind：批写内的 panic MUST NOT 杀死 loop 或丢失 receiver。
    match AssertUnwindSafe(write_batch(batch.clone())).catch_unwind().await {
      Ok(Ok(())) => return,
      Ok(Err(e)) if attempt < MAX_WRITE_ATTEMPTS => {
        tracing::warn!(metric = "ai_usage_write_retry_total", attempt, error = %e, "ai usage batch write failed; retrying");
        tokio::time::sleep(backoff(attempt)).await;
      }
      Ok(Err(e)) => {
        // 重试耗尽 → log + drop。不重入队：持续失败的写目标会让队列无界增长。
        metrics.inc_write_failed(batch.len() as u64);
        tracing::error!(
          metric = "ai_usage_write_failed_total",
          error = %e,
          dropped = batch.len(),
          "ai usage batch write exhausted retries; dropping batch"
        );
        return;
      }
      Err(_panic) => {
        metrics.inc_worker_restart();
        tracing::error!(
          metric = "ai_usage_worker_restart_total",
          dropped = batch.len(),
          "ai usage batch write panicked; recovered (batch dropped, loop continues)"
        );
        return;
      }
    }
  }
}

/// 退避（第 1 次重试 [`RETRY_BACKOFF_INITIAL`]，其后 [`RETRY_BACKOFF_SUBSEQUENT`]）。
fn backoff(attempt: u32) -> Duration {
  match attempt {
    1 => RETRY_BACKOFF_INITIAL,
    _ => RETRY_BACKOFF_SUBSEQUENT,
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use chrono::Utc;
  use uuid::Uuid;

  use super::*;
  use crate::llm::TokenUsage;
  use crate::llm::metered::{AiUsageCtx, MatchedScope, Outcome};

  /// 测试事件（构造器面：`AiUsageEvent` 是 `#[non_exhaustive]`，新增模态字段不破坏本测试）。
  fn ev(feature: &str) -> AiUsageEvent {
    let ctx = AiUsageCtx {
      tenant_id: 0,
      dimensions: Default::default(),
      feature_code: feature.into(),
      matched_scope: MatchedScope::SystemDefault,
      provider: "deepseek".into(),
      model: "deepseek-flash".into(),
      credential_id: None,
      session_id: Some(Uuid::now_v7()),
      request_kind: "ai_chat".into(),
      resolved_region: None,
    };
    let usage = TokenUsage { prompt_tokens: 10, completion_tokens: 2, total_tokens: 12, cached_input_tokens: 0 };
    AiUsageEvent::from_ctx_tokens(&ctx, &usage, Outcome::Success, Utc::now(), None)
  }

  #[tokio::test]
  async fn sink_drops_and_counts_when_channel_is_full() {
    let metrics = Arc::new(UsageMetrics::default());
    // 容量 1 且无 consumer：第 2 条必满。
    let (tx, _rx) = mpsc::channel::<AiUsageEvent>(1);
    let sink = BatchSink::new(tx, metrics.clone());
    sink.record(ev("f1"));
    sink.record(ev("f2"));
    assert_eq!(metrics.dropped(), 1, "满通道的第 2 条 MUST 丢弃并计数");
  }

  #[tokio::test]
  async fn writer_batches_and_drains_on_channel_close() {
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();
    let pipeline = spawn_usage_batch_writer(16, move |batch| {
      let r = r.clone();
      async move {
        r.lock().unwrap().extend(batch.iter().map(|e| e.feature_code.clone()));
        Ok(())
      }
    });
    for i in 0..5 {
      pipeline.sink.record(ev(&format!("f{i}")));
    }
    // 关机协议：drop sink（最后一个 sender 归零）→ await writer（排空退出）。
    drop(pipeline.sink);
    pipeline.writer.await.expect("writer must exit cleanly");
    let got = received.lock().unwrap().clone();
    assert_eq!(got, vec!["f0", "f1", "f2", "f3", "f4"], "全部事件按序落 write_batch");
    assert_eq!(pipeline.metrics.dropped(), 0);
    assert_eq!(pipeline.metrics.write_failed(), 0);
  }

  #[tokio::test]
  async fn exhausted_retries_drop_the_batch_and_count() {
    let attempts = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let pipeline = spawn_usage_batch_writer(16, move |_batch| {
      let n = attempts.clone();
      async move {
        n.fetch_add(1, Ordering::Relaxed);
        Err("db down".to_string())
      }
    });
    pipeline.sink.record(ev("f"));
    drop(pipeline.sink);
    pipeline.writer.await.expect("writer must exit after exhaustion");
    // 1 条事件 × 3 次尝试后丢弃。
    assert_eq!(pipeline.metrics.write_failed(), 1);
  }

  #[tokio::test]
  async fn a_panicking_write_is_recovered_and_the_loop_continues() {
    let received: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();
    let pipeline = spawn_usage_batch_writer(16, move |batch| {
      let r = r.clone();
      async move {
        if batch.iter().any(|e| e.feature_code == "will-panic") {
          panic!("first batch write panics");
        }
        r.lock().unwrap().extend(batch.iter().map(|e| e.feature_code.clone()));
        Ok(())
      }
    });
    pipeline.sink.record(ev("will-panic"));
    // 给 writer 处理第一批的时间（panic 路径经 catch_unwind 后 return）。
    tokio::time::sleep(Duration::from_millis(50)).await;
    pipeline.sink.record(ev("after-panic"));
    drop(pipeline.sink);
    pipeline.writer.await.expect("writer must survive the panic");
    assert_eq!(pipeline.metrics.worker_restart(), 1, "panic 批计一次 worker_restart");
    assert_eq!(pipeline.metrics.dropped(), 0);
    let got = received.lock().unwrap().clone();
    assert_eq!(got, vec!["after-panic"], "panic 后的批 MUST 继续写出");
  }
}
