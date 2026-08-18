//! opendal Operator 工厂（fs / oss / s3 / obs，feature 透传）。

use opendal::Operator;
use opendal::layers::LoggingLayer;

use crate::config::StorageConfig;

/// 从配置构造 opendal `Operator`（统一挂 `LoggingLayer`）。
///
/// 后端分支的编译面由 cargo feature 决定；错误三分级（MUST NOT 混淆，防排障误导）：
/// 1. 后端已知但 feature 未启用 → 指向 enable fusion-storage feature；
/// 2. 后端未知 → Unsupported storage backend；
/// 3. 分支内构造失败 → 各后端的具体失败原因。
///
/// fs 后端 `root` 必填：本 crate 不设默认 root（消费方各有取值，装配期注入）；
/// 云后端 `root` 缺省 `/`（对象存储自然命名空间根）。
///
/// # Errors
///
/// 后端不支持 / feature 未启用 / root 缺失（fs）/ Operator 构造失败时返回错误描述。
pub fn build_operator(config: &StorageConfig) -> Result<Operator, String> {
  let op = match config.backend.as_str() {
    "fs" => build_fs(config)?,
    "oss" => build_oss(config)?,
    "s3" => build_s3(config)?,
    "obs" => build_obs(config)?,
    other => return Err(format!("Unsupported storage backend: '{other}'. Use fs, oss, s3, or obs.")),
  };

  Ok(op.layer(LoggingLayer::default()))
}

/// feature 未启用的后端分支统一错误（区别于「后端不支持」）。
/// 仅在存在未启用后端（即任一 not-branch 桩参与编译）时被引用。
#[cfg(not(all(feature = "fs", feature = "oss", feature = "s3", feature = "obs")))]
fn backend_feature_disabled(name: &str) -> String {
  format!("storage backend '{name}' is not compiled in: enable the fusion-storage cargo feature '{name}'")
}

#[cfg(feature = "fs")]
fn build_fs(config: &StorageConfig) -> Result<Operator, String> {
  let root = config.root.as_deref().ok_or_else(|| {
    "fs backend requires 'root' to be set: fusion-storage sets no default; inject it during config assembly".to_owned()
  })?;
  std::fs::create_dir_all(root).map_err(|e| format!("Failed to create fs storage root {root}: {e}"))?;
  let builder = opendal::services::Fs::default().root(root);
  Ok(Operator::new(builder).map_err(|e| format!("Failed to create fs operator: {e}"))?.finish())
}

#[cfg(not(feature = "fs"))]
fn build_fs(_config: &StorageConfig) -> Result<Operator, String> {
  Err(backend_feature_disabled("fs"))
}

#[cfg(feature = "oss")]
fn build_oss(config: &StorageConfig) -> Result<Operator, String> {
  let mut builder = opendal::services::Oss::default()
    .root(config.root.as_deref().unwrap_or("/"))
    .bucket(config.bucket.as_deref().unwrap_or(""))
    .endpoint(config.endpoint.as_deref().unwrap_or(""))
    .access_key_id(config.access_key.as_deref().unwrap_or(""))
    .access_key_secret(config.secret_key.as_deref().unwrap_or(""));
  if let Some(pe) = config.presign_endpoint.as_deref() {
    builder = builder.presign_endpoint(pe);
  }
  Ok(Operator::new(builder).map_err(|e| format!("Failed to create OSS operator: {e}"))?.finish())
}

#[cfg(not(feature = "oss"))]
fn build_oss(_config: &StorageConfig) -> Result<Operator, String> {
  Err(backend_feature_disabled("oss"))
}

#[cfg(feature = "s3")]
fn build_s3(config: &StorageConfig) -> Result<Operator, String> {
  let mut builder = opendal::services::S3::default()
    .root(config.root.as_deref().unwrap_or("/"))
    .bucket(config.bucket.as_deref().unwrap_or(""))
    .region(config.region.as_deref().unwrap_or(""))
    .access_key_id(config.access_key.as_deref().unwrap_or(""))
    .secret_access_key(config.secret_key.as_deref().unwrap_or(""));
  if let Some(ep) = config.endpoint.as_deref() {
    builder = builder.endpoint(ep);
  }
  Ok(Operator::new(builder).map_err(|e| format!("Failed to create S3 operator: {e}"))?.finish())
}

#[cfg(not(feature = "s3"))]
fn build_s3(_config: &StorageConfig) -> Result<Operator, String> {
  Err(backend_feature_disabled("s3"))
}

#[cfg(feature = "obs")]
fn build_obs(config: &StorageConfig) -> Result<Operator, String> {
  let builder = opendal::services::Obs::default()
    .root(config.root.as_deref().unwrap_or("/"))
    .bucket(config.bucket.as_deref().unwrap_or(""))
    .endpoint(config.endpoint.as_deref().unwrap_or(""))
    .access_key_id(config.access_key.as_deref().unwrap_or(""))
    .secret_access_key(config.secret_key.as_deref().unwrap_or(""));
  Ok(Operator::new(builder).map_err(|e| format!("Failed to create OBS operator: {e}"))?.finish())
}

#[cfg(not(feature = "obs"))]
fn build_obs(_config: &StorageConfig) -> Result<Operator, String> {
  Err(backend_feature_disabled("obs"))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn temp_root(tag: &str) -> String {
    let dir = std::env::temp_dir().join(format!("fusion-storage-test-{tag}-{}", unique_suffix()));
    std::fs::create_dir_all(&dir).expect("create temp root");
    dir.to_str().expect("utf-8 path").to_owned()
  }

  /// 单测临时目录名的唯一后缀（纳秒时间戳 + 单调计数，不引入 uuid 依赖）。
  fn unique_suffix() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .expect("clock after epoch")
      .as_nanos();
    now + u128::from(SEQ.fetch_add(1, Ordering::Relaxed))
  }

  #[test]
  fn fs_operator_constructs_and_creates_root() {
    let root = temp_root("fs-ok");
    let mut c = StorageConfig::new("fs");
    c.root = Some(root.clone());
    let op = build_operator(&c).expect("fs operator");
    assert!(!op.info().full_capability().presign, "fs must not report native presign");
    assert!(std::path::Path::new(&root).is_dir(), "root directory is created");
    let _ = std::fs::remove_dir_all(&root);
  }

  #[test]
  fn fs_without_root_is_error_naming_injection_point() {
    let c = StorageConfig::new("fs");
    let err = build_operator(&c).unwrap_err();
    assert!(err.contains("root"), "error must point at the missing root: {err}");
    assert!(err.contains("no default"), "error must state the crate sets no default: {err}");
  }

  #[test]
  fn unknown_backend_is_unsupported() {
    let c = StorageConfig::new("gcs");
    let err = build_operator(&c).unwrap_err();
    assert!(err.starts_with("Unsupported storage backend: 'gcs'"), "unexpected: {err}");
  }

  /// feature-off 分支的错误指向 feature 而非「不支持」（错误分级门，D2 附带约束）。
  /// default feature 集仅含 fs，default 档由本测试覆盖 s3/obs 的 off 分支；
  /// --all-features 档本测试被 cfg 排除，on 分支由下方各后端构造测试接管。
  #[cfg(not(any(feature = "s3", feature = "obs")))]
  #[test]
  fn disabled_backend_error_points_to_feature_flag() {
    for backend in ["s3", "obs"] {
      let c = StorageConfig::new(backend);
      let err = build_operator(&c).unwrap_err();
      assert!(err.contains("fusion-storage cargo feature"), "backend {backend}: {err}");
      assert!(!err.contains("Unsupported"), "must not read as unsupported: {err}");
    }
  }

  #[cfg(feature = "oss")]
  #[test]
  fn oss_operator_constructs_with_dummy_credentials() {
    let mut c = StorageConfig::new("oss");
    c.endpoint = Some("https://oss-cn-hangzhou.aliyuncs.com".to_owned());
    c.bucket = Some("test-bucket".to_owned());
    c.access_key = Some("ak".to_owned());
    c.secret_key = Some("sk".to_owned());
    let op = build_operator(&c).expect("oss operator with static credentials builds without network");
    let cap = op.info().full_capability();
    assert!(
      cap.presign || cap.presign_read,
      "oss must report native presign capability (capability gate routing anchor)"
    );
  }

  #[cfg(feature = "oss")]
  #[test]
  fn oss_presign_endpoint_is_optional() {
    let mut c = StorageConfig::new("oss");
    c.endpoint = Some("https://oss-cn-hangzhou.aliyuncs.com".to_owned());
    c.bucket = Some("test-bucket".to_owned());
    c.access_key = Some("ak".to_owned());
    c.secret_key = Some("sk".to_owned());
    c.presign_endpoint = Some("https://cdn.example.com".to_owned());
    build_operator(&c).expect("oss operator with presign_endpoint builds");
  }

  #[cfg(feature = "s3")]
  #[test]
  fn s3_operator_constructs_with_custom_endpoint() {
    let mut c = StorageConfig::new("s3");
    c.endpoint = Some("http://minio.local:9000".to_owned());
    c.region = Some("us-east-1".to_owned());
    c.bucket = Some("test-bucket".to_owned());
    c.access_key = Some("ak".to_owned());
    c.secret_key = Some("sk".to_owned());
    let op = build_operator(&c).expect("s3 operator with endpoint + region + static credentials");
    assert!(op.info().full_capability().presign_write, "s3 must report native presign_write capability");
  }

  #[cfg(feature = "obs")]
  #[test]
  fn obs_operator_constructs_with_dummy_credentials() {
    let mut c = StorageConfig::new("obs");
    c.endpoint = Some("https://obs.cn-north-4.myhuaweicloud.com".to_owned());
    c.bucket = Some("test-bucket".to_owned());
    c.access_key = Some("ak".to_owned());
    c.secret_key = Some("sk".to_owned());
    let op = build_operator(&c).expect("obs operator with static credentials");
    assert!(op.info().full_capability().presign, "obs must report native presign capability");
  }
}
