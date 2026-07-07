use tokio::task::JoinHandle;

use crate::CoreError;

pub struct ServiceHandle<T = ()> {
  name: String,
  handle: JoinHandle<T>,
}

impl<T> ServiceHandle<T> {
  pub fn new(name: impl Into<String>, handle: JoinHandle<T>) -> Self {
    Self { name: name.into(), handle }
  }

  pub fn name(&self) -> &str {
    &self.name
  }

  /// Abort the underlying task.
  ///
  /// 转调 [`JoinHandle::abort`]：在下一个 `.await` 点请求取消任务。
  /// 随后 [`Self::complete`] 会以 `Err((name, _))`（`JoinError::is_cancelled`）返回。
  pub fn abort(&self) {
    self.handle.abort();
  }

  /// Wait for the service to complete and return the result.
  ///
  /// # Returns
  ///
  /// * `Ok((name, res))` - The service name and result.
  /// * `Err((name, e))` - The service panicked or was cancelled.
  pub async fn complete(self) -> Result<(String, T), (String, CoreError)> {
    let name = self.name;
    match self.handle.await {
      Ok(r) => Ok((name, r)),
      Err(e) => Err((name, CoreError::from(e))),
    }
  }
}
