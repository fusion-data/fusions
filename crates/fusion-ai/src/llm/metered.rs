//! Platform 模型 token 计量捕获缝 —— `MeteredLlmProvider` 装饰器 + `AiUsageSink` trait。
//!
//! 设计真相源：`docs/designs/ai/platform-model-token-metering.md` §3.2。
//!
//! ## 为什么是装饰器（不是 per-extractor）
//!
//! voice NLU（`LlmNluExtractor`）与 AIH（`LlmHealthAnalyzer`）是**同构** LLM 调用点：
//! 均持 [`LlmChatProvider`] + 调 `chat_complete`，均经 [`super::build_provider`] 构造。
//! 在 `build_provider` 输出处包一层 [`MeteredLlmProvider`] 一处覆盖所有调用方（含未来
//! summary / rag），DRY 且不漏计已 live 的 AIH（plan-eng-review D-捕获层）。
//!
//! ## 捕获语义
//!
//! - `chat_complete` 透传 inner；`Ok(resp)` 且 `resp.usage` 有值 → 戳 `occurred_at`
//!   = `Utc::now()`（捕获时刻，非 DB 写时）+ `record()`。
//! - `Err`（无 resp，请求失败未烧 token）→ **不记**。
//! - `Ok` 但 `usage=None`（vendor 未回 usage）→ **不记**（无可计量事实）。
//! - 捕获在 `chat_complete` 之外的装饰层，`AiUsageSink::record` 非阻塞 enqueue，
//!   **绝不**阻塞实时语音热路径、**绝不**向 caller propagate 写错误。
//!
//! outcome 细分（success vs no_tool_call-but-burned）由 caller 视角决定；装饰层只知
//! `chat_complete` 成功与否，本期一律记 [`Outcome::Success`]（设计 §8 开放，可后补回标）。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{ChatCompletionRequest, ChatCompletionResponse, LlmChatProvider, LlmError, LlmProviderId, TokenUsage};

/// 调用结局 —— 与 `ai_model_usage_events.outcome` 列 CHECK 约定的字符串一一对应。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Outcome {
  /// `chat_complete` 成功且产出可用。
  Success,
  /// 成功但结果模糊（如 no_tool_call-but-burned）—— 本期装饰层不区分，预留给 caller 回标。
  Ambiguous,
  /// 调用出错（装饰层不记 Err，此变体留给 caller 显式回标场景）。
  Error,
}

impl Outcome {
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Success => "success",
      Self::Ambiguous => "ambiguous",
      Self::Error => "error",
    }
  }
}

/// 命中层级 —— 与 `ai_model_usage_events.matched_scope` 列 CHECK 约定的字符串一一对应
/// （`facility｜tenant｜platform｜system_default`）。
///
/// AI feature route 四层解析的判定结果（enum-type-mapping-conventions.md R5：低基数 flag
/// 用 inline CHECK，Rust 侧用 domain enum 而非裸 String）。`matched_scope` 在 `ResolvedRoute`
/// 边界（voice / AIH 的 `resolve_*_provider`）由 [`Self::from_route_str`] 从上游字符串收敛：
/// 未知值 / 上游缺省（`""`）→ 明确 [`Self::SystemDefault`]，杜绝 `""` 撞 CHECK 静默丢账。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MatchedScope {
  Facility,
  Tenant,
  Platform,
  /// 编译期默认 / fail-open / 上游未填 —— 收敛兜底值。
  SystemDefault,
}

impl MatchedScope {
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Facility => "facility",
      Self::Tenant => "tenant",
      Self::Platform => "platform",
      Self::SystemDefault => "system_default",
    }
  }

  /// 从上游 `ResolvedRoute.matched_scope` 字符串收敛到枚举。未知值 / 空串
  /// （上游缺省，理论不发生——hylx-access 对四层均填充）→ [`Self::SystemDefault`]，
  /// 防 `""` / typo 撞 `matched_scope` CHECK 静默丢账（enum 偏差修复）。
  pub fn from_route_str(raw: &str) -> Self {
    match raw {
      "facility" => Self::Facility,
      "tenant" => Self::Tenant,
      "platform" => Self::Platform,
      _ => Self::SystemDefault,
    }
  }
}

/// 计量上下文 —— 在 `build_provider` 处按调用方组装（session-start snapshot）。
///
/// `credential_id` 仅内部成本归集用，**永不**进任何租户读面投影（设计 §4.2）。
#[derive(Debug, Clone)]
pub struct AiUsageCtx {
  pub tenant_id: i64,
  pub facility_id: Option<Uuid>,
  /// AI feature 路由 feature_code（`'nlu'` | `'health_risk'` | `'summary'` | ...）。
  pub feature_code: String,
  /// 命中层级（domain enum）。fail-open / 上游缺省由 [`MatchedScope::from_route_str`]
  /// 在 `ResolvedRoute` 边界收敛到 [`MatchedScope::SystemDefault`]。
  pub matched_scope: MatchedScope,
  pub provider: String,
  pub model: String,
  pub credential_id: Option<Uuid>,
  /// voice session id；AIH 无会话则 `None`。
  pub session_id: Option<Uuid>,
  /// 调用种类（`'voice_nlu'` | `'health_assess'`）。
  pub request_kind: String,
}

/// 一条模型 token 计量事件 —— 落 `ai_model_usage_events`（hylx_ai 库）。
///
/// `occurred_at` 由事件携带（捕获时戳），token 用 `i64`（bind BIGINT，`u32 → i64` 无损上转）。
#[derive(Debug, Clone)]
pub struct AiUsageEvent {
  pub occurred_at: DateTime<Utc>,
  pub tenant_id: i64,
  pub facility_id: Option<Uuid>,
  pub feature_code: String,
  pub provider: String,
  pub model: String,
  pub matched_scope: MatchedScope,
  pub outcome: Outcome,
  pub credential_id: Option<Uuid>,
  pub prompt_tokens: i64,
  pub completion_tokens: i64,
  pub total_tokens: i64,
  pub session_id: Option<Uuid>,
  pub request_kind: String,
  pub latency_ms: Option<i64>,
}

impl AiUsageEvent {
  /// 由 ctx snapshot + 本次 usage + outcome 组装事件。`occurred_at` 由 caller 传入
  /// （捕获时 `Utc::now()`），`latency_ms` 为 `chat_complete` 耗时（可空）。
  pub fn from_ctx_usage(
    ctx: &AiUsageCtx,
    usage: &TokenUsage,
    outcome: Outcome,
    occurred_at: DateTime<Utc>,
    latency_ms: Option<i64>,
  ) -> Self {
    Self {
      occurred_at,
      tenant_id: ctx.tenant_id,
      facility_id: ctx.facility_id,
      feature_code: ctx.feature_code.clone(),
      provider: ctx.provider.clone(),
      model: ctx.model.clone(),
      matched_scope: ctx.matched_scope,
      outcome,
      credential_id: ctx.credential_id,
      prompt_tokens: i64::from(usage.prompt_tokens),
      completion_tokens: i64::from(usage.completion_tokens),
      total_tokens: i64::from(usage.total_tokens),
      session_id: ctx.session_id,
      request_kind: ctx.request_kind.clone(),
      latency_ms,
    }
  }
}

/// 持久化缝 —— impl 在 hylx-ai infra（`PgUsageSink`），共享层只定 trait（backend-layering）。
///
/// `record` MUST 非阻塞 enqueue，**绝不**向 caller propagate 写错误（计量失败不能拖垮业务）。
pub trait AiUsageSink: Send + Sync {
  fn record(&self, ev: AiUsageEvent);
}

/// gate 关时装它，零行为。捕获装饰**常驻**，行为由 sink 切换（无需改业务码）。
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopUsageSink;

impl AiUsageSink for NoopUsageSink {
  fn record(&self, _ev: AiUsageEvent) {}
}

/// [`LlmChatProvider`] 计量装饰器 —— 与 inner 透明同型，业务侧无感知。
pub struct MeteredLlmProvider {
  inner: Arc<dyn LlmChatProvider>,
  ctx: AiUsageCtx,
  sink: Arc<dyn AiUsageSink>,
}

impl MeteredLlmProvider {
  pub fn new(inner: Arc<dyn LlmChatProvider>, ctx: AiUsageCtx, sink: Arc<dyn AiUsageSink>) -> Self {
    Self { inner, ctx, sink }
  }
}

#[async_trait]
impl LlmChatProvider for MeteredLlmProvider {
  fn provider_id(&self) -> LlmProviderId {
    self.inner.provider_id()
  }

  fn default_model(&self) -> &str {
    self.inner.default_model()
  }

  async fn chat_complete(&self, req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError> {
    let started = std::time::Instant::now();
    let resp = self.inner.chat_complete(req).await;
    // Ok 且有 usage 才记；Err（无 resp）/ usage=None → 不记。
    if let Ok(r) = &resp
      && let Some(u) = &r.usage
    {
      let latency_ms = i64::try_from(started.elapsed().as_millis()).ok();
      let ev = AiUsageEvent::from_ctx_usage(&self.ctx, u, Outcome::Success, Utc::now(), latency_ms);
      self.sink.record(ev);
    }
    resp
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use super::*;
  use crate::llm::{ChatMessage, ChatRole};

  /// 收集 record 的测试 sink。
  struct RecordingSink {
    events: Mutex<Vec<AiUsageEvent>>,
  }
  impl RecordingSink {
    fn new() -> Arc<Self> {
      Arc::new(Self { events: Mutex::new(Vec::new()) })
    }
    fn count(&self) -> usize {
      self.events.lock().unwrap().len()
    }
  }
  impl AiUsageSink for RecordingSink {
    fn record(&self, ev: AiUsageEvent) {
      self.events.lock().unwrap().push(ev);
    }
  }

  /// 单次返回固定结果的 mock provider。
  struct MockProvider {
    resp: Mutex<Option<Result<ChatCompletionResponse, LlmError>>>,
  }
  impl MockProvider {
    fn ok(usage: Option<TokenUsage>) -> Arc<Self> {
      let r = ChatCompletionResponse {
        model: "mock-model".into(),
        message: ChatMessage { role: ChatRole::Assistant, content: "ok".into(), tool_calls: vec![] },
        usage,
        provider_metadata: serde_json::json!({}),
      };
      Arc::new(Self { resp: Mutex::new(Some(Ok(r))) })
    }
    fn err() -> Arc<Self> {
      Arc::new(Self {
        resp: Mutex::new(Some(Err(LlmError::Transport { provider: LlmProviderId::DeepSeek, message: "boom".into() }))),
      })
    }
  }
  #[async_trait]
  impl LlmChatProvider for MockProvider {
    fn provider_id(&self) -> LlmProviderId {
      LlmProviderId::DeepSeek
    }
    fn default_model(&self) -> &str {
      "mock-model"
    }
    async fn chat_complete(&self, _req: ChatCompletionRequest) -> Result<ChatCompletionResponse, LlmError> {
      self.resp.lock().unwrap().take().expect("consumed twice")
    }
  }

  fn ctx(matched_scope: MatchedScope) -> AiUsageCtx {
    AiUsageCtx {
      tenant_id: 2,
      facility_id: Some(Uuid::now_v7()),
      feature_code: "nlu".into(),
      matched_scope,
      provider: "deepseek".into(),
      model: "deepseek-v4-flash".into(),
      credential_id: Some(Uuid::now_v7()),
      session_id: Some(Uuid::now_v7()),
      request_kind: "voice_nlu".into(),
    }
  }

  #[test]
  fn matched_scope_from_route_str_converges_unknown_and_empty_to_system_default() {
    assert_eq!(MatchedScope::from_route_str("facility"), MatchedScope::Facility);
    assert_eq!(MatchedScope::from_route_str("tenant"), MatchedScope::Tenant);
    assert_eq!(MatchedScope::from_route_str("platform"), MatchedScope::Platform);
    assert_eq!(MatchedScope::from_route_str("system_default"), MatchedScope::SystemDefault);
    // 空串（上游缺省）/ 未知值 → SystemDefault，杜绝 "" 撞 CHECK。
    assert_eq!(MatchedScope::from_route_str(""), MatchedScope::SystemDefault);
    assert_eq!(MatchedScope::from_route_str("bogus"), MatchedScope::SystemDefault);
    // round-trip：as_str 的输出经 from_route_str 还原同值。
    for s in [MatchedScope::Facility, MatchedScope::Tenant, MatchedScope::Platform, MatchedScope::SystemDefault] {
      assert_eq!(MatchedScope::from_route_str(s.as_str()), s);
    }
  }

  #[tokio::test]
  async fn records_on_ok_with_usage() {
    let sink = RecordingSink::new();
    let inner = MockProvider::ok(Some(TokenUsage { prompt_tokens: 120, completion_tokens: 30, total_tokens: 150 }));
    let metered = MeteredLlmProvider::new(inner, ctx(MatchedScope::Platform), sink.clone());
    let out = metered.chat_complete(ChatCompletionRequest::default()).await;
    assert!(out.is_ok());
    assert_eq!(sink.count(), 1);
    let ev = &sink.events.lock().unwrap()[0];
    assert_eq!(ev.matched_scope, MatchedScope::Platform);
    assert_eq!(ev.prompt_tokens, 120);
    assert_eq!(ev.completion_tokens, 30);
    assert_eq!(ev.total_tokens, 150);
    assert_eq!(ev.outcome, Outcome::Success);
    assert_eq!(ev.feature_code, "nlu");
    assert_eq!(ev.request_kind, "voice_nlu");
  }

  #[tokio::test]
  async fn no_record_on_ok_without_usage() {
    let sink = RecordingSink::new();
    let inner = MockProvider::ok(None);
    let metered = MeteredLlmProvider::new(inner, ctx(MatchedScope::Tenant), sink.clone());
    let _ = metered.chat_complete(ChatCompletionRequest::default()).await;
    assert_eq!(sink.count(), 0, "usage=None MUST NOT 记一条");
  }

  #[tokio::test]
  async fn no_record_on_err() {
    let sink = RecordingSink::new();
    let inner = MockProvider::err();
    let metered = MeteredLlmProvider::new(inner, ctx(MatchedScope::Platform), sink.clone());
    let out = metered.chat_complete(ChatCompletionRequest::default()).await;
    assert!(out.is_err());
    assert_eq!(sink.count(), 0, "Err（无 resp）MUST NOT 记一条");
  }

  #[tokio::test]
  async fn noop_sink_is_silent() {
    let inner = MockProvider::ok(Some(TokenUsage { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 }));
    let metered = MeteredLlmProvider::new(inner, ctx(MatchedScope::SystemDefault), Arc::new(NoopUsageSink));
    // NoopUsageSink 不 panic、不副作用，仅透传 chat_complete。
    assert!(metered.chat_complete(ChatCompletionRequest::default()).await.is_ok());
  }

  #[tokio::test]
  async fn delegates_provider_id_and_default_model() {
    let inner = MockProvider::ok(None);
    let metered = MeteredLlmProvider::new(inner, ctx(MatchedScope::Platform), Arc::new(NoopUsageSink));
    assert_eq!(metered.provider_id(), LlmProviderId::DeepSeek);
    assert_eq!(metered.default_model(), "mock-model");
  }
}
