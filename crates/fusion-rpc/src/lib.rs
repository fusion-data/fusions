use std::net::SocketAddr;

use ::config::{File, FileFormat};
use config::DEFAULT_CONFIG_STR;
use connectrpc::Router as ConnectRouter;
use fusion_core::{application::ApplicationBuilder, async_trait, plugin::Plugin};

pub mod auth_middleware;
pub mod client;
pub mod config;
pub mod context_validation_middleware;
pub mod server;
pub mod utils;

pub use auth_middleware::{AuthConfig, AuthLayer, ClaimMapping, ClaimSource};
pub use client::{ConnectTransport, TransportConfig, build_connect_transport, build_connect_transport_with};
pub use context_validation_middleware::{ContextValidationConfig, ContextValidationLayer};

pub use connectrpc::{ConnectError, ConnectRpcService, ErrorCode, RequestContext, Response, ServiceResult};

#[derive(Debug)]
pub struct RpcStartInfo {
  pub local_addr: SocketAddr,
}

pub struct RpcSettings {
  pub conf: config::RpcConfig,
  pub connect_router: ConnectRouter,
}

/// 仅负责把 RPC 默认配置（`resources/default.toml`）注入 [`ApplicationBuilder`]。
///
/// 这是一个空壳 plugin：`build` 只追加配置源，不注册任何 component、
/// 不启动服务、不持有运行期状态。RPC server 的实际装配（路由合并、监听）
/// 由各 bin 的 `main.rs` 自行完成（见 [`server`] 模块）。
pub struct RpcPlugin;

#[async_trait]
impl Plugin for RpcPlugin {
  async fn build(&self, app: &mut ApplicationBuilder) {
    app.add_config_source(File::from_str(DEFAULT_CONFIG_STR, FileFormat::Toml));
  }
}
