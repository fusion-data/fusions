use std::{future::Future, time::Duration};

use crate::CoreError;

use super::{ServiceHandle, TaskResult};

#[derive(Debug)]
pub struct RetryStrategy {
  enable: bool,
  retry_limit: u32,
  interval: Duration,
  increase_rate: Option<f64>,
  /// 退避上限。指数退避后的间隔会被 clamp 到此值；`None` 表示不封顶。
  max_interval: Option<Duration>,
}

impl Default for RetryStrategy {
  fn default() -> Self {
    Self { enable: false, retry_limit: 5, interval: Duration::from_secs(30), increase_rate: None, max_interval: None }
  }
}

impl RetryStrategy {
  pub fn new_disable() -> Self {
    Self::default().with_disable()
  }

  pub fn new_enable() -> Self {
    Self::default().with_enable()
  }

  pub fn enable(&self) -> bool {
    self.enable
  }

  pub fn retry_limit(&self) -> u32 {
    self.retry_limit
  }

  pub fn interval(&self) -> Duration {
    self.interval
  }

  pub fn increase_rate(&self) -> Option<f64> {
    self.increase_rate
  }

  pub fn max_interval(&self) -> Option<Duration> {
    self.max_interval
  }

  pub fn with_enable(mut self) -> Self {
    self.enable = true;
    self
  }

  pub fn with_disable(mut self) -> Self {
    self.enable = false;
    self
  }

  pub fn with_retry_limit(mut self, retry_limit: u32) -> Self {
    self.retry_limit = retry_limit;
    self
  }

  pub fn with_interval(mut self, interval: Duration) -> Self {
    self.interval = interval;
    self
  }

  pub fn with_increase_rate(mut self, increase_rate: f64) -> Self {
    self.increase_rate = Some(increase_rate);
    self
  }

  pub fn with_max_interval(mut self, max_interval: Duration) -> Self {
    self.max_interval = Some(max_interval);
    self
  }
}

pub trait ServiceTask<T>
where
  T: Send + 'static,
{
  fn name(&self) -> &str {
    std::any::type_name_of_val(self)
  }

  fn retry_strategy(&self) -> RetryStrategy {
    RetryStrategy::default()
  }

  fn start(mut self) -> ServiceHandle<Result<TaskResult<T>, CoreError>>
  where
    Self: Send + Sized + 'static,
  {
    let name = self.name().to_string();
    let name2 = name.clone();
    let retry_strategy = self.retry_strategy();
    let handle = tokio::spawn(async move {
      let mut retry_count = 0;
      let mut duration = retry_strategy.interval();
      let retry_limit = retry_strategy.retry_limit();
      loop {
        match self.run_loop().await {
          Ok(result) => {
            log::info!("The ServiceTask: [{}] has been executed successfully after {} retries", name, retry_count);
            return Ok(TaskResult { result, retry_count });
          }
          Err(err) => {
            retry_count += 1;
            if retry_count > retry_limit {
              log::error!(
                "The ServiceTask: [{}] stop after {} retries has reached the retry limit: {}",
                name,
                retry_count,
                retry_limit
              );
              return Err(err);
            }
            if let Some(increase_rate) = retry_strategy.increase_rate() {
              let next_secs = duration.as_secs_f64() * increase_rate;
              // `from_secs_f64` 遇到非有限值（溢出 / NaN）会 panic：先校验，
              // 非有限则退化为 max_interval 或基础 interval。
              let next = if next_secs.is_finite() {
                Duration::from_secs_f64(next_secs)
              } else {
                retry_strategy.max_interval().unwrap_or_else(|| retry_strategy.interval())
              };
              // 指数退避封顶：避免 retry_limit 大时 sleep 膨胀失控。
              duration = match retry_strategy.max_interval() {
                Some(max) => next.min(max),
                None => next,
              };
            }
            tokio::time::sleep(duration).await;
          }
        }
      }
    });
    ServiceHandle::new(name2, handle)
  }

  fn run_loop(&mut self) -> impl Future<Output = Result<T, CoreError>> + Send;
}
