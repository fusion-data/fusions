//! 东西向 ConnectRPC transport 工厂（HTTP/2 cleartext + 自愈）。
//!
//! 在 `connectrpc` 内置 `Reconnect` 状态机之上，用**自定义 [`HttpConnector`]**
//! 注入三项内核层韧性，覆盖优雅重启之外的"半开连接"故障：
//!
//! 1. **TCP keepalive**（`TCP_KEEPIDLE`/`INTVL`/`CNT`）—— 探测**空闲**半开连接。
//! 2. **`TCP_USER_TIMEOUT`**（仅 linux/android/fuchsia）—— 给**在途未确认写数据**
//!    设硬上界（~30s），覆盖 keepalive 探不到的"分区时正有请求在途"场景，
//!    避免连接卡在 TCP 重传（Linux `tcp_retries2` ≈ 15 次 ~924s）。
//! 3. **connect-timeout** —— bound 重连 dial，避免黑洞地址卡 OS SYN 重传 ~127s。
//!
//! 无论空闲还是在途，半开都在 ~30s 内被探到 → `connectrpc` 既有 `Reconnect`
//! 自动接管重连；重连 dial 受 connect-timeout（5s）约束。`Reconnect` 内部
//! `lazy=true`，连接掉线后下一次 `poll_ready` 自动重 dial + 重握手，
//! `.shared()` 的 `tower::buffer::Buffer` 永不被毒化、worker 永不退出。
//!
//! 为什么走 TCP 层而非 h2 PING：`connectrpc` 0.6.1 公开 API 中"自定义 connector"
//! 与"自定义 h2 builder"互斥，connect-timeout / user_timeout 必须走 connector，
//! 故半开探测由 TCP 层承担。TCP keepalive/user_timeout 是内核层、不受应用 CPU
//! 停顿影响 → 不误杀健康稀疏长流（如 voice），并保活 NAT/LB 映射。
//! bin↔bin 直连内网 h2c（无中间 L7 代理），TCP 层探活足够。
//!
//! 默认值对齐 reqwest 0.13.4 验证过的 connector 默认（`client.rs:303-307,368`）。
//!
//! 规约：SDD service-dependency-contract §4.4。

use std::time::Duration;

use connectrpc::client::{Http2Connection, SharedHttp2Connection};
use http::Uri;
use hyper_util::client::legacy::connect::HttpConnector;

/// 东西向 ConnectRPC transport 别名（与消费方应用层 crate 历史别名保持一致）。
/// `*ServiceClient<ConnectTransport>` 是统一签名。
pub type ConnectTransport = SharedHttp2Connection;

/// 东西向 transport 自愈参数。
///
/// 默认值开箱即用（[`Default`]）；需按部署调优时直接构造并传给
/// [`build_connect_transport_with`]。
#[derive(Debug, Clone)]
pub struct TransportConfig {
  /// 新建连接 connect 超时；`None` = 不限（OS SYN ~127s）。bound 重连 dial。
  pub connect_timeout: Option<Duration>,
  /// TCP keepalive 空闲探测起始（`TCP_KEEPIDLE`）；`None` = 关闭。处理**空闲**半开。
  pub tcp_keepalive: Option<Duration>,
  /// TCP keepalive 探测间隔（`TCP_KEEPINTVL`）。
  pub tcp_keepalive_interval: Option<Duration>,
  /// TCP keepalive 探测次数（`TCP_KEEPCNT`）。
  pub tcp_keepalive_retries: Option<u32>,
  /// `TCP_USER_TIMEOUT`（仅 linux/android/fuchsia 生效）：未确认数据最长存活；
  /// 处理**在途写数据**半开。字段在所有平台保留（无害），仅 connector setter
  /// 调用按 target cfg 守卫。
  pub tcp_user_timeout: Option<Duration>,
  /// tower buffer 容量（multiplex 单连接并发上限）。
  pub buffer_bound: usize,
}

impl Default for TransportConfig {
  fn default() -> Self {
    // 对齐 reqwest 0.13.4 验证过的 connector 默认（client.rs:303-307,368）。
    Self {
      connect_timeout: Some(Duration::from_secs(5)),
      tcp_keepalive: Some(Duration::from_secs(15)),
      tcp_keepalive_interval: Some(Duration::from_secs(15)),
      tcp_keepalive_retries: Some(3),
      tcp_user_timeout: Some(Duration::from_secs(30)), // 半开（空闲 or 在途）硬上界 ~30s
      buffer_bound: 1024,
    }
  }
}

/// 按 [`TransportConfig`] 构造一个注入了 keepalive + user_timeout + connect-timeout
/// 的 [`HttpConnector`]（plaintext h2c）。
fn make_connector(cfg: &TransportConfig) -> HttpConnector {
  let mut c = HttpConnector::new();
  c.set_nodelay(true);
  c.set_connect_timeout(cfg.connect_timeout);
  c.set_keepalive(cfg.tcp_keepalive);
  c.set_keepalive_interval(cfg.tcp_keepalive_interval);
  c.set_keepalive_retries(cfg.tcp_keepalive_retries);
  // hyper-util 的 `set_tcp_user_timeout` 本身带
  // `#[cfg(any(target_os = "android"|"fuchsia"|"linux"))]`（http.rs:430），
  // 其它 target 不存在该方法，无条件调用会**编译失败** → MUST 同款 cfg 守卫。
  // 非这些 target 时 tcp_user_timeout 不应用（仅 keepalive + connect_timeout 生效）。
  #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
  c.set_tcp_user_timeout(cfg.tcp_user_timeout);
  c
}

/// 用默认自愈配置构造 transport。drop-in 替换消费方应用层历史的 `build_connect_transport`。
///
/// MUST 在 tokio runtime 上下文中调用（`.shared()` 内部 `tokio::spawn(worker)`）。
/// 所有 bin 入口都是 `#[tokio::main]` async fn，在 `builder.run().await` 之后
/// 构造客户端即可。
pub fn build_connect_transport(base_uri: Uri) -> ConnectTransport {
  build_connect_transport_with(base_uri, &TransportConfig::default())
}

/// 用显式 [`TransportConfig`] 构造 transport（部署/调用方按需调优）。
///
/// `lazy_with_connector` 内部 `Reconnect` 仍 `lazy=true`（不毒化、worker 不退出、
/// 懒首连全保留）；自定义 connector 注入 connect-timeout + TCP keepalive +
/// user_timeout；h2 握手沿用 connectrpc 默认 builder。
pub fn build_connect_transport_with(base_uri: Uri, cfg: &TransportConfig) -> ConnectTransport {
  Http2Connection::lazy_with_connector(make_connector(cfg), base_uri).shared(cfg.buffer_bound)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_config_aligns_with_reqwest_defaults() {
    let cfg = TransportConfig::default();
    assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(5)));
    assert_eq!(cfg.tcp_keepalive, Some(Duration::from_secs(15)));
    assert_eq!(cfg.tcp_keepalive_interval, Some(Duration::from_secs(15)));
    assert_eq!(cfg.tcp_keepalive_retries, Some(3));
    assert_eq!(cfg.tcp_user_timeout, Some(Duration::from_secs(30)));
    assert_eq!(cfg.buffer_bound, 1024);
  }

  #[test]
  fn config_is_clone_and_debug() {
    let cfg = TransportConfig::default();
    let cloned = cfg.clone();
    assert_eq!(format!("{cfg:?}"), format!("{cloned:?}"));
  }

  /// 各 `Option` 字段取 `None` 边界时 connector 构造不 panic。
  #[test]
  fn make_connector_handles_all_none() {
    let cfg = TransportConfig {
      connect_timeout: None,
      tcp_keepalive: None,
      tcp_keepalive_interval: None,
      tcp_keepalive_retries: None,
      tcp_user_timeout: None,
      buffer_bound: 1,
    };
    // 不 panic 即通过（HttpConnector 无公开 getter，构造成功即验证 setter 路径）。
    let _ = make_connector(&cfg);
  }

  /// 默认配置在 tokio runtime 下构造 transport 不 panic，且句柄可 clone。
  #[tokio::test]
  async fn build_default_transport_is_clonable() {
    let uri: Uri = "http://127.0.0.1:9".parse().unwrap();
    let transport = build_connect_transport(uri);
    // SharedHttp2Connection 是廉价可 clone 句柄（lazy，不实际建连）。
    let _clone = transport.clone();
  }

  /// 自定义配置（含 None 边界）在 tokio runtime 下同样可构造。
  #[tokio::test]
  async fn build_custom_transport_with_none_fields() {
    let uri: Uri = "http://127.0.0.1:9".parse().unwrap();
    let cfg = TransportConfig {
      connect_timeout: None,
      tcp_keepalive: None,
      tcp_keepalive_interval: None,
      tcp_keepalive_retries: None,
      tcp_user_timeout: None,
      buffer_bound: 8,
    };
    let transport = build_connect_transport_with(uri, &cfg);
    let _clone = transport.clone();
  }
}
