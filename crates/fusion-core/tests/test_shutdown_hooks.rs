use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use fusion_core::application::Application;

/// Hooks registered via `add_shutdown_hook` must run (in registration order)
/// when `Application::await_shutdown` completes; a failing hook must not
/// prevent the remaining hooks from running.
#[tokio::test]
async fn test_shutdown_hooks_run_on_await_shutdown() {
  let counter = Arc::new(AtomicUsize::new(0));
  let c1 = counter.clone();
  let c2 = counter.clone();

  Application::builder()
    .add_shutdown_hook(move |_app| {
      Box::new(async move {
        c1.fetch_add(1, Ordering::SeqCst);
        Ok("hook-1".to_string())
      })
    })
    .add_shutdown_hook(|_app| Box::new(async move { Err(fusion_core::CoreError::custom("hook-2 deliberately fails")) }))
    .add_shutdown_hook(move |_app| {
      Box::new(async move {
        c2.fetch_add(1, Ordering::SeqCst);
        Ok("hook-3".to_string())
      })
    })
    .run()
    .await
    .unwrap();

  assert_eq!(counter.load(Ordering::SeqCst), 0, "hooks must not run before shutdown");

  // Application 手写 Debug：下游 `#[derive(Debug)]` 包装可编译，输出摘要不递归 dump 组件
  let dbg = format!("{:?}", Application::global());
  assert!(dbg.contains("Application") && dbg.contains("start_time"), "unexpected Debug output: {dbg}");

  Application::shutdown().await;
  assert!(Application::await_shutdown().await);
  assert_eq!(counter.load(Ordering::SeqCst), 2, "both succeeding hooks must have run despite hook-2 failing");
}
