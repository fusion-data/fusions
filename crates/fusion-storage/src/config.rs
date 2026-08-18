//! storage 配置（后端超集平铺：调用方按后端取用）。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 对象存储配置段（字段为各消费方配置形态的并集平铺，全 Option 除 backend）。
///
/// 按后端取用：
/// - `fs`：`root` 必填——本 crate 不设默认 root，由消费方在配置装配期注入；
/// - `oss`：`endpoint` + `bucket` + AK/SK；`root` 为可选前缀（缺省 `/`）；
///   `presign_endpoint` 用于 CDN/反代场景（缺省 = `endpoint`）；
/// - `s3`：`region`（或自定义 `endpoint`）+ `bucket` + AK/SK；
/// - `obs`：`endpoint` + `bucket` + AK/SK。
///
/// 本类型不提供 `Default`：backend 与 root 均无隐式值，缺省语义归消费方装配层。
#[derive(Clone, Serialize, Deserialize)]
pub struct StorageConfig {
  /// 后端类型："fs" | "oss" | "s3" | "obs"。
  pub backend: String,
  /// fs 根目录 / 对象存储内前缀（oss/s3/obs 缺省 `/`）。
  #[serde(default)]
  pub root: Option<String>,
  /// 对象存储 endpoint（oss/obs 必填；s3 为可选自定义端点）。
  #[serde(default)]
  pub endpoint: Option<String>,
  /// bucket 名。
  #[serde(default)]
  pub bucket: Option<String>,
  /// s3 region（s3 分支用）。
  #[serde(default)]
  pub region: Option<String>,
  /// 对外暴露的预签名 endpoint（oss `presign_endpoint`，CDN/反代场景；缺省 = endpoint）。
  #[serde(default)]
  pub presign_endpoint: Option<String>,
  /// AccessKey ID。
  #[serde(default)]
  pub access_key: Option<String>,
  /// AccessKey Secret。
  #[serde(default)]
  pub secret_key: Option<String>,
}

impl StorageConfig {
  /// 按后端构造空配置（其余字段 None，由调用方填充）。
  pub fn new(backend: &str) -> Self {
    Self {
      backend: backend.to_owned(),
      root: None,
      endpoint: None,
      bucket: None,
      region: None,
      presign_endpoint: None,
      access_key: None,
      secret_key: None,
    }
  }
}

// 携密类型 MUST NOT derive Debug（framework-conventions §2）：AK/SK 打 <REDACTED>，
// 防 tracing::debug!(?config) 把明文密钥落盘。
impl fmt::Debug for StorageConfig {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("StorageConfig")
      .field("backend", &self.backend)
      .field("root", &self.root)
      .field("endpoint", &self.endpoint)
      .field("bucket", &self.bucket)
      .field("region", &self.region)
      .field("presign_endpoint", &self.presign_endpoint)
      .field("access_key", &"<REDACTED>")
      .field("secret_key", &"<REDACTED>")
      .finish()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 超集字段全部可从 TOML 反序列化（各后端字段并集）。
  #[test]
  fn deserializes_superset_fields() {
    let raw = r#"
      backend = "oss"
      root = "prefix"
      endpoint = "https://oss.example.aliyuncs.com"
      bucket = "b"
      region = "cn-east-1"
      presign_endpoint = "https://cdn.example.com"
      access_key = "ak"
      secret_key = "sk"
    "#;
    let c: StorageConfig = toml::from_str(raw).unwrap();
    assert_eq!(c.backend, "oss");
    assert_eq!(c.root.as_deref(), Some("prefix"));
    assert_eq!(c.bucket.as_deref(), Some("b"));
    assert_eq!(c.region.as_deref(), Some("cn-east-1"));
    assert_eq!(c.presign_endpoint.as_deref(), Some("https://cdn.example.com"));
    assert_eq!(c.access_key.as_deref(), Some("ak"));
    assert_eq!(c.secret_key.as_deref(), Some("sk"));
  }

  /// 除 backend 外全部可缺省（各后端最小配置形态）。
  #[test]
  fn optional_fields_default_to_none() {
    let c: StorageConfig = toml::from_str("backend = \"fs\"").unwrap();
    assert!(c.root.is_none() && c.endpoint.is_none() && c.bucket.is_none());
    assert!(c.region.is_none() && c.presign_endpoint.is_none());
    assert!(c.access_key.is_none() && c.secret_key.is_none());
  }

  /// 携密 Debug 脱敏：AK/SK 不出现明文，其余字段可见。
  #[test]
  fn debug_redacts_credentials() {
    let mut c = StorageConfig::new("oss");
    c.access_key = Some("ak-plain-secret".to_owned());
    c.secret_key = Some("sk-plain-secret".to_owned());
    c.bucket = Some("visible-bucket".to_owned());
    let d = format!("{c:?}");
    assert!(d.contains("visible-bucket"));
    assert!(d.contains("<REDACTED>"));
    assert!(!d.contains("ak-plain-secret"));
    assert!(!d.contains("sk-plain-secret"));
  }

  /// serde 往返：Serialize 面完整（配置回写场景）。
  #[test]
  fn serializes_round_trip() {
    let mut c = StorageConfig::new("s3");
    c.region = Some("us-east-1".to_owned());
    c.bucket = Some("b".to_owned());
    let round: StorageConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
    assert_eq!(round.backend, "s3");
    assert_eq!(round.region.as_deref(), Some("us-east-1"));
  }
}
