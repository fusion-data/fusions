use crate::{EventId, MqError, PublishEvent};
use async_trait::async_trait;
use std::ops::Deref;
use std::sync::Arc;

/// 事件发布抽象。
#[async_trait]
pub trait EventProducer: Send + Sync + 'static {
  /// 发布事件，返回事件 ID。
  async fn publish(&self, event: PublishEvent) -> Result<EventId, MqError>;
}

/// Component 注册用 newtype —— 把 `Arc<dyn EventProducer>` 收口成一个具体类型，
/// 让 `Application::add_component / component<T>` 的 `std::any::type_name::<T>()`
/// 索引干净（避免裸 `dyn Trait + Send + Sync` 的全路径作为 key）。
///
/// 业务侧：
/// ```ignore
/// let producer = Application::global().component::<EventProducerHandle>();
/// producer.publish(event).await?;
/// ```
#[derive(Clone)]
pub struct EventProducerHandle(Arc<dyn EventProducer>);

impl EventProducerHandle {
  pub fn new<P>(producer: P) -> Self
  where
    P: EventProducer,
  {
    Self(Arc::new(producer))
  }

  pub fn from_arc(producer: Arc<dyn EventProducer>) -> Self {
    Self(producer)
  }

  pub fn into_inner(self) -> Arc<dyn EventProducer> {
    self.0
  }
}

impl Deref for EventProducerHandle {
  type Target = dyn EventProducer;

  fn deref(&self) -> &Self::Target {
    &*self.0
  }
}
