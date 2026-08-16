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
use crate::providers::dashscope::DashScopeRegion;

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

  /// Qwen（DashScope）命名构造器 —— 中性参数面（fusion-ai-de-rig.md §4.4 #1）。
  ///
  /// region 字符串别名归一经 [`parse_dashscope_region`]；api_key 非空校验；
  /// `default_chat_model` 缺省回退系统默认表。bin 侧 proto 字段映射后走这里，
  /// MUST NOT 各自维护别名表副本。
  pub fn qwen_from_parts(
    api_key: &str,
    workspace_id: Option<String>,
    region: Option<&str>,
    default_chat_model: Option<String>,
    timeout: Option<Duration>,
    base_url_override: Option<String>,
  ) -> Result<Self, LlmError> {
    require_api_key(LlmProviderId::Qwen, api_key)?;
    Ok(Self::Qwen {
      api_key: api_key.to_string(),
      workspace_id,
      region: parse_dashscope_region(region),
      default_chat_model: default_chat_model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_QWEN.to_string()),
      timeout,
      base_url_override,
    })
  }

  /// DeepSeek 命名构造器（中性参数面，§4.4 #1）。
  pub fn deepseek_from_parts(
    api_key: &str,
    endpoint: Option<String>,
    default_chat_model: Option<String>,
    timeout: Option<Duration>,
  ) -> Result<Self, LlmError> {
    require_api_key(LlmProviderId::DeepSeek, api_key)?;
    Ok(Self::DeepSeek {
      api_key: api_key.to_string(),
      endpoint,
      default_chat_model: default_chat_model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_DEEPSEEK.to_string()),
      timeout,
    })
  }

  /// OpenAI 命名构造器（中性参数面，§4.4 #1）。
  pub fn openai_from_parts(
    api_key: &str,
    organization: Option<String>,
    endpoint: Option<String>,
    default_chat_model: Option<String>,
    timeout: Option<Duration>,
  ) -> Result<Self, LlmError> {
    require_api_key(LlmProviderId::OpenAi, api_key)?;
    Ok(Self::OpenAi {
      api_key: api_key.to_string(),
      organization,
      endpoint,
      default_chat_model: default_chat_model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_OPENAI.to_string()),
      timeout,
    })
  }

  /// Anthropic 命名构造器（中性参数面，§4.4 #1）。
  pub fn anthropic_from_parts(api_key: &str, default_chat_model: Option<String>) -> Result<Self, LlmError> {
    require_api_key(LlmProviderId::Anthropic, api_key)?;
    Ok(Self::Anthropic {
      api_key: api_key.to_string(),
      default_chat_model: default_chat_model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_ANTHROPIC.to_string()),
    })
  }

  /// Gemini 命名构造器（中性参数面，§4.4 #1）。
  pub fn gemini_from_parts(api_key: &str, default_chat_model: Option<String>) -> Result<Self, LlmError> {
    require_api_key(LlmProviderId::Gemini, api_key)?;
    Ok(Self::Gemini {
      api_key: api_key.to_string(),
      default_chat_model: default_chat_model
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL_GEMINI.to_string()),
    })
  }
}

/// DashScope 区域字符串解析归一 —— 唯一真相源（fusion-ai-de-rig.md §4.4 #1）。
///
/// 别名表：`singapore` / `intl`（大小写不敏感）→ Singapore；其余（含 None / 未知值）
/// → Beijing。消费方 MUST NOT 自带别名副本。
pub fn parse_dashscope_region(region: Option<&str>) -> DashScopeRegion {
  match region.map(|x| x.to_ascii_lowercase()).as_deref() {
    Some("singapore" | "intl") => DashScopeRegion::Singapore,
    _ => DashScopeRegion::Beijing,
  }
}

/// api_key 非空校验。错误只报字段名，MUST NOT 回显值（该错误会进日志）。
fn require_api_key(provider: LlmProviderId, key: &str) -> Result<(), LlmError> {
  if key.trim().is_empty() { Err(LlmError::ConfigInvalid(provider, "api_key required".to_string())) } else { Ok(()) }
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
  fn qwen_from_parts_normalizes_region_and_model() {
    // 别名归一：singapore / intl（大小写不敏感）→ Singapore；None / 未知 → Beijing
    let cfg = LlmProviderConfig::qwen_from_parts("sk-x", None, Some("INTL"), None, None, None).unwrap();
    assert!(
      matches!(cfg, LlmProviderConfig::Qwen { region: DashScopeRegion::Singapore, ref default_chat_model, .. } if default_chat_model == "qwen3.7-plus")
    );

    let cfg = LlmProviderConfig::qwen_from_parts("sk-x", None, Some("garbage"), Some("  ".into()), None, None).unwrap();
    assert!(
      matches!(cfg, LlmProviderConfig::Qwen { region: DashScopeRegion::Beijing, ref default_chat_model, .. } if default_chat_model == "qwen3.7-plus")
    );

    let cfg = LlmProviderConfig::qwen_from_parts("sk-x", None, None, Some("qwen3-max".into()), None, None).unwrap();
    assert!(matches!(cfg, LlmProviderConfig::Qwen { ref default_chat_model, .. } if default_chat_model == "qwen3-max"));
  }

  #[test]
  fn from_parts_rejects_blank_api_key_without_echoing_it() {
    // LlmProviderConfig 刻意无 Debug（携密），用 match 提取错误而非 unwrap_err
    for blank in ["", "   "] {
      let err = match LlmProviderConfig::qwen_from_parts(blank, None, None, None, None, None) {
        Err(e) => e,
        Ok(_) => panic!("blank api_key must be rejected"),
      };
      let msg = err.to_string();
      assert!(msg.contains("api_key required"), "got: {msg}");
      if !blank.trim().is_empty() {
        assert!(!msg.contains(blank.trim()), "error must not echo the key value");
      }
    }
    assert!(LlmProviderConfig::deepseek_from_parts("", None, None, None).is_err());
    assert!(LlmProviderConfig::openai_from_parts("", None, None, None, None).is_err());
    assert!(LlmProviderConfig::anthropic_from_parts("", None).is_err());
    assert!(LlmProviderConfig::gemini_from_parts("", None).is_err());
  }

  #[test]
  fn parse_dashscope_region_alias_table() {
    assert_eq!(parse_dashscope_region(None), DashScopeRegion::Beijing);
    assert_eq!(parse_dashscope_region(Some("singapore")), DashScopeRegion::Singapore);
    assert_eq!(parse_dashscope_region(Some("Singapore")), DashScopeRegion::Singapore);
    assert_eq!(parse_dashscope_region(Some("INTL")), DashScopeRegion::Singapore);
    assert_eq!(parse_dashscope_region(Some("intl")), DashScopeRegion::Singapore);
    assert_eq!(parse_dashscope_region(Some("garbage")), DashScopeRegion::Beijing);
    assert_eq!(parse_dashscope_region(Some("")), DashScopeRegion::Beijing);
  }

  #[test]
  fn deepseek_from_parts_fills_defaults() {
    let cfg = LlmProviderConfig::deepseek_from_parts("sk-x", None, None, None).unwrap();
    assert!(
      matches!(cfg, LlmProviderConfig::DeepSeek { ref default_chat_model, .. } if default_chat_model == "deepseek-v4-flash")
    );
  }
}
