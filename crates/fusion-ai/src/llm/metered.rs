//! Platform 模型 token 计量捕获缝 —— `MeteredLlmProvider` 装饰器 + `AiUsageSink` trait。
//!
//! ## 为什么是装饰器（不是 per-caller）
//!
//! 所有 LLM 调用点都是**同构**的：均持 [`LlmChatProvider`] + 调 `chat_complete`，均经
//! [`super::build_provider`] 构造。在 `build_provider` 输出处包一层 [`MeteredLlmProvider`]
//! 即一处覆盖全部调用方，DRY 且不会漏计新增调用点。
//!
//! ## 捕获语义
//!
//! - `chat_complete` 透传 inner；`Ok(resp)` 且 `resp.usage` 有值 → 戳 `occurred_at`
//!   = `Utc::now()`（捕获时刻，非 DB 写时）+ `record()`。
//! - `Err`（无 resp，请求失败未烧 token）→ **不记**。
//! - `Ok` 但 `usage=None`（vendor 未回 usage）→ **不记**（无可计量事实）。
//! - 捕获在 `chat_complete` 之外的装饰层，`AiUsageSink::record` 非阻塞 enqueue，
//!   **绝不**阻塞业务热路径、**绝不**向 caller propagate 写错误。
//!
//! ## 消费方边界
//!
//! 本模块只定义**捕获缝**：装饰器 + 事件类型 + sink trait。持久化实现（`PgUsageSink`）、
//! 路由策略、凭证真相源、用量读面与计费均属消费方应用，不进本仓
//! （见 `docs/exec-plans/deferred/fusion-ai-model-gateway-core.md` 的边界声明）。
//!
//! outcome 细分（success vs 成功但结果模糊）由 caller 视角决定；装饰层只知
//! `chat_complete` 成功与否，本期一律记 [`Outcome::Success`]（可后补回标）。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{ChatCompletionRequest, ChatCompletionResponse, LlmChatProvider, LlmError, LlmProviderId, TokenUsage};

/// 调用结局 —— 与 `ai_model_usage_events.outcome` 列 CHECK 约定的字符串一一对应。
///
/// DB 列为 SMALLINT（形态 C 自治编号：success=1/ambiguous=2/error=3）。`as_str` 保留为 wire/审计
/// 串边界，DB 写路径用 [`Self::as_i16`]。
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

  /// DB 自治编号（形态 C：success=1/ambiguous=2/error=3）—— `ai_model_usage_events.outcome` 列。
  pub const fn as_i16(self) -> i16 {
    match self {
      Self::Success => 1,
      Self::Ambiguous => 2,
      Self::Error => 3,
    }
  }

  /// [`Self::as_i16`] 的逆。未知值 → [`Self::Error`]（fail-closed，不静默丢账）。
  pub const fn from_i16(value: i16) -> Self {
    match value {
      1 => Self::Success,
      2 => Self::Ambiguous,
      _ => Self::Error,
    }
  }
}

/// 命中层级 —— 与 `ai_model_usage_events.matched_scope` 列 CHECK 约定的字符串一一对应
/// （`facility｜tenant｜platform｜system_default`）。
///
/// AI feature route 四层解析的判定结果（enum-type-mapping-conventions.md R5：低基数 flag
/// 用 inline CHECK，Rust 侧用 domain enum 而非裸 String）。`matched_scope` 在 `ResolvedRoute`
/// 边界由 [`Self::from_route_str`] 从上游字符串收敛：未知值 / 上游缺省（`""`）→ 明确
/// [`Self::SystemDefault`]，杜绝 `""` 撞 CHECK 静默丢账。
///
/// [`Self::Dimension`] 是业务声明的 scope 维度（原 facility 专用层的泛化）：路由表以
/// `scope_dimensions` 键值对表达作用域，命中非空维度集的行即属该层。
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MatchedScope {
  Dimension,
  Tenant,
  Platform,
  /// 编译期默认 / fail-open / 上游未填 —— 收敛兜底值。
  SystemDefault,
}

impl MatchedScope {
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Dimension => "dimension",
      Self::Tenant => "tenant",
      Self::Platform => "platform",
      Self::SystemDefault => "system_default",
    }
  }

  /// DB 自治编号（形态 C：dimension=1/tenant=2/platform=3/system_default=4）——
  /// `ai_model_usage_events.matched_scope` 列。
  pub const fn as_i16(self) -> i16 {
    match self {
      Self::Dimension => 1,
      Self::Tenant => 2,
      Self::Platform => 3,
      Self::SystemDefault => 4,
    }
  }

  /// [`Self::as_i16`] 的逆。未知值 → [`Self::SystemDefault`]（兜底，与 `from_route_str` 同纪律）。
  pub const fn from_i16(value: i16) -> Self {
    match value {
      1 => Self::Dimension,
      2 => Self::Tenant,
      3 => Self::Platform,
      _ => Self::SystemDefault,
    }
  }

  /// 从上游 `ResolvedRoute.matched_scope` 字符串收敛到枚举。未知值 / 空串
  /// （上游缺省，理论不发生——路由服务对四层均填充）→ [`Self::SystemDefault`]，
  /// 防 `""` / typo 撞 `matched_scope` CHECK 静默丢账（enum 偏差修复）。
  pub fn from_route_str(raw: &str) -> Self {
    match raw {
      "dimension" => Self::Dimension,
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
  /// 调用发生时的业务维度上下文（键 → 当前值），空 map = 租户级调用。
  /// 泛化自原先单一的 `facility_id`：维度键集由消费方业务声明，共享层不认识具体键名。
  pub dimensions: BTreeMap<String, String>,
  /// AI feature 路由 feature_code（`'chat'` | `'summary'` | `'extract'` | ...）。
  pub feature_code: String,
  /// 命中层级（domain enum）。fail-open / 上游缺省由 [`MatchedScope::from_route_str`]
  /// 在 `ResolvedRoute` 边界收敛到 [`MatchedScope::SystemDefault`]。
  pub matched_scope: MatchedScope,
  pub provider: String,
  pub model: String,
  pub credential_id: Option<Uuid>,
  /// 会话 id；无会话语义的调用则 `None`。
  pub session_id: Option<Uuid>,
  /// 调用种类（`'ai_chat'` | ...）。
  pub request_kind: String,
  /// 本次调用**实际**落地的凭证区域（`'singapore'` | `'beijing'`）；`None` = 该 provider
  /// 无区域概念（区域由 vendor 端点自身决定，本层无从标注）。
  ///
  /// 合规「数据存放地」清单以此为标注来源，故取值 MUST 来自调用完成后已知的真实区域，
  /// MUST NOT 取自路由配置表的自报字段——凭证 config 整体加密，路由层拿不到明文 region。
  pub resolved_region: Option<String>,
}

/// 一条模型计量事件 —— 落 `ai_model_usage_events`（消费方业务库）。
///
/// `occurred_at` 由事件携带（捕获时戳），token 用 `i64`（bind BIGINT，`u32 → i64` 无损上转）。
///
/// `#[non_exhaustive]`：本结构会随新模态增补字段（`audio_duration_ms` 即是一例），
/// 构造 MUST 经 [`Self::from_ctx_tokens`] / [`Self::from_ctx_audio`],而不是字面量 ——
/// 否则每加一个模态都是一次下游破坏性变更。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct AiUsageEvent {
  pub occurred_at: DateTime<Utc>,
  pub tenant_id: i64,
  /// 调用发生时的业务维度上下文快照（空 map = 租户级调用）。
  pub dimensions: BTreeMap<String, String>,
  pub feature_code: String,
  pub provider: String,
  pub model: String,
  pub matched_scope: MatchedScope,
  pub outcome: Outcome,
  pub credential_id: Option<Uuid>,
  pub prompt_tokens: i64,
  pub completion_tokens: i64,
  pub total_tokens: i64,
  /// cache 命中 input tokens —— `Option` 即 DB 列 NULL 语义（`cached_input_tokens`
  /// BIGINT NULL：NULL = 未拆分/不适用，0 = 真零命中，类型层可区分）：
  /// - chat（[`Self::from_ctx_tokens`]）：恒 `Some`（wire 皆无 cache 字段 → `Some(0)`）；
  /// - STT（[`Self::from_ctx_audio`]）：恒 `None` —— STT 无 cache 维度，不拆分。
  pub cached_input_tokens: Option<i64>,
  pub session_id: Option<Uuid>,
  pub request_kind: String,
  pub latency_ms: Option<i64>,
  /// 音频时长（ms）—— **STT 的可计费维度**，token 三列对 STT 恒为 0。
  ///
  /// chat 调用恒为 `None`。provider 未回时长时也为 `None`：计量是 best-effort，
  /// MUST NOT 为了「有个数」而编造一个值。
  pub audio_duration_ms: Option<i64>,
  /// 实际落地的凭证区域，chat 与 STT 共用。语义见 [`AiUsageCtx::resolved_region`]。
  pub resolved_region: Option<String>,
}

impl AiUsageEvent {
  /// 由 ctx snapshot + 本次 token usage + outcome 组装 **chat** 事件。`occurred_at` 由 caller
  /// 传入（捕获时 `Utc::now()`），`latency_ms` 为 `chat_complete` 耗时（可空）。
  ///
  /// 名字点明模态:早先的通用名 `from_ctx_usage` 会让 STT 路径误用它 —— 那会静默丢掉音频时长,
  /// 也就是该模态**唯一**的可计费维度。
  pub fn from_ctx_tokens(
    ctx: &AiUsageCtx,
    usage: &TokenUsage,
    outcome: Outcome,
    occurred_at: DateTime<Utc>,
    latency_ms: Option<i64>,
  ) -> Self {
    Self {
      occurred_at,
      tenant_id: ctx.tenant_id,
      dimensions: ctx.dimensions.clone(),
      feature_code: ctx.feature_code.clone(),
      provider: ctx.provider.clone(),
      model: ctx.model.clone(),
      matched_scope: ctx.matched_scope,
      outcome,
      credential_id: ctx.credential_id,
      prompt_tokens: i64::from(usage.prompt_tokens),
      completion_tokens: i64::from(usage.completion_tokens),
      total_tokens: i64::from(usage.total_tokens),
      cached_input_tokens: Some(i64::from(usage.cached_input_tokens)),
      session_id: ctx.session_id,
      request_kind: ctx.request_kind.clone(),
      latency_ms,
      audio_duration_ms: None,
      resolved_region: ctx.resolved_region.clone(),
    }
  }

  /// 由 ctx snapshot + 音频时长组装 **STT** 事件。token 三列记 0——不是「零 token」的断言，
  /// 而是「本模态不以 token 计量」的表达；读侧按 `request_kind` 区分即可。
  ///
  /// 一次识别会话记**一行**：按分段或按帧拆行会把一次调用的成本碎成不可归并的片段。
  ///
  /// `audio_duration_ms` 是 `i64` 而非 `Option`：一条既无 token 又无时长的行没有任何意义,
  /// 也与「provider 没回时长」不可区分。provider 未回时长时 MUST 不记这一行,而不是记一行空的。
  pub fn from_ctx_audio(
    ctx: &AiUsageCtx,
    audio_duration_ms: i64,
    outcome: Outcome,
    occurred_at: DateTime<Utc>,
    latency_ms: Option<i64>,
  ) -> Self {
    Self {
      occurred_at,
      tenant_id: ctx.tenant_id,
      dimensions: ctx.dimensions.clone(),
      feature_code: ctx.feature_code.clone(),
      provider: ctx.provider.clone(),
      model: ctx.model.clone(),
      matched_scope: ctx.matched_scope,
      outcome,
      credential_id: ctx.credential_id,
      prompt_tokens: 0,
      completion_tokens: 0,
      total_tokens: 0,
      // STT 无 cache 维度（计量维度是 audio_duration_ms）—— None = 未拆分，非「零命中」。
      cached_input_tokens: None,
      session_id: ctx.session_id,
      request_kind: ctx.request_kind.clone(),
      latency_ms,
      audio_duration_ms: Some(audio_duration_ms),
      resolved_region: ctx.resolved_region.clone(),
    }
  }
}

/// 持久化缝 —— impl 在消费方 AI infra（如 `PgUsageSink`），共享层只定 trait（backend-layering）。
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
      let ev = AiUsageEvent::from_ctx_tokens(&self.ctx, u, Outcome::Success, Utc::now(), latency_ms);
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
      dimensions: BTreeMap::from([("region".to_string(), "sg".to_string())]),
      feature_code: "chat".into(),
      matched_scope,
      provider: "deepseek".into(),
      model: "deepseek-v4-flash".into(),
      credential_id: Some(Uuid::now_v7()),
      session_id: Some(Uuid::now_v7()),
      request_kind: "ai_chat".into(),
      // deepseek 无区域概念——`None` 是「该 provider 不可标注」，不是「未校验」。
      resolved_region: None,
    }
  }

  #[test]
  fn matched_scope_from_route_str_converges_unknown_and_empty_to_system_default() {
    assert_eq!(MatchedScope::from_route_str("dimension"), MatchedScope::Dimension);
    assert_eq!(MatchedScope::from_route_str("tenant"), MatchedScope::Tenant);
    assert_eq!(MatchedScope::from_route_str("platform"), MatchedScope::Platform);
    assert_eq!(MatchedScope::from_route_str("system_default"), MatchedScope::SystemDefault);
    // 空串（上游缺省）/ 未知值 → SystemDefault，杜绝 "" 撞 CHECK。
    assert_eq!(MatchedScope::from_route_str(""), MatchedScope::SystemDefault);
    assert_eq!(MatchedScope::from_route_str("bogus"), MatchedScope::SystemDefault);
    // 旧的 facility 层名已泛化为 dimension；残留调用方传 "facility" 时收敛到兜底值，
    // 而非静默当作维度层记账。
    assert_eq!(MatchedScope::from_route_str("facility"), MatchedScope::SystemDefault);
    // round-trip：as_str 的输出经 from_route_str 还原同值。
    for s in [MatchedScope::Dimension, MatchedScope::Tenant, MatchedScope::Platform, MatchedScope::SystemDefault] {
      assert_eq!(MatchedScope::from_route_str(s.as_str()), s);
    }
  }

  /// `matched_scope` / `outcome` DB 列已迁至 SMALLINT（形态 C 自治编号）。as_i16 的输出必须
  /// 与 CHECK 约定的值一一对应，from_i16 必须 round-trip。
  #[test]
  fn matched_scope_and_outcome_i16_round_trip_matches_the_check_constraint() {
    assert_eq!(MatchedScope::Dimension.as_i16(), 1);
    assert_eq!(MatchedScope::Tenant.as_i16(), 2);
    assert_eq!(MatchedScope::Platform.as_i16(), 3);
    assert_eq!(MatchedScope::SystemDefault.as_i16(), 4);
    for s in [MatchedScope::Dimension, MatchedScope::Tenant, MatchedScope::Platform, MatchedScope::SystemDefault] {
      assert_eq!(MatchedScope::from_i16(s.as_i16()), s);
    }
    // 未知值兜底（与 from_route_str 同纪律）。
    assert_eq!(MatchedScope::from_i16(0), MatchedScope::SystemDefault);
    assert_eq!(MatchedScope::from_i16(99), MatchedScope::SystemDefault);

    assert_eq!(Outcome::Success.as_i16(), 1);
    assert_eq!(Outcome::Ambiguous.as_i16(), 2);
    assert_eq!(Outcome::Error.as_i16(), 3);
    for o in [Outcome::Success, Outcome::Ambiguous, Outcome::Error] {
      assert_eq!(Outcome::from_i16(o.as_i16()), o);
    }
    // 未知值 fail-closed → Error。
    assert_eq!(Outcome::from_i16(0), Outcome::Error);
    assert_eq!(Outcome::from_i16(99), Outcome::Error);
  }

  #[test]
  fn event_carries_dimension_snapshot_from_ctx() {
    let c = ctx(MatchedScope::Dimension);
    let usage = TokenUsage { prompt_tokens: 1, completion_tokens: 2, total_tokens: 3, cached_input_tokens: 0 };
    let ev = AiUsageEvent::from_ctx_tokens(&c, &usage, Outcome::Success, Utc::now(), Some(7));
    assert_eq!(ev.dimensions, c.dimensions, "维度快照 MUST 随事件落账");
    assert_eq!(ev.matched_scope, MatchedScope::Dimension);
    // chat 事件恒 Some：wire 皆无 cache 字段时 = Some(0)（真零），不是 None（未拆分）。
    assert_eq!(ev.cached_input_tokens, Some(0));
  }

  #[tokio::test]
  async fn records_on_ok_with_usage() {
    let sink = RecordingSink::new();
    let inner = MockProvider::ok(Some(TokenUsage {
      prompt_tokens: 120,
      completion_tokens: 30,
      total_tokens: 150,
      cached_input_tokens: 80,
    }));
    let metered = MeteredLlmProvider::new(inner, ctx(MatchedScope::Platform), sink.clone());
    let out = metered.chat_complete(ChatCompletionRequest::default()).await;
    assert!(out.is_ok());
    assert_eq!(sink.count(), 1);
    let ev = &sink.events.lock().unwrap()[0];
    assert_eq!(ev.matched_scope, MatchedScope::Platform);
    assert_eq!(ev.prompt_tokens, 120);
    assert_eq!(ev.completion_tokens, 30);
    assert_eq!(ev.total_tokens, 150);
    // cache 维度 MUST 随事件透传（计量链完整性）。
    assert_eq!(ev.cached_input_tokens, Some(80));
    assert_eq!(ev.outcome, Outcome::Success);
    assert_eq!(ev.feature_code, "chat");
    assert_eq!(ev.request_kind, "ai_chat");
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
    let inner = MockProvider::ok(Some(TokenUsage {
      prompt_tokens: 1,
      completion_tokens: 1,
      total_tokens: 2,
      cached_input_tokens: 0,
    }));
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
