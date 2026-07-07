use crate::{ClaimedEvent, EventId, MqError};
use async_trait::async_trait;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

/// 处理失败后的重试决策。caller 根据 [`ClaimedEvent::retry_count`] 与
/// [`ClaimedEvent::max_retries`] 自行决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
  /// 失败后回 `pending`，下个 poll 周期再领取
  Retry,
  /// 重试次数耗尽，终局标记为 `failed`（事实 DLQ）
  Dead,
}

/// 事件消费抽象。
///
/// # 必须周期性调用 `reap_zombie`
///
/// `claim_pending` 把事件原子置 `processing` 后，若 worker 在 `mark_processed` /
/// `mark_failed` 之前 crash（进程被 kill / panic / OOM），该事件会**永久卡在
/// `processing`**，既不会被重新领取也不会进入终局状态。消费方 MUST 起一个后台
/// 任务按固定周期（建议远小于业务 SLA）调用 [`EventConsumer::reap_zombie`]，把
/// 卡死超时的事件回收；否则 crash 留下的事件会静默丢失。
#[async_trait]
pub trait EventConsumer: Send + Sync + 'static {
  /// 批量领取 `pending` 事件并原子置 `processing`。
  ///
  /// Postgres provider 走 `FOR UPDATE SKIP LOCKED` 保证多 consumer 不重复领取。
  async fn claim_pending(&self, target_service: &str, batch_size: u32) -> Result<Vec<ClaimedEvent>, MqError>;

  /// 处理成功，标记 `completed` + 写 `processed_at`。
  async fn mark_processed(&self, event_id: EventId) -> Result<(), MqError>;

  /// 处理失败：根据 `decision` 回 `pending` 或终止 `failed`。
  ///
  /// `retry_count` 由 provider 内部 `+1`，caller 不需要传。
  async fn mark_failed(&self, event_id: EventId, error: &str, decision: RetryDecision) -> Result<(), MqError>;

  /// Zombie 回收：把卡在 `processing` 超过 `stuck_after` 的事件 reset 回 `pending`。
  /// 返回 reset 行数。
  async fn reap_zombie(&self, target_service: &str, stuck_after: Duration) -> Result<u64, MqError>;
}

/// Component 注册用 newtype（同 [`crate::EventProducerHandle`]）。
#[derive(Clone)]
pub struct EventConsumerHandle(Arc<dyn EventConsumer>);

impl EventConsumerHandle {
  pub fn new<C>(consumer: C) -> Self
  where
    C: EventConsumer,
  {
    Self(Arc::new(consumer))
  }

  pub fn from_arc(consumer: Arc<dyn EventConsumer>) -> Self {
    Self(consumer)
  }

  pub fn into_inner(self) -> Arc<dyn EventConsumer> {
    self.0
  }
}

impl Deref for EventConsumerHandle {
  type Target = dyn EventConsumer;

  fn deref(&self) -> &Self::Target {
    &*self.0
  }
}
