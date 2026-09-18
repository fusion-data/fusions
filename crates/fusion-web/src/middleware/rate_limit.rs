//! 进程内 keyed token-bucket 限流中间件(公开端点防滥用)。
//!
//! 单机口径:状态在进程内存,多实例部署时每实例独立计数(阈值按实例数折算,
//! 或届时升级外部存储——本层不引依赖)。429 响应体固定
//! `{"error":"rate_limited"}`(机器面英文;用户可见文案由前端按错误码兜底),
//! 并带 `Retry-After` 头。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use http::{Request, Response, StatusCode, header};
use tower_http::auth::{AsyncAuthorizeRequest, AsyncRequireAuthorizationLayer};

/// 空闲桶硬上限:正常 key 空间(真实 IP 数)远不可达;触达 = key 伪造 / 扫描,
/// 清空仅损失计数精度,不放大拒绝面。
const MAX_KEYS: usize = 65_536;

struct Bucket {
  tokens: f64,
  last_refill: Instant,
}

struct RateLimiterInner {
  buckets: Mutex<HashMap<String, Bucket>>,
  capacity: f64,
  refill_per_sec: f64,
}

impl RateLimiterInner {
  /// 取一枚令牌;返回 (是否放行, 建议重试等待秒数)。
  fn take(&self, key: &str) -> (bool, u64) {
    let mut guard = self.buckets.lock().expect("rate limiter poisoned");
    if guard.len() >= MAX_KEYS {
      let now = Instant::now();
      guard.retain(|_, b| b.tokens < self.capacity && now - b.last_refill < Duration::from_secs(3600));
    }
    let now = Instant::now();
    let bucket = guard.entry(key.to_string()).or_insert(Bucket { tokens: self.capacity, last_refill: now });
    let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
    bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
    bucket.last_refill = now;
    if bucket.tokens >= 1.0 {
      bucket.tokens -= 1.0;
      (true, 0)
    } else {
      let need = 1.0 - bucket.tokens;
      (false, (need / self.refill_per_sec).ceil().max(1.0) as u64)
    }
  }
}

/// key 提取器类型(请求 → 限流维度键,如客户端 IP)。
pub type RateLimitKeyFn = Arc<dyn Fn(&Request<Body>) -> String + Send + Sync>;

/// 限流中间件(`Clone` = 共享同一计数器,适合随 router 装配)。
///
/// key 提取器决定维度(默认 IP:代理 `X-Forwarded-For` 首段 → `X-Real-IP` →
/// `ConnectInfo<SocketAddr>` 扩展 → `"unknown"`)。XFF 信任前提 = 前置反代
/// 覆写该头(单域部署标准行为);直连暴露场景下客户端可伪造 XFF 谋取更大
/// key 空间——由部署面保证反代在位。
#[derive(Clone)]
pub struct RateLimiter {
  inner: Arc<RateLimiterInner>,
  key_extractor: RateLimitKeyFn,
}

impl RateLimiter {
  /// IP 维度限流:`burst` = 瞬时容量,`per_minute` = 每分钟补充令牌数。
  pub fn per_ip(burst: u32, per_minute: u32) -> Self {
    Self {
      inner: Arc::new(RateLimiterInner {
        buckets: Mutex::new(HashMap::new()),
        capacity: burst as f64,
        refill_per_sec: per_minute as f64 / 60.0,
      }),
      key_extractor: Arc::new(client_ip_key),
    }
  }

  /// 自定义 key 提取器(header / 路径 / 账号 id 等维度)。
  pub fn with_key_extractor(mut self, extractor: RateLimitKeyFn) -> Self {
    self.key_extractor = extractor;
    self
  }

  /// Handler-side admission check(shared counter with the layer form;returns false = over limit).
  /// axum layer cannot filter by RPC method(handler-internal call is the precise per-method form).
  pub fn check(&self, key: &str) -> bool {
    self.inner.take(key).0
  }

  pub fn into_layer(self) -> AsyncRequireAuthorizationLayer<Self> {
    AsyncRequireAuthorizationLayer::new(self)
  }
}

impl AsyncAuthorizeRequest<Body> for RateLimiter {
  type RequestBody = Body;
  type ResponseBody = Body;
  type Future =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Request<Body>, Response<Self::ResponseBody>>> + Send>>;

  fn authorize(&mut self, request: Request<Body>) -> Self::Future {
    let limiter = self.inner.clone();
    let key = (self.key_extractor)(&request);
    Box::pin(async move {
      let (allowed, retry_after) = limiter.take(&key);
      if allowed { Ok(request) } else { Err(rate_limited_response(retry_after)) }
    })
  }
}

fn client_ip_key(req: &Request<Body>) -> String {
  if let Some(first) = req
    .headers()
    .get("x-forwarded-for")
    .and_then(|v| v.to_str().ok())
    .and_then(|xff| xff.split(',').next().map(|s| s.trim()))
    .filter(|s| !s.is_empty())
  {
    return first.to_string();
  }
  if let Some(rip) = req.headers().get("x-real-ip").and_then(|v| v.to_str().ok()).filter(|s| !s.is_empty()) {
    return rip.to_string();
  }
  if let Some(addr) = req.extensions().get::<std::net::SocketAddr>() {
    return addr.ip().to_string();
  }
  "unknown".to_string()
}

fn rate_limited_response(retry_after_secs: u64) -> Response<Body> {
  Response::builder()
    .status(StatusCode::TOO_MANY_REQUESTS)
    .header(header::CONTENT_TYPE, "application/json")
    .header(header::RETRY_AFTER, retry_after_secs.to_string())
    .body(Body::from(r#"{"error":"rate_limited"}"#))
    .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
  use super::*;

  fn req_with_ip(ip: &str) -> Request<Body> {
    Request::builder()
      .method("POST")
      .uri("/x")
      .header("x-forwarded-for", ip)
      .body(Body::empty())
      .unwrap()
  }

  async fn run(limiter: &mut RateLimiter, req: Request<Body>) -> Result<Request<Body>, Response<Body>> {
    // authorize 需要 &mut self(AsyncAuthorizeRequest),clone 共享计数器
    let mut l = limiter.clone();
    AsyncAuthorizeRequest::<Body>::authorize(&mut l, req).await
  }

  #[tokio::test]
  async fn burst_allows_up_to_capacity_then_429() {
    let mut limiter = RateLimiter::per_ip(3, 60);
    for i in 0..3 {
      assert!(run(&mut limiter, req_with_ip("1.1.1.1")).await.is_ok(), "request {i} within burst");
    }
    let r = run(&mut limiter, req_with_ip("1.1.1.1")).await;
    assert!(r.is_err(), "4th request exceeds burst");
    let resp = r.unwrap_err();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(resp.headers().contains_key(header::RETRY_AFTER));
    // per_minute=60 → 每秒补 1 枚;刚耗尽时 need≈1 → Retry-After=1
    assert_eq!(resp.headers().get(header::RETRY_AFTER).unwrap(), "1");
  }

  #[tokio::test]
  async fn refill_recovers_after_wait() {
    // burst 1 / 每分钟 600 = 每 0.1s 补 1 枚
    let mut limiter = RateLimiter::per_ip(1, 600);
    assert!(run(&mut limiter, req_with_ip("2.2.2.2")).await.is_ok());
    assert!(run(&mut limiter, req_with_ip("2.2.2.2")).await.is_err());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(run(&mut limiter, req_with_ip("2.2.2.2")).await.is_ok(), "token refilled");
  }

  #[tokio::test]
  async fn keys_are_isolated() {
    let mut limiter = RateLimiter::per_ip(1, 60);
    assert!(run(&mut limiter, req_with_ip("3.3.3.3")).await.is_ok());
    assert!(run(&mut limiter, req_with_ip("3.3.3.3")).await.is_err());
    assert!(run(&mut limiter, req_with_ip("4.4.4.4")).await.is_ok(), "different key unaffected");
  }

  #[tokio::test]
  async fn xff_first_segment_wins_and_trimmed() {
    let mut limiter = RateLimiter::per_ip(1, 60);
    let r1 = Request::builder()
      .method("POST")
      .uri("/x")
      .header("x-forwarded-for", " 5.5.5.5 , 10.0.0.1 ")
      .body(Body::empty())
      .unwrap();
    assert!(run(&mut limiter, r1).await.is_ok());
    let r2 = Request::builder()
      .method("POST")
      .uri("/x")
      .header("x-forwarded-for", "5.5.5.5")
      .body(Body::empty())
      .unwrap();
    assert!(run(&mut limiter, r2).await.is_err(), "same first XFF segment = same bucket");
  }

  #[tokio::test]
  async fn missing_ip_falls_back_to_unknown() {
    let mut limiter = RateLimiter::per_ip(1, 60);
    let r = Request::builder().method("POST").uri("/x").body(Body::empty()).unwrap();
    assert!(run(&mut limiter, r).await.is_ok());
    let r2 = Request::builder().method("POST").uri("/x").body(Body::empty()).unwrap();
    assert!(run(&mut limiter, r2).await.is_err(), "no-ip requests share the fallback bucket");
  }

  #[tokio::test]
  async fn custom_key_extractor() {
    let limiter = RateLimiter::per_ip(1, 60).with_key_extractor(Arc::new(|_req: &Request<Body>| "fixed".to_string()));
    let mut l = limiter.clone();
    let r = Request::builder().method("POST").uri("/x").body(Body::empty()).unwrap();
    assert!(AsyncAuthorizeRequest::<Body>::authorize(&mut l, r).await.is_ok());
    let r2 = Request::builder().method("POST").uri("/x").body(Body::empty()).unwrap();
    assert!(AsyncAuthorizeRequest::<Body>::authorize(&mut l, r2).await.is_err());
  }
}
