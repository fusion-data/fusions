use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use fusion_core::configuration::Configurable;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RpcConfig {
  pub enable: bool,

  /// Connect 服务挂载路径前缀（默认 "/"）
  #[serde(default = "default_path_prefix")]
  pub path_prefix: String,

  pub clients: HashMap<String, RpcClientConfig>,
}

impl Configurable for RpcConfig {
  fn config_prefix() -> &'static str {
    "fusion.rpc"
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcClientConfig {
  pub addr: String,

  #[serde(default = "default_plaintext")]
  pub plaintext: bool,
}

fn default_plaintext() -> bool {
  true
}

fn default_path_prefix() -> String {
  "/".to_string()
}

pub const DEFAULT_CONFIG_STR: &str = include_str!("../resources/default.toml");
