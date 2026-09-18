//! 进程内图形验证码(challenge-response)——防自动化刷量前置件。
//!
//! 形态:4 位算式(`a + b = ?`)渲染为自研 SVG(无第三方依赖、无行为采集),
//! 干扰线 3 条;一次性(验证即删,无论成败)、默认 2 分钟有效。
//! 单机口径(与限流同假设):存储在进程内存,重启即失效——攻击面为重启窗口
//! 内的重放,由短有效期与一次性语义收敛。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 默认有效期。
const DEFAULT_TTL: Duration = Duration::from_secs(120);
/// 在途条目硬上限:正常流量远不可达;触达 = 自动化刷量,整体清空(在途验证码
/// 集体失效可接受——清空本身即是防刷响应,且 2 分钟 TTL 使真实用户重取成本低)。
const MAX_INFLIGHT: usize = 65_536;

/// 一次验证码挑战。
#[derive(Debug, Clone)]
pub struct CaptchaChallenge {
  /// 挑战 id(客户端回传定位)。
  pub id: String,
  /// 渲染好的 SVG(客户端直接内嵌展示)。
  pub svg: String,
}

struct InflightAnswer {
  answer: String,
  expires_at: Instant,
}

/// 进程内一次性验证码存储(`Clone` 语义 = 共享同一存储,适合随服务状态分发)。
#[derive(Clone)]
pub struct CaptchaStore {
  inner: std::sync::Arc<Mutex<HashMap<String, InflightAnswer>>>,
  ttl: Duration,
}

impl CaptchaStore {
  pub fn new() -> Self {
    Self::with_ttl(DEFAULT_TTL)
  }

  /// 自定义有效期(测试用秒级 TTL)。
  pub fn with_ttl(ttl: Duration) -> Self {
    Self { inner: std::sync::Arc::new(Mutex::new(HashMap::new())), ttl }
  }

  /// 生成一次挑战:算式 `a + b = ?`(a,b ∈ 2..=19,答案 4..=38,两位数内)。
  pub fn generate(&self) -> CaptchaChallenge {
    let a: u32 = rand::random_range(2..=19);
    let b: u32 = rand::random_range(2..=19);
    let answer = (a + b).to_string();
    let text = format!("{a} + {b} = ?");

    let id = format!("{:032x}", rand::random::<u128>());
    {
      let mut guard = self.inner.lock().expect("captcha store poisoned");
      purge_expired(&mut guard);
      if guard.len() >= MAX_INFLIGHT {
        guard.clear();
        tracing::warn!(target: "fusion_security::captcha", "captcha: in-flight cap ({MAX_INFLIGHT}) hit, store flushed");
      }
      guard.insert(id.clone(), InflightAnswer { answer, expires_at: Instant::now() + self.ttl });
    }

    CaptchaChallenge { id, svg: render_svg(&text) }
  }

  /// 校验(一次性):取出即删,成败皆作废;过期同样作废。输入宽松 trim。
  pub fn verify(&self, id: &str, answer: &str) -> bool {
    let Some(stored) = self.inner.lock().expect("captcha store poisoned").remove(id) else {
      return false;
    };
    if Instant::now() >= stored.expires_at {
      return false;
    }
    stored.answer == answer.trim()
  }
}

/// `Default` 与 [`CaptchaStore::new`] 同语义(默认 TTL)——derive 的 `Default`
/// 会把 `ttl` 置零使所有挑战立即过期,故手写。
impl Default for CaptchaStore {
  fn default() -> Self {
    Self::new()
  }
}

fn purge_expired(store: &mut HashMap<String, InflightAnswer>) {
  let now = Instant::now();
  store.retain(|_, v| v.expires_at > now);
}

/// 自研 SVG:算式文本 + 3 条随机干扰线。字符集固定(数字/加号/等号/问号),
/// 无用户输入进 SVG,不存在注入面。
fn render_svg(text: &str) -> String {
  let mut lines = String::new();
  for _ in 0..3 {
    let x1: u32 = rand::random_range(0..120);
    let y1: u32 = rand::random_range(0..40);
    let x2: u32 = rand::random_range(0..120);
    let y2: u32 = rand::random_range(0..40);
    lines.push_str(&format!(r##"<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="#c8d0dc" stroke-width="1"/>"##));
  }
  format!(
    r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="40" viewBox="0 0 120 40" role="img" aria-label="captcha">{lines}<text x="60" y="27" text-anchor="middle" font-family="ui-monospace, Menlo, monospace" font-size="20" fill="#16294d" letter-spacing="2">{text}</text></svg>"##
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  /// 测试辅助:从 svg 里解出算式两操作数(模拟客户端识读)。
  fn expr_operands(svg: &str) -> (u32, u32) {
    let expr = svg.split('>').find(|s| s.contains('+')).and_then(|s| s.split('<').next()).unwrap();
    let lhs = expr.split(" = ").next().unwrap();
    let mut it = lhs.split(" + ");
    (it.next().unwrap().parse().unwrap(), it.next().unwrap().parse().unwrap())
  }

  #[test]
  fn verify_accepts_correct_answer_then_burns_challenge() {
    let store = CaptchaStore::new();
    // 算式文本在 svg 里,答案 = a+b;从 svg 解析回来(测试视角模拟客户端)
    let c = store.generate();
    let (a, b) = expr_operands(&c.svg);
    let answer = (a + b).to_string();

    assert!(store.verify(&c.id, &answer), "correct answer accepted");
    assert!(!store.verify(&c.id, &answer), "challenge is one-shot — replay rejected");
  }

  #[test]
  fn verify_rejects_wrong_answer_and_unknown_id() {
    let store = CaptchaStore::new();
    let c = store.generate();
    assert!(!store.verify(&c.id, "9999"));
    assert!(!store.verify("nonexistent", "12"));
  }

  #[test]
  fn verify_trims_input() {
    let store = CaptchaStore::new();
    let c = store.generate();
    let (a, b) = expr_operands(&c.svg);
    assert!(store.verify(&c.id, &format!("  {}  ", a + b)));
  }

  #[test]
  fn challenge_expires_after_ttl() {
    let store = CaptchaStore::with_ttl(Duration::from_millis(30));
    let c = store.generate();
    let (a, b) = expr_operands(&c.svg);
    std::thread::sleep(Duration::from_millis(60));
    assert!(!store.verify(&c.id, &(a + b).to_string()), "expired challenge rejected");
  }

  #[test]
  fn default_matches_new_ttl() {
    // 手写 Default 防 ttl=0 回归(derive 会把 Duration 置零使挑战立即过期)
    let d = CaptchaStore::default();
    let c = d.generate();
    let (a, b) = expr_operands(&c.svg);
    assert!(d.verify(&c.id, &(a + b).to_string()), "default() must use DEFAULT_TTL, not zero");
  }

  #[test]
  fn svg_shape_is_stable() {
    let store = CaptchaStore::new();
    let c = store.generate();
    assert!(c.svg.starts_with("<svg ") && c.svg.ends_with("</svg>"));
    assert!(c.svg.contains('+') && c.svg.contains("= ?"));
    assert!(c.svg.matches("<line ").count() == 3, "exactly 3 noise lines");
    assert_eq!(c.id.len(), 32);
  }
}
