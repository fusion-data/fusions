//! [`LlmChatProvider`] factory —— 按 [`LlmProviderConfig`] enum 派发到具体 impl。
//!
//! caller（上层 ai_route）构造 `LlmProviderConfig::Qwen { api_key, ... }`
//! 后调 [`build_provider`] 得 `Arc<dyn LlmChatProvider>`，业务层不直接知道哪家 vendor。

use std::sync::Arc;
use std::time::Duration;

use super::providers::anthropic::DEFAULT_MODEL_ANTHROPIC;
use super::providers::deepseek::{DEFAULT_ENDPOINT_DEEPSEEK, DEFAULT_MODEL_DEEPSEEK};
use super::providers::gemini::DEFAULT_MODEL_GEMINI;
use super::providers::openai::{DEFAULT_ENDPOINT_OPENAI, DEFAULT_MODEL_OPENAI};
use super::providers::qwen::DEFAULT_MODEL_QWEN;
use super::providers::{
  AnthropicChatProvider, DeepSeekChatProvider, GeminiChatProvider, OpenAiChatProvider, QwenChatProvider,
};
use super::{LlmChatProvider, LlmError, LlmProviderId, SharedLlmChatProvider};
use crate::providers::dashscope::{DashScopeRegion, parse_dashscope_region};

/// Provider 配置 enum —— 上层 ai_route 层从 `provider_credentials.config_json`
/// 解出 vendor 字段后填进对应 variant。
///
/// 注意：`api_key` 类字段全部是 plaintext（在消费方进程内解密后跨 internal
/// RPC 传回，不落盘）。该 enum **不要**实现 `Debug` —— 避免 `tracing::info!`
/// 误打到日志（`#[derive(Debug)]` 已显式不加）。
#[non_exhaustive]
pub enum LlmProviderConfig {
  Qwen {
    api_key: String,
    workspace_id: Option<String>,
    region: DashScopeRegion,
    default_chat_model: String,
    timeout: Option<Duration>,
    /// 覆盖由 `region` 决定的 chat 兼容端点。`None` = 用 region 的官方端点(生产恒为此)。
    ///
    /// 与 `DeepSeek { endpoint }` 一类字段的**性质不同**,故命名上刻意区分:那些 vendor 本就
    /// 支持自建 / 代理端点,endpoint 是凭证的一部分;dashscope 的端点由 `region` 单独决定,
    /// 而 `region` 又是本仓驻留档的判据。因此本字段 MUST NOT 由凭证或租户配置填充 ——
    /// 那等于让被判定方自报判据。它只承载**部署形态**的注入(如集成测试对着本地假服务端跑),
    /// 由进程级配置提供,且提供方 MUST 明确告知该进程的驻留标注不可信。
    base_url_override: Option<String>,
  },
  DeepSeek {
    api_key: String,
    endpoint: Option<String>,
    default_chat_model: String,
    timeout: Option<Duration>,
  },
  OpenAi {
    api_key: String,
    organization: Option<String>,
    endpoint: Option<String>,
    default_chat_model: String,
    timeout: Option<Duration>,
  },
  Anthropic {
    api_key: String,
    default_chat_model: String,
  },
  Gemini {
    api_key: String,
    default_chat_model: String,
  },
}

impl LlmProviderConfig {
  pub fn provider_id(&self) -> LlmProviderId {
    match self {
      Self::Qwen { .. } => LlmProviderId::Qwen,
      Self::DeepSeek { .. } => LlmProviderId::DeepSeek,
      Self::OpenAi { .. } => LlmProviderId::OpenAi,
      Self::Anthropic { .. } => LlmProviderId::Anthropic,
      Self::Gemini { .. } => LlmProviderId::Gemini,
    }
  }

  /// 系统默认模型名 —— ResolveRoute 未命中 / 用户未配置 model 时回退。
  pub fn provider_default_model(id: LlmProviderId) -> &'static str {
    match id {
      LlmProviderId::Qwen => DEFAULT_MODEL_QWEN,
      LlmProviderId::DeepSeek => DEFAULT_MODEL_DEEPSEEK,
      LlmProviderId::OpenAi => DEFAULT_MODEL_OPENAI,
      LlmProviderId::Anthropic => DEFAULT_MODEL_ANTHROPIC,
      LlmProviderId::Gemini => DEFAULT_MODEL_GEMINI,
    }
  }

  /// 系统默认 endpoint —— 配置缺省时 fallback。
  pub fn provider_default_endpoint(id: LlmProviderId) -> Option<&'static str> {
    match id {
      LlmProviderId::DeepSeek => Some(DEFAULT_ENDPOINT_DEEPSEEK),
      LlmProviderId::OpenAi => Some(DEFAULT_ENDPOINT_OPENAI),
      // dashscope 走 region 判断，不在此处暴露
      _ => None,
    }
  }
}

/// `from_parts` 家族共用：api_key 非空校验。错误只报字段名，**MUST NOT** 回显值——
/// `LlmError` 的 Display 会进入调用方日志。
fn required_api_key(id: LlmProviderId, api_key: Option<&str>) -> Result<String, LlmError> {
  match api_key {
    Some(k) if !k.is_empty() => Ok(k.to_string()),
    _ => Err(LlmError::ConfigInvalid(id, "api_key required".to_string())),
  }
}

/// `from_parts` 家族共用：`default_chat_model` 空串过滤。空 / 纯空白按「未设置」处理、
/// 回退 provider 默认模型——空模型名从来不是合法配置，在这里失败早于 wire 层且可归因。
fn sanitize_default_model(raw: Option<String>, fallback: &'static str) -> String {
  raw.filter(|m| !m.trim().is_empty()).unwrap_or_else(|| fallback.to_string())
}

/// 中性参数构造 API（`from_parts` 家族）。
///
/// 调用方（上层 bin）只做 proto → 参数的薄映射；region 别名归一 / api_key 非空校验 /
/// 默认模型表回退全部收在这里。**不改 enum variant 形状**——这只是构造路径的收敛，
/// 既有按字段 match 的消费方（驻留判定等）不受影响。
impl LlmProviderConfig {
  /// Qwen(DashScope)。`region` 收原始别名串，归一见 [`parse_dashscope_region`]。
  /// `base_url_override` 仅承载部署形态注入（如集成测试对本地假服务端），凭证数据
  /// MUST NOT 填入——让被驻留判定方自报判据等于让驻留档形同虚设。
  pub fn qwen_from_parts(
    api_key: Option<&str>,
    workspace_id: Option<String>,
    region: Option<&str>,
    default_chat_model: Option<String>,
    timeout: Option<Duration>,
    base_url_override: Option<String>,
  ) -> Result<Self, LlmError> {
    Ok(Self::Qwen {
      api_key: required_api_key(LlmProviderId::Qwen, api_key)?,
      workspace_id,
      region: parse_dashscope_region(region),
      default_chat_model: sanitize_default_model(default_chat_model, DEFAULT_MODEL_QWEN),
      timeout,
      base_url_override,
    })
  }

  pub fn deepseek_from_parts(
    api_key: Option<&str>,
    endpoint: Option<String>,
    default_chat_model: Option<String>,
    timeout: Option<Duration>,
  ) -> Result<Self, LlmError> {
    Ok(Self::DeepSeek {
      api_key: required_api_key(LlmProviderId::DeepSeek, api_key)?,
      endpoint,
      default_chat_model: sanitize_default_model(default_chat_model, DEFAULT_MODEL_DEEPSEEK),
      timeout,
    })
  }

  pub fn openai_from_parts(
    api_key: Option<&str>,
    organization: Option<String>,
    endpoint: Option<String>,
    default_chat_model: Option<String>,
    timeout: Option<Duration>,
  ) -> Result<Self, LlmError> {
    Ok(Self::OpenAi {
      api_key: required_api_key(LlmProviderId::OpenAi, api_key)?,
      organization,
      endpoint,
      default_chat_model: sanitize_default_model(default_chat_model, DEFAULT_MODEL_OPENAI),
      timeout,
    })
  }

  pub fn anthropic_from_parts(api_key: Option<&str>, default_chat_model: Option<String>) -> Result<Self, LlmError> {
    Ok(Self::Anthropic {
      api_key: required_api_key(LlmProviderId::Anthropic, api_key)?,
      default_chat_model: sanitize_default_model(default_chat_model, DEFAULT_MODEL_ANTHROPIC),
    })
  }

  pub fn gemini_from_parts(api_key: Option<&str>, default_chat_model: Option<String>) -> Result<Self, LlmError> {
    Ok(Self::Gemini {
      api_key: required_api_key(LlmProviderId::Gemini, api_key)?,
      default_chat_model: sanitize_default_model(default_chat_model, DEFAULT_MODEL_GEMINI),
    })
  }
}

/// 根据 [`LlmProviderConfig`] 派发到对应 impl。错误均为 [`LlmError::ConfigInvalid`]
/// 子类型，caller 应把错误透传给 ws handshake handler，UI 给出明确报错。
pub fn build_provider(cfg: LlmProviderConfig) -> Result<SharedLlmChatProvider, LlmError> {
  match cfg {
    LlmProviderConfig::Qwen { api_key, workspace_id, region, default_chat_model, timeout, base_url_override } => {
      let p = QwenChatProvider::new(api_key, workspace_id, region, default_chat_model, timeout)?;
      let p = match base_url_override.filter(|s| !s.trim().is_empty()) {
        Some(url) => p.with_base_url(url),
        None => p,
      };
      Ok(Arc::new(p) as Arc<dyn LlmChatProvider>)
    }
    LlmProviderConfig::DeepSeek { api_key, endpoint, default_chat_model, timeout } => {
      let p = DeepSeekChatProvider::new(api_key, endpoint, default_chat_model, timeout)?;
      Ok(Arc::new(p) as Arc<dyn LlmChatProvider>)
    }
    LlmProviderConfig::OpenAi { api_key, organization, endpoint, default_chat_model, timeout } => {
      let p = OpenAiChatProvider::new(api_key, organization, endpoint, default_chat_model, timeout)?;
      Ok(Arc::new(p) as Arc<dyn LlmChatProvider>)
    }
    LlmProviderConfig::Anthropic { api_key, default_chat_model } => {
      let p = AnthropicChatProvider::new(api_key, default_chat_model);
      Ok(Arc::new(p) as Arc<dyn LlmChatProvider>)
    }
    LlmProviderConfig::Gemini { api_key, default_chat_model } => {
      let p = GeminiChatProvider::new(api_key, default_chat_model);
      Ok(Arc::new(p) as Arc<dyn LlmChatProvider>)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn qwen_cfg(base_url_override: Option<String>) -> LlmProviderConfig {
    LlmProviderConfig::Qwen {
      api_key: "sk".into(),
      workspace_id: None,
      region: DashScopeRegion::Beijing,
      default_chat_model: DEFAULT_MODEL_QWEN.into(),
      timeout: None,
      base_url_override,
    }
  }

  #[test]
  fn dispatches_qwen() {
    let p = build_provider(qwen_cfg(None)).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::Qwen);
    assert_eq!(p.default_model(), "qwen3.7-plus");
  }

  #[test]
  fn a_blank_base_url_override_is_the_same_as_none() {
    // 配置文件里留空字符串是最常见的「我没配」写法;把它当成一个真的 base_url 会让
    // transport 拿到空 host,失败点离原因很远。
    for blank in [Some(String::new()), Some("   ".to_string())] {
      let p = build_provider(qwen_cfg(blank)).unwrap();
      assert_eq!(p.provider_id(), LlmProviderId::Qwen);
    }
  }

  #[test]
  fn dispatches_deepseek() {
    let cfg = LlmProviderConfig::DeepSeek {
      api_key: "sk".into(),
      endpoint: None,
      default_chat_model: DEFAULT_MODEL_DEEPSEEK.into(),
      timeout: None,
    };
    let p = build_provider(cfg).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::DeepSeek);
  }

  #[test]
  fn dispatches_openai() {
    let cfg = LlmProviderConfig::OpenAi {
      api_key: "sk".into(),
      organization: None,
      endpoint: None,
      default_chat_model: DEFAULT_MODEL_OPENAI.into(),
      timeout: None,
    };
    let p = build_provider(cfg).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::OpenAi);
  }

  #[test]
  fn dispatches_anthropic_stub() {
    let cfg = LlmProviderConfig::Anthropic { api_key: "sk".into(), default_chat_model: DEFAULT_MODEL_ANTHROPIC.into() };
    let p = build_provider(cfg).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::Anthropic);
  }

  #[test]
  fn dispatches_gemini_stub() {
    let cfg = LlmProviderConfig::Gemini { api_key: "sk".into(), default_chat_model: DEFAULT_MODEL_GEMINI.into() };
    let p = build_provider(cfg).unwrap();
    assert_eq!(p.provider_id(), LlmProviderId::Gemini);
  }

  #[test]
  fn provider_default_model_table() {
    assert_eq!(LlmProviderConfig::provider_default_model(LlmProviderId::Qwen), "qwen3.7-plus");
    assert_eq!(LlmProviderConfig::provider_default_model(LlmProviderId::DeepSeek), "deepseek-v4-flash");
    assert_eq!(LlmProviderConfig::provider_default_model(LlmProviderId::OpenAi), "gpt-4o-mini");
  }

  #[test]
  fn from_parts_builds_each_variant() {
    assert!(matches!(
      LlmProviderConfig::qwen_from_parts(Some("sk"), None, None, None, None, None).unwrap(),
      LlmProviderConfig::Qwen { .. }
    ));
    assert!(matches!(
      LlmProviderConfig::deepseek_from_parts(Some("sk"), None, None, None).unwrap(),
      LlmProviderConfig::DeepSeek { .. }
    ));
    assert!(matches!(
      LlmProviderConfig::openai_from_parts(Some("sk"), None, None, None, None).unwrap(),
      LlmProviderConfig::OpenAi { .. }
    ));
    assert!(matches!(
      LlmProviderConfig::anthropic_from_parts(Some("sk"), None).unwrap(),
      LlmProviderConfig::Anthropic { .. }
    ));
    assert!(matches!(
      LlmProviderConfig::gemini_from_parts(Some("sk"), None).unwrap(),
      LlmProviderConfig::Gemini { .. }
    ));
  }

  #[test]
  fn from_parts_rejects_missing_or_empty_api_key_without_echoing_it() {
    // 与调用方既有口径一致：None / 空串拒绝；不做 trim（whitespace key 的拒绝不是本收敛
    // 裁决的一部分，留给 vendor 端点报错）。
    for bad in [None, Some("")] {
      // `LlmProviderConfig` 刻意无 Debug（持有明文 key），断言走 match 而非 unwrap_err。
      match LlmProviderConfig::deepseek_from_parts(bad, None, None, None) {
        Err(LlmError::ConfigInvalid(id, message)) => {
          assert_eq!(id, LlmProviderId::DeepSeek);
          assert!(message.contains("api_key"), "{message}");
          assert!(!message.contains("sk-"), "错误消息 MUST NOT 回显 key: {message}");
        }
        Err(other) => panic!("expected ConfigInvalid, got {other}"),
        Ok(_) => panic!("expected an error for a missing api_key"),
      }
    }
  }

  /// 行为变化点的锚点：空串 / 纯空白 default_chat_model 按「未设置」处理、回退默认表。
  #[test]
  fn from_parts_filters_blank_default_chat_model_to_the_provider_default() {
    for blank in [None, Some(String::new()), Some("   ".to_string())] {
      let cfg = LlmProviderConfig::qwen_from_parts(Some("sk"), None, None, blank, None, None).unwrap();
      match cfg {
        LlmProviderConfig::Qwen { default_chat_model, .. } => {
          assert_eq!(default_chat_model, DEFAULT_MODEL_QWEN, "blank model must fall back to the default");
        }
        _ => panic!("expected the Qwen variant"),
      }
    }
    let cfg =
      LlmProviderConfig::deepseek_from_parts(Some("sk"), None, Some("deepseek-reasoner".into()), None).unwrap();
    match cfg {
      LlmProviderConfig::DeepSeek { default_chat_model, .. } => assert_eq!(default_chat_model, "deepseek-reasoner"),
      _ => panic!("expected the DeepSeek variant"),
    }
  }

  #[test]
  fn from_parts_normalizes_dashscope_region_aliases() {
    let region_of = |raw: Option<&str>| match LlmProviderConfig::qwen_from_parts(Some("sk"), None, raw, None, None, None) {
      Ok(LlmProviderConfig::Qwen { region, .. }) => region,
      Ok(_) => panic!("expected the Qwen variant"),
      Err(e) => panic!("unexpected error: {e}"),
    };
    assert_eq!(region_of(None), DashScopeRegion::Beijing);
    assert_eq!(region_of(Some("garbage")), DashScopeRegion::Beijing);
    assert_eq!(region_of(Some("Singapore")), DashScopeRegion::Singapore);
    assert_eq!(region_of(Some("intl")), DashScopeRegion::Singapore);
  }
}
