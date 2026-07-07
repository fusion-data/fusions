use log::error;
use tokio::task::JoinError;

#[inline]
pub fn handle_join_error<T, E>(ret: Result<Result<T, E>, JoinError>, task_name: &str)
where
  E: core::fmt::Display,
{
  match ret {
    Ok(ret) => {
      if let Err(err) = ret {
        error!("Asynchronous task '{}' error: {}", task_name, err);
      }
    }
    Err(err) => error!("Asynchronous task '{}' Join error: {}", task_name, err),
  }
}

pub async fn wait_exit_signals() {
  // 同时监听 ctrl_c 和 kill 信号（SIGTERM）
  #[cfg(unix)]
  {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
      .expect("Failed to get SIGTERM signal handle");
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::select! {
      _ = ctrl_c => {
        log::info!("Received Ctrl+C signal, preparing to shutdown...");
      }
      _ = sigterm.recv() => {
        log::info!("Received kill(SIGTERM) signal, preparing to shutdown...");
      }
    }
  }
  #[cfg(not(unix))]
  {
    let ctrl_c = tokio::signal::ctrl_c();
    ctrl_c.await.expect("Failed to install Ctrl-C signal handler");
  }
}

/// 获取当前请求的 trace id。
///
/// 真实现位于 [`crate::tracing::get_trace_id`]（基于 OpenTelemetry 上下文），
/// 仅在启用 `with-tracing` feature 时可用。本函数是 feature 无关的统一入口，
/// 委托真实现；未启用 `with-tracing` 时无 OTel 上下文可读，返回 `None`。
#[inline]
pub fn get_trace_id() -> Option<String> {
  #[cfg(feature = "with-tracing")]
  {
    crate::tracing::get_trace_id()
  }
  #[cfg(not(feature = "with-tracing"))]
  {
    None
  }
}
