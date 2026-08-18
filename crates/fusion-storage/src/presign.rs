//! 预签名 URL 生成（读 + 写）。
//!
//! 机制：
//! - 云后端（capability `presign` / `presign_read` / `presign_write`）：走 opendal native
//!   presign（云厂商签名由 opendal/reqsign 负责，本 crate 不复刻其正确性）。
//! - fs 后端：走 HMAC 签名的本地路由 URL；路由形态（挂载点路径）由消费方经
//!   [`FsPresignRoutes`] 注入，密钥由消费方传入。
//!
//! capability gate 让 fs→云后端切换零代码改动（云后端原生支持 presign）。
//!
//! 签名缓存：同一输入在同一 ttl 桶内复用签好的 URL。expires 对齐到 ttl 桶边界
//! （fs 路径显式对齐；云 native presign 内部取时间戳、调用方无法对齐，由缓存提供
//! 同桶稳定性），服务于轮询类列表接口的响应稳定（同桶同 URL，消费方不误判「数据变了」）。

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use opendal::Operator;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};

use crate::hmac::{sign_read, sign_upload};

/// fs 后端预签名 URL 的消费方装配面：对外 authority + scheme + 两个路由挂载点。
///
/// 路由 URL 形态归消费方（本 crate 只做组装机制）：`read` / `write` 必须与消费方
/// 实际挂载的本地路由一致，生成的 URL 才能被其验签 handler 接住。
#[derive(Debug, Clone, Copy)]
pub struct FsPresignRoutes<'a> {
  /// 对外 authority（如 "files.example.test"）。
  pub authority: &'a str,
  /// scheme（"http" / "https"）。
  pub scheme: &'a str,
  /// 读路由挂载点（如 "/storage"）。
  pub read: &'a str,
  /// 写路由挂载点（如 "/storage-upload"）。
  pub write: &'a str,
}

/// expires 桶对齐：`(now / ttl + 2) * ttl`。
///
/// 列表接口出口处现签 URL 时，若 expires 取 `now + ttl`，秒级时间差让同一对象的 URL
/// 每次响应都不同，轮询方永远判定「数据变了」（如视频 src 更新 → 重载）。对齐到
/// ttl 桶边界后，同一桶内同一 (key, creator_id) 的签名完全一致；跨桶时旧 URL 仍剩
/// > ttl 的有效期，消费方有完整窗口切换。`+2` 保证桶内最短剩余有效期 ≥ ttl。
fn bucketed_expires(now_secs: i64, ttl_secs: u64) -> i64 {
  ((now_secs / ttl_secs as i64) + 2) * ttl_secs as i64
}

/// 签名缓存条目键：决定「同一签名输入」的全部维度。
///
/// `hmac_secret` 刻意不在键内：缓存按「密钥进程不变」假设工作（消费方以进程级
/// 全局装配一次密钥）。调用方在进程内轮换密钥会拿到旧密钥的缓存 URL——如需密钥
/// 轮换，消费方必须自行保证换密钥即换进程（滚动重启）。
#[derive(Hash, PartialEq, Eq, Clone)]
struct PresignCacheKey {
  /// false = read（download），true = write（upload）。
  write: bool,
  key: String,
  creator_id: String,
  ttl_secs: u64,
  download_filename: Option<String>,
  fs_authority: String,
  fs_scheme: String,
  fs_read_route: String,
  fs_write_route: String,
}

/// 签名结果桶缓存：同一输入在同一 ttl 桶内直接复用签好的 URL。
///
/// OSS native presign（reqsign）内部取当前时间戳，调用方无法像 fs 路径那样对齐
/// expires——本缓存让两种后端获得一致的「同桶同 URL」稳定性。命中条件 = 签发桶
/// 未变；桶宽 = ttl。云后端条目桶尾最短剩余有效期可至 0（桶末拿到的 URL 在跨桶
/// 瞬间过期，前端下一轮轮询即换新）；fs 无此问题（expires 显式 +2 桶）。上限
/// 4096 懒清理防无界增长（满时先清过期桶，仍满整体清空——签名可随时重算，清空无损）。
static PRESIGN_CACHE: LazyLock<Mutex<HashMap<PresignCacheKey, (String, i64)>>> =
  LazyLock::new(|| Mutex::new(HashMap::new()));

const PRESIGN_CACHE_MAX_ENTRIES: usize = 4096;

/// 缓存查找：同桶命中返回缓存 URL。
fn presign_cache_hit(key: &PresignCacheKey, now_bucket: i64) -> Option<String> {
  let cache = PRESIGN_CACHE.lock().unwrap();
  cache.get(key).filter(|(_, b)| *b == now_bucket).map(|(url, _)| url.clone())
}

/// 缓存写入（带上限懒清理）。
fn presign_cache_store(key: PresignCacheKey, url: &str, now_bucket: i64) {
  let mut cache = PRESIGN_CACHE.lock().unwrap();
  if cache.len() >= PRESIGN_CACHE_MAX_ENTRIES {
    cache.retain(|_, (_, b)| *b >= now_bucket);
  }
  if cache.len() >= PRESIGN_CACHE_MAX_ENTRIES {
    cache.clear();
  }
  cache.insert(key, (url.to_string(), now_bucket));
}

/// 生成下载用预签名 URL。
///
/// - 云后端：opendal `presign_read_with`（可选 `override_content_disposition`）。
/// - fs 后端：HMAC 签名的
///   `{scheme}://{authority}{read}/{key}?token=...&expires=...&creator_id=...`。
///
/// # Arguments
/// * `op` - opendal Operator
/// * `key` - storage key
/// * `creator_id` - 签名绑定的主体标识（fs HMAC 签名绑定）
/// * `ttl_secs` - 有效期秒数
/// * `download_filename` - 可选下载文件名（Content-Disposition）
/// * `routes` - fs 后端 URL 装配面（云后端忽略）
/// * `hmac_secret` - fs 后端 HMAC 密钥（云后端忽略）。MUST 进程不变——签名缓存按
///   此假设工作（见 [`PresignCacheKey`]），进程内轮换会复用旧密钥的缓存 URL。
pub async fn generate_signed_download_url(
  op: &Operator,
  key: &str,
  creator_id: &str,
  ttl_secs: u64,
  download_filename: Option<&str>,
  routes: &FsPresignRoutes<'_>,
  hmac_secret: &[u8],
) -> Result<String, String> {
  let now_bucket = chrono::Utc::now().timestamp() / ttl_secs as i64;
  let cache_key = PresignCacheKey {
    write: false,
    key: key.to_string(),
    creator_id: creator_id.to_string(),
    ttl_secs,
    download_filename: download_filename.map(str::to_string),
    fs_authority: routes.authority.to_string(),
    fs_scheme: routes.scheme.to_string(),
    fs_read_route: routes.read.to_string(),
    fs_write_route: routes.write.to_string(),
  };
  if let Some(url) = presign_cache_hit(&cache_key, now_bucket) {
    return Ok(url);
  }

  let url = presign_download_uncached(op, key, creator_id, ttl_secs, download_filename, routes, hmac_secret).await?;
  presign_cache_store(cache_key, &url, now_bucket);
  Ok(url)
}

/// [`generate_signed_download_url`] 的缓存 miss 路径（现签）。
async fn presign_download_uncached(
  op: &Operator,
  key: &str,
  creator_id: &str,
  ttl_secs: u64,
  download_filename: Option<&str>,
  routes: &FsPresignRoutes<'_>,
  hmac_secret: &[u8],
) -> Result<String, String> {
  let cap = op.info().full_capability();
  if cap.presign || cap.presign_read {
    // 云后端：native presign_read
    let mut fut = op.presign_read_with(key, Duration::from_secs(ttl_secs));
    if let Some(name) = download_filename {
      fut = fut.override_content_disposition(&content_disposition_attachment(name));
    }
    let req = fut.await.map_err(|e| format!("presign_read failed: {e}"))?;
    return Ok(req.uri().to_string());
  }

  // fs：HMAC 签名本地路由
  let expires = bucketed_expires(chrono::Utc::now().timestamp(), ttl_secs);
  let token = sign_read(key, expires, creator_id, hmac_secret);
  Ok(fs_signed_url(routes.scheme, routes.authority, routes.read, key, &token, expires, creator_id, download_filename))
}

/// 生成上传用预签名 URL（两步直传 init）。
///
/// - 云后端：opendal `presign_write_with`。
/// - fs 后端：HMAC 签名的 `{write}/{key}` 路由。
///
/// 注意：opendal 0.55+ presign_write 丢弃 content_type（用 OpWrite::default()），
/// confirm 步骤 MUST 调 op.stat 重新校验 size/content_type/prefix。
pub async fn generate_signed_upload_url(
  op: &Operator,
  key: &str,
  creator_id: &str,
  ttl_secs: u64,
  routes: &FsPresignRoutes<'_>,
  hmac_secret: &[u8],
) -> Result<String, String> {
  let now_bucket = chrono::Utc::now().timestamp() / ttl_secs as i64;
  let cache_key = PresignCacheKey {
    write: true,
    key: key.to_string(),
    creator_id: creator_id.to_string(),
    ttl_secs,
    download_filename: None,
    fs_authority: routes.authority.to_string(),
    fs_scheme: routes.scheme.to_string(),
    fs_read_route: routes.read.to_string(),
    fs_write_route: routes.write.to_string(),
  };
  if let Some(url) = presign_cache_hit(&cache_key, now_bucket) {
    return Ok(url);
  }

  let url = presign_upload_uncached(op, key, creator_id, ttl_secs, routes, hmac_secret).await?;
  presign_cache_store(cache_key, &url, now_bucket);
  Ok(url)
}

/// [`generate_signed_upload_url`] 的缓存 miss 路径（现签）。
async fn presign_upload_uncached(
  op: &Operator,
  key: &str,
  creator_id: &str,
  ttl_secs: u64,
  routes: &FsPresignRoutes<'_>,
  hmac_secret: &[u8],
) -> Result<String, String> {
  let cap = op.info().full_capability();
  if cap.presign_write {
    let req = op
      .presign_write_with(key, Duration::from_secs(ttl_secs))
      .await
      .map_err(|e| format!("presign_write failed: {e}"))?;
    return Ok(req.uri().to_string());
  }

  // fs：HMAC 签名上传路由
  let expires = bucketed_expires(chrono::Utc::now().timestamp(), ttl_secs);
  let token = sign_upload(key, expires, creator_id, hmac_secret);
  Ok(fs_signed_url(routes.scheme, routes.authority, routes.write, key, &token, expires, creator_id, None))
}

/// fs 后端签名 URL 组装（wire 格式，MUST NOT 变更——签发与消费方验签 handler 跨版本互认）：
/// `{scheme}://{authority}{route}/{key}?token=..&expires=..&creator_id=..[&download_filename=..]`
fn fs_signed_url(
  scheme: &str,
  authority: &str,
  route: &str,
  key: &str,
  token: &str,
  expires: i64,
  creator_id: &str,
  download_filename: Option<&str>,
) -> String {
  let mut url = format!("{scheme}://{authority}{route}/{key}?token={token}&expires={expires}&creator_id={creator_id}",);
  if let Some(name) = download_filename {
    url.push_str("&download_filename=");
    url.push_str(&percent_encode_query(name));
  }
  url
}

fn percent_encode_query(s: &str) -> String {
  utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// 清理文件名中的特殊字符（Content-Disposition 安全线）。
fn sanitize_filename(name: &str) -> String {
  name.replace(['"', '\\'], "")
}

/// 下载用 Content-Disposition：`attachment` + 文件名。
///
/// 纯 ASCII → 单 `filename=`；含非 ASCII → 双写 `filename*=`（RFC 5987 percent-encoded
/// UTF-8），兼顾 Chrome / Firefox / Safari（仅 `filename=` 带非 ASCII 在 Safari/Firefox
/// 会乱码）。fs 验签 handler 与云后端 presign 出口共用此函数，保证两条路径文件名一致。
pub fn content_disposition_attachment(filename: &str) -> String {
  let clean = sanitize_filename(filename);
  if clean.is_ascii() {
    format!("attachment; filename=\"{clean}\"")
  } else {
    let encoded = utf8_percent_encode(&clean, NON_ALPHANUMERIC).to_string();
    format!("attachment; filename=\"download\"; filename*=UTF-8''{encoded}")
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // 金样对拍锚：与抽取前消费仓实现对同一输入的产出逐字节一致（wire 契约跨版本锁定）。
  // secret/输入与 hmac.rs 金样同源，URL 由 fs_signed_url 纯函数以固定 expires 直接组装。
  const GOLDEN_SECRET: &[u8] = b"golden-secret";
  const GOLDEN_CREATOR: &str = "creator-1";
  const GOLDEN_KEY: &str = "creator-1/knowledge/asset-1";
  const GOLDEN_EXPIRES: i64 = 4102444800;
  const GOLDEN_READ_URL: &str = "http://files.example.test/storage/creator-1/knowledge/asset-1?token=d9da5de5c0962fdf7c290b00257a026b216cee83912e21bb3f8c22b1826b0ea1&expires=4102444800&creator_id=creator-1";
  const GOLDEN_UPLOAD_URL: &str = "http://files.example.test/storage-upload/creator-1/knowledge/asset-1?token=d5da3d8d851dc4263421343ee21eb0d2b4a01fa2682fc2178c6cd376998b7b22&expires=4102444800&creator_id=creator-1";

  fn golden_routes() -> FsPresignRoutes<'static> {
    FsPresignRoutes { authority: "files.example.test", scheme: "http", read: "/storage", write: "/storage-upload" }
  }

  fn fs_operator() -> Operator {
    let dir = std::env::temp_dir().join(format!("fusion-storage-presign-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp root");
    Operator::new(opendal::services::Fs::default().root(dir.to_str().expect("utf-8 path")))
      .expect("wrap fs access")
      .finish()
  }

  fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio rt").block_on(fut)
  }

  /// 金样对拍：fs 读 URL 与抽取前实现逐字节一致（路由形态经 FsPresignRoutes 注入）。
  #[test]
  fn fs_read_url_matches_pre_extraction_golden() {
    let token = sign_read(GOLDEN_KEY, GOLDEN_EXPIRES, GOLDEN_CREATOR, GOLDEN_SECRET);
    let url =
      fs_signed_url("http", "files.example.test", "/storage", GOLDEN_KEY, &token, GOLDEN_EXPIRES, GOLDEN_CREATOR, None);
    assert_eq!(url, GOLDEN_READ_URL);
  }

  /// 金样对拍：fs 写 URL 与抽取前实现逐字节一致。
  #[test]
  fn fs_upload_url_matches_pre_extraction_golden() {
    let token = sign_upload(GOLDEN_KEY, GOLDEN_EXPIRES, GOLDEN_CREATOR, GOLDEN_SECRET);
    let url = fs_signed_url(
      "http",
      "files.example.test",
      "/storage-upload",
      GOLDEN_KEY,
      &token,
      GOLDEN_EXPIRES,
      GOLDEN_CREATOR,
      None,
    );
    assert_eq!(url, GOLDEN_UPLOAD_URL);
  }

  /// download_filename 追加为 percent-encoded query（NON_ALPHANUMERIC 连 `-` / `.`
  /// 一并编码，与抽取前行为一致）。
  #[test]
  fn fs_read_url_appends_encoded_download_filename() {
    let url = fs_signed_url("http", "h", "/storage", "k.mp4", "t", 1000, "c", Some("01-认识数据.mp4"));
    assert!(url.ends_with("&download_filename=01%2D%E8%AE%A4%E8%AF%86%E6%95%B0%E6%8D%AE%2Emp4"), "got: {url}");
  }

  #[test]
  fn content_disposition_ascii_filename() {
    assert_eq!(content_disposition_attachment("final.mp4"), "attachment; filename=\"final.mp4\"");
  }

  #[test]
  fn content_disposition_unicode_filename() {
    // 中文文件名走 RFC 5987 双写：filename*=UTF-8''<percent-encoded>
    let d = content_disposition_attachment("01-认识数据.mp4");
    assert!(d.starts_with("attachment; filename=\"download\"; filename*=UTF-8''"));
    // 「认」= E8 AE A4 → %E8%AE%A4，验证中文被 percent-encode
    assert!(d.contains("%E8%AE%A4"));
  }

  #[test]
  fn content_disposition_strips_quotes_and_backslashes() {
    // sanitize_filename 去 " 和 \（Content-Disposition 注入防线）
    let d = content_disposition_attachment("a\"b\\c.mp4");
    assert_eq!(d, "attachment; filename=\"abc.mp4\"");
  }

  #[test]
  fn bucketed_expires_is_stable_within_bucket() {
    // 同一 ttl 桶内任意时刻签名，expires 一致（轮询响应 URL 稳定的前提）
    let ttl = 900i64;
    let a = bucketed_expires(1_000_000_000 * ttl + 1, 900);
    let b = bucketed_expires(1_000_000_000 * ttl + ttl - 1, 900);
    assert_eq!(a, b);
    assert_eq!(a, (1_000_000_000 + 2) * ttl);
  }

  #[test]
  fn bucketed_expires_keeps_full_ttl_margin() {
    // 桶内最晚时刻（桶边界前 1s）签名，剩余有效期仍 ≥ ttl
    let ttl = 900i64;
    let bucket_start = 1_000_000_000 * ttl;
    let expires = bucketed_expires(bucket_start + ttl - 1, 900);
    assert!(expires - (bucket_start + ttl - 1) >= ttl);
  }

  /// capability gate 分流锚（fs 侧）：无 native presign 能力 → HMAC 路由 URL。
  /// 只锚分流行为；云厂商 native presign 的签名正确性是 opendal 上游测试责任。
  #[test]
  fn fs_backend_routes_to_hmac_local_url() {
    let op = fs_operator();
    let url = block_on(generate_signed_download_url(&op, "k.mp4", "u1", 900, None, &golden_routes(), b"s"))
      .expect("fs presign");
    assert!(url.starts_with("http://files.example.test/storage/k.mp4?token="), "got: {url}");
    assert!(url.contains("&expires="));
    assert!(url.ends_with("&creator_id=u1"));
    // token 可被 verify 侧原样验回（签发↔验签闭环）
    let token = url.split("token=").nth(1).and_then(|t| t.split('&').next()).expect("token in url");
    let expires: i64 = url
      .split("expires=")
      .nth(1)
      .and_then(|t| t.split('&').next())
      .expect("expires")
      .parse()
      .expect("i64");
    assert!(crate::hmac::verify_hmac("k.mp4", expires, token, "u1", b"s"));
  }

  /// capability gate 分流锚（fs 上传侧）。
  #[test]
  fn fs_backend_upload_routes_to_hmac_local_url() {
    let op = fs_operator();
    let url =
      block_on(generate_signed_upload_url(&op, "k.mp4", "u1", 900, &golden_routes(), b"s")).expect("fs presign");
    assert!(url.starts_with("http://files.example.test/storage-upload/k.mp4?token="), "got: {url}");
  }

  /// 路由形态注入生效：不同 routes 产出不同 URL 前缀（机制不绑死消费方挂载点）。
  #[test]
  fn routes_injection_changes_url_shape() {
    let op = fs_operator();
    let custom = FsPresignRoutes { authority: "h2", scheme: "https", read: "/files", write: "/files-put" };
    let url = block_on(generate_signed_download_url(&op, "k", "u1", 900, None, &custom, b"s")).expect("presign");
    assert!(url.starts_with("https://h2/files/k?token="), "got: {url}");
  }

  #[test]
  fn fs_download_url_is_deterministic_within_bucket() {
    // fs 后端：同一 (key, creator) 在同一桶内两次现签，URL 字节级一致
    // （opendal fs 无 presign capability → 走 HMAC 路径；桶对齐 + 缓存双保险）
    let op = fs_operator();
    let a = block_on(generate_signed_download_url(&op, "k.mp4", "u1", 900, None, &golden_routes(), b"s")).unwrap();
    let b = block_on(generate_signed_download_url(&op, "k.mp4", "u1", 900, None, &golden_routes(), b"s")).unwrap();
    assert_eq!(a, b);
  }

  #[test]
  fn presign_cache_isolates_inputs() {
    // 缓存键隔离：不同 creator / filename / host / 路由形态各自独立签名，互不串用。
    // hmac_secret 刻意不在缓存键（进程不变假设，见 PresignCacheKey 文档）——
    // 密钥维度的签名区分由 hmac 模块 sign/verify 层保证。
    let op = fs_operator();
    let other_routes = FsPresignRoutes { authority: "h2", scheme: "http", read: "/storage", write: "/storage-upload" };
    let base = block_on(generate_signed_download_url(&op, "k.mp4", "u1", 900, None, &golden_routes(), b"s")).unwrap();
    let other_creator =
      block_on(generate_signed_download_url(&op, "k.mp4", "u2", 900, None, &golden_routes(), b"s")).unwrap();
    let other_host =
      block_on(generate_signed_download_url(&op, "k.mp4", "u1", 900, None, &other_routes, b"s")).unwrap();
    let other_name =
      block_on(generate_signed_download_url(&op, "k.mp4", "u1", 900, Some("x.mp4"), &golden_routes(), b"s")).unwrap();
    assert_ne!(base, other_creator);
    assert_ne!(base, other_host);
    assert_ne!(base, other_name);
  }

  #[test]
  fn fs_upload_url_is_deterministic_within_bucket() {
    let op = fs_operator();
    let a = block_on(generate_signed_upload_url(&op, "k.mp4", "u1", 900, &golden_routes(), b"s")).unwrap();
    let b = block_on(generate_signed_upload_url(&op, "k.mp4", "u1", 900, &golden_routes(), b"s")).unwrap();
    assert_eq!(a, b);
  }
}
