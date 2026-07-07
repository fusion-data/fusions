use crate::{EventConsumerHandle, EventProducerHandle, MqConfig, MqProviderKind};
use async_trait::async_trait;
use fusion_core::{application::ApplicationBuilder, configuration::ConfigRegistry, plugin::Plugin};

/// `MessageQueuePlugin` 在 `Application` 启动时读取 `[fusion.mq]` 配置段：
///
/// - `enable = false` 时整体跳过（适合不需要 MQ 的 bin）
/// - `enable = true` 时按 `provider` 构造 provider 实例，
///   以 [`EventProducerHandle`] / [`EventConsumerHandle`] 注册到 Component 注册表
///
/// ## 业务侧用法
///
/// ```ignore
/// use fusion_mq::{MessageQueuePlugin, EventProducerHandle, PublishEvent};
///
/// // 1. 启动时注册：
/// Application::builder()
///   .add_plugin(MessageQueuePlugin::new())
///   .run().await?;
///
/// // 2. 业务处理器中取用：
/// let producer = Application::global().component::<EventProducerHandle>();
/// producer.publish(PublishEvent::new("sms.send", "hylx-identity", "hylx-infra", payload)).await?;
/// ```
pub struct MessageQueuePlugin;

impl MessageQueuePlugin {
  pub fn new() -> Self {
    Self
  }
}

impl Default for MessageQueuePlugin {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl Plugin for MessageQueuePlugin {
  async fn build(&self, app: &mut ApplicationBuilder) {
    let config: MqConfig = app
      .get_config_by_path("fusion.mq")
      .unwrap_or_else(|e| panic!("MessageQueuePlugin: load [fusion.mq] failed: {e}"));
    if !config.enable {
      log::info!("MessageQueuePlugin: fusion.mq.enable = false, skip");
      return;
    }

    match config.provider {
      MqProviderKind::Postgres => {
        #[cfg(feature = "with-postgres")]
        {
          let provider = crate::postgres::PostgresEventQueueProvider::connect(&config.postgres)
            .await
            .unwrap_or_else(|e| panic!("MessageQueuePlugin: postgres connect failed: {e}"));
          app.add_component(EventProducerHandle::new(provider.clone()));
          app.add_component(EventConsumerHandle::new(provider));
          log::info!("MessageQueuePlugin: postgres provider registered (table={})", config.postgres.table_name);
        }
        #[cfg(not(feature = "with-postgres"))]
        {
          panic!("MessageQueuePlugin: provider=postgres but feature `with-postgres` is disabled");
        }
      }
    }
  }

  fn name(&self) -> &str {
    "fusion-mq::MessageQueuePlugin"
  }
}
