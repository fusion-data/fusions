use std::net::SocketAddr;

use axum::Router;
use config::{File, FileFormat};
use log::info;

use fusion_core::{application::Application, configuration::ConfigRegistry};
use mea::shutdown::ShutdownRecv;

use crate::config::{DEFAULT_CONFIG_STR, WebConfig};
use crate::error::WebError;

pub struct WebServerBuilder {
  router: Router,
  shutdown_rx: Option<ShutdownRecv>,
}

impl WebServerBuilder {
  pub fn new(router: Router) -> Self {
    Self { router, shutdown_rx: None }
  }

  pub fn with_shutdown(mut self, shutdown_rx: ShutdownRecv) -> Self {
    self.shutdown_rx = Some(shutdown_rx);
    self
  }

  /// Bind the configured listener and run the serve loop, **blocking until
  /// shutdown** (graceful when [`Self::with_shutdown`] was supplied). This is
  /// not a builder-style "build and return" — the future only resolves when
  /// the server exits.
  ///
  /// Requires an initialized global [`Application`] (i.e. call after
  /// `Application::builder()...run()`); panics otherwise.
  pub async fn serve(self) -> Result<(), WebError> {
    let app = Application::global();
    app.config_registry().prepend_config_source(File::from_str(DEFAULT_CONFIG_STR, FileFormat::Toml))?;
    let conf: WebConfig = app.get_config()?;
    self.init_server_with_config(conf).await
  }

  #[deprecated(since = "0.2.0", note = "renamed to `serve` — this method runs the server loop, it does not build")]
  pub async fn build(self) -> Result<(), WebError> {
    self.serve().await
  }

  async fn init_server_with_config(self, conf: WebConfig) -> Result<(), WebError> {
    // HTTP/2 (h2c) 自动支持：`axum::serve()` 内部使用 `hyper_util::server::conn::auto::Builder`，
    // 在 axum 的 `http1` + `http2` feature 同时启用时（见 backend/Cargo.toml）对外同时承载
    // HTTP/1.1 与 HTTP/2 cleartext。ConnectRPC bidi-stream 要求 HTTP/2，由此处自动满足。
    // 规约：SDD service-dependency-contract §4.4
    let listener = tokio::net::TcpListener::bind(&conf.server_addr)
      .await
      .map_err(|e| WebError::server_error(format!("bind {} failed: {e}", conf.server_addr)))?;
    let local_addr = listener.local_addr()?;
    info!("The Web Server listening on {}", local_addr);
    let serve = if conf.enable_remote_addr {
      let s = axum::serve(listener, self.router.into_make_service_with_connect_info::<SocketAddr>());
      if let Some(shutdown_rx) = self.shutdown_rx {
        s.with_graceful_shutdown(shutdown_rx.is_shutdown_owned()).await
      } else {
        s.await
      }
    } else {
      let s = axum::serve(listener, self.router.into_make_service());
      if let Some(shutdown_rx) = self.shutdown_rx {
        s.with_graceful_shutdown(shutdown_rx.is_shutdown_owned()).await
      } else {
        s.await
      }
    };
    serve.map_err(WebError::from)
  }
}
