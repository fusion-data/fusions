use axum::Router;
use connectrpc::Router as ConnectRouter;
use log::info;

use crate::config::RpcConfig;

/// 将 ConnectRPC 路由挂载到 Axum Router
///
/// Connect-rust 原生集成 Axum，通过 `into_axum_service()`
/// 将 Connect 服务作为 Axum fallback service 挂载，
/// 三协议（Connect + gRPC + gRPC-Web）共用同一 handler。
pub fn mount_rpc_services(axum_router: Router, connect_router: ConnectRouter, conf: &RpcConfig) -> Router {
  if !conf.enable {
    return axum_router;
  }
  info!("ConnectRPC services mounted at prefix: {}", conf.path_prefix);
  axum_router.fallback_service(connect_router.into_axum_service())
}
