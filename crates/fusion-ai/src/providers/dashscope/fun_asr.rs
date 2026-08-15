//! 阿里云 DashScope **Fun-ASR 实时**流式语音识别。
//!
//! 协议:`wss://dashscope[-intl].aliyuncs.com/api-ws/v1/inference`
//! 控制消息 JSON:`run-task` / `continue-task` / `finish-task`,音频以二进制帧上传,
//! 事件 `task-started` / `result-generated` / `task-finished` / `task-failed`。
//!
//! 与已下线的 Paraformer 实时 v2 的关系:信封同形,差别在
//! ① 模型族 `fun-asr-realtime`;② `input` 从空对象换成 `{"context":[...]}`(上下文增强);
//! ③ 支持 `continue-task` 在会话中更新上下文。
//!
//! **为什么只留 Fun-ASR**:Paraformer 系列仅北京地域可用,新加坡地域的模型列表不含它;
//! 本仓的驻留档要求区域驻留端点,故不保留双实现(避免"能选到一个在目标地域必然失败的
//! provider")。
//!
//! # PHI 纪律
//!
//! 音频字节与转写文本 MUST NOT 进入日志、错误消息或 `Debug` 输出。本模块的 tracing 字段
//! 只放元数据(task_id / model / region / 事件名 / provider 错误码 / 时长),协议解析失败的
//! 错误消息**不带 body**。
//!
//! 参考:<https://help.aliyun.com/zh/model-studio/real-time-speech-recognition-user-guide>
//!      <https://help.aliyun.com/zh/model-studio/improve-asr-accuracy>

use std::sync::Arc;
use std::time::Duration;

use async_stream::try_stream;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, USER_AGENT};
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_util::task::AbortOnDropHandle;
use uuid::Uuid;

use crate::providers::dashscope::{DashScopeCredentials, DashScopeRegion};
use crate::speech_to_text::{
  AudioEncoding, AudioStreamConfig, SpeechToText, SpeechToTextError, SttEvent, SttEventStream, SttUplink,
  SttUplinkStream, TranscriptSegment, TranscriptWord, TranscriptionResult,
};

const PROVIDER_NAME: &str = "fun_asr_realtime";

/// 实时 Fun-ASR 的默认模型名。地域校验按前缀判定模型族,故常量与前缀分开定义。
pub const DEFAULT_REALTIME_ASR_MODEL: &str = "fun-asr-realtime";

/// Fun-ASR 模型族前缀。新加坡地域只接受该族(见 [`validate_model_for_region`])。
pub const FUN_ASR_MODEL_PREFIX: &str = "fun-asr";

/// 单条上下文增强项的字符上限(provider 限制)。超限在建连前拒绝。
pub const MAX_CONTEXT_ITEM_CHARS: usize = 400;

/// 上下文增强的条数上限(provider 限制)。超限在建连前拒绝。
pub const MAX_CONTEXT_ITEMS: usize = 5;

/// 默认的识别阶段空闲上限:距上一次收到**任何**服务端帧超过此时长即判定会话卡死。
///
/// 没有它,provider 侧挂起(既不回 task-finished 也不关连接)会让事件流永久悬挂 —— 上层
/// 请求挂着、界面停在"转写中",而降级路径永远等不到触发点。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 默认的**建连 + `task-started`** 合计期限。
///
/// 覆盖 TCP 连接、TLS 握手、HTTP Upgrade 与随后的 `task-started` 等待 —— 四段共用一个期限。
/// 分开计时没有意义:调用方关心的是"多久之后可以判定这条会话起不来",而挂死的中间设备
/// (接受 TCP 与 TLS 却不回 101)造成的悬挂与 provider 收下 run-task 却不回 task-started
/// 是同一种失败,只是发生在更早一个阶段。
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(10);

/// Fun-ASR 实时 client。
#[derive(Debug, Clone)]
pub struct FunAsrRealtime {
  credentials: Arc<DashScopeCredentials>,
  region: DashScopeRegion,
  model: String,
  /// `task-started` 等待超时。
  start_timeout: Duration,
  /// 识别阶段的空闲上限。
  idle_timeout: Duration,
  /// 测试用 endpoint 覆盖。生产路径恒为 `None`,endpoint 只由 [`DashScopeRegion`] 决定 ——
  /// 让部署侧能改 endpoint 等于让驻留档形同虚设。
  #[cfg(test)]
  endpoint_override: Option<String>,
}

impl FunAsrRealtime {
  /// 地域是**构造参数**而非可选覆盖:endpoint 由它单独决定,而 `DashScopeRegion::default()`
  /// 是北京 —— 若地域可缺省,那么 `FunAsrRealtime::new(creds)` 这个最顺手的写法恰好是把
  /// 音频送出驻留区的写法,且编译期毫无提示。要求显式传入使"忘了设地域"不可拼写。
  pub fn new(credentials: DashScopeCredentials, region: DashScopeRegion) -> Self {
    Self {
      credentials: Arc::new(credentials),
      region,
      model: DEFAULT_REALTIME_ASR_MODEL.to_string(),
      start_timeout: DEFAULT_START_TIMEOUT,
      idle_timeout: DEFAULT_IDLE_TIMEOUT,
      #[cfg(test)]
      endpoint_override: None,
    }
  }

  /// STT 一等构造（fusion-ai-de-rig.md §4.4 #2）—— credentials + 显式 region + model。
  ///
  /// 消费方（如 hetuos stt_route）MUST 走这里，MUST NOT 先造 chat 侧
  /// `LlmProviderConfig::Qwen` 再解构丢弃借道。region 字符串归一经
  /// [`crate::llm::parse_dashscope_region`]（唯一真相源）；api_key 非空校验。
  /// region 保持显式传入（沿 `new` 的防呆口径：「忘了设地域」不可拼写）。
  pub fn from_parts(
    api_key: &str,
    workspace_id: Option<String>,
    region: DashScopeRegion,
    model: Option<&str>,
  ) -> Result<Self, crate::speech_to_text::SpeechToTextError> {
    if api_key.trim().is_empty() {
      // 只报字段名，不回显值（该错误会进日志）
      return Err(crate::speech_to_text::SpeechToTextError::ConfigInvalid(
        "api_key required".to_string(),
      ));
    }
    Ok(Self::new(DashScopeCredentials { api_key: api_key.to_string(), workspace_id }, region)
      .with_model(model.unwrap_or(DEFAULT_REALTIME_ASR_MODEL)))
  }

  /// 覆盖 WebSocket endpoint —— **仅测试**(对着本地假服务端跑会话级用例)。
  #[cfg(test)]
  #[must_use]
  pub(crate) fn with_endpoint_for_test(mut self, endpoint: impl Into<String>) -> Self {
    self.endpoint_override = Some(endpoint.into());
    self
  }

  /// 生效的 endpoint。生产恒由地域决定。
  fn endpoint(&self) -> String {
    #[cfg(test)]
    if let Some(e) = &self.endpoint_override {
      return e.clone();
    }
    self.region.websocket_endpoint().to_string()
  }

  /// 覆盖模型名。
  ///
  /// **此处不校验**:地域 × 模型族的 fail-closed 校验发生在 `transcribe_realtime` 建连之前
  /// (builder 既不该 panic 也不该返回 `Result`)。需要提前判定用 [`validate_model_for_region`]。
  #[must_use]
  pub fn with_model(mut self, model: impl Into<String>) -> Self {
    self.model = model.into();
    self
  }

  /// 覆盖建连 + `task-started` 的合计期限(见 [`DEFAULT_START_TIMEOUT`])。
  #[must_use]
  pub fn with_start_timeout(mut self, timeout: Duration) -> Self {
    self.start_timeout = timeout;
    self
  }

  /// 覆盖识别阶段空闲上限(见 [`DEFAULT_IDLE_TIMEOUT`])。
  #[must_use]
  pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
    self.idle_timeout = timeout;
    self
  }

  /// 生效的地域。
  pub fn region(&self) -> DashScopeRegion {
    self.region
  }

  /// 生效的模型名。
  pub fn model(&self) -> &str {
    &self.model
  }

  /// 生效的建连 + `task-started` 期限。
  pub fn start_timeout(&self) -> Duration {
    self.start_timeout
  }

  /// 生效的识别阶段空闲上限。
  pub fn idle_timeout(&self) -> Duration {
    self.idle_timeout
  }
}

/// 地域 × 模型族校验。
///
/// 新加坡地域可用的语音识别模型族只有 Fun-ASR(与 Qwen3-ASR-Flash-Realtime);Paraformer
/// 系列仅北京可用。把非 Fun-ASR 模型送到新加坡端点会在 provider 侧才失败,而那时音频
/// 已经出境 —— 故在建连之前 fail-closed 拒绝。
pub fn validate_model_for_region(model: &str, region: DashScopeRegion) -> Result<(), SpeechToTextError> {
  match region {
    DashScopeRegion::Singapore if !model.starts_with(FUN_ASR_MODEL_PREFIX) => Err(SpeechToTextError::ConfigInvalid(
      format!("region=singapore only serves the {FUN_ASR_MODEL_PREFIX}* model family, got {model:?}"),
    )),
    _ => Ok(()),
  }
}

/// 建连前的上下文增强校验:超限 **拒绝**,不静默裁剪。
///
/// 静默裁剪会让上下文增强变成"有时生效有时不生效"——调用方按 trait 编程,不会去读本模块的
/// 常量,更不会知道自己第 6 条术语被丢了。
fn validate_context_items(items: &[String]) -> Result<(), SpeechToTextError> {
  let meaningful = items.iter().filter(|s| !s.trim().is_empty()).count();
  if meaningful > MAX_CONTEXT_ITEMS {
    return Err(SpeechToTextError::ConfigInvalid(format!(
      "fun-asr-realtime accepts at most {MAX_CONTEXT_ITEMS} context items, got {meaningful}"
    )));
  }
  if let Some(pos) = items.iter().position(|s| s.trim().chars().count() > MAX_CONTEXT_ITEM_CHARS) {
    return Err(SpeechToTextError::ConfigInvalid(format!(
      "fun-asr-realtime context item #{pos} exceeds {MAX_CONTEXT_ITEM_CHARS} chars"
    )));
  }
  Ok(())
}

/// 建连前的 `provider_options` 校验:与具名参数撞名的 key **拒绝**。
///
/// `#[serde(flatten)]` 遇到撞名 key 会在 wire 上产出**重复 JSON key**(`to_string` 不去重),
/// provider 取哪个未定义;`format` 撞名甚至会让音频按错误格式解码。静默丢弃同样不行 ——
/// 那会让"我明明覆盖了 sample_rate"变成不可诊断问题。
fn validate_provider_options(options: &serde_json::Value) -> Result<(), SpeechToTextError> {
  let map = match options {
    serde_json::Value::Object(map) => map,
    // 缺省 = 不覆盖任何东西,合法。
    serde_json::Value::Null => return Ok(()),
    // 数组 / 字符串 / 数字等:`build_run_task` 只认 object,其余会被整体丢弃。放行等于让
    // 一份写成 `[{...}]` 或 JSON 字符串的配置**全部参数静默失效**,连 provider 都到不了。
    other => {
      return Err(SpeechToTextError::ConfigInvalid(format!(
        "provider_options must be a JSON object, got {}",
        json_type_name(other)
      )));
    }
  };
  let offending: Vec<&str> = RESERVED_OPTION_KEYS
    .iter()
    .copied()
    // 具名布尔覆盖是刻意支持的用法,不算撞名。
    .filter(|k| !BOOL_OVERRIDE_KEYS.contains(k) && map.contains_key(*k))
    .collect();
  if !offending.is_empty() {
    return Err(SpeechToTextError::ConfigInvalid(format!(
      "provider_options may not override the named parameter(s) {offending:?}; set them on AudioStreamConfig instead"
    )));
  }
  // 具名布尔覆盖给了非布尔值:`read_bool` 的 `as_bool()` 会回 `None` 而退回默认值,同时该 key
  // 又因在 RESERVED 里被剔出 `extra` —— 既没生效也没报错,provider 那边什么都没收到。
  // `"false"`(字符串)是 JSON 配置里最常见的写法,恰好命中这条。
  for key in BOOL_OVERRIDE_KEYS {
    if let Some(value) = map.get(*key)
      && !value.is_boolean()
    {
      return Err(SpeechToTextError::ConfigInvalid(format!(
        "provider_options.{key} must be a boolean, got {}",
        json_type_name(value)
      )));
    }
  }
  Ok(())
}

/// BCP-47 标签 → DashScope 认的主语言子标签(`zh-CN` → `zh`)。
///
/// provider 的取值表是 `zh` / `en` 这一级;调用方按 BCP-47 传 `zh-CN` 是完全正常的写法
/// (调用方按地区存 locale 是常见做法),不该因此被拒。归一发生在最靠近 wire 的一层,且不丢失
/// 调用方意图 —— `zh-CN` 与 `zh` 对一个只区分语言的识别器是同一件事。空标签整条丢弃。
fn primary_language_subtags(hints: &[String]) -> Vec<String> {
  let mut out: Vec<String> = Vec::with_capacity(hints.len());
  for hint in hints {
    let primary = hint.split(['-', '_']).next().unwrap_or("").trim().to_ascii_lowercase();
    if !primary.is_empty() && !out.contains(&primary) {
      out.push(primary);
    }
  }
  out
}

/// JSON 值的类型名 —— 错误消息里**只**说类型,不说值(`provider_options` 由调用方填,
/// 但同一条格式化路径不该给未来某个带 PHI 的字段留缺口)。
fn json_type_name(value: &serde_json::Value) -> &'static str {
  match value {
    serde_json::Value::Null => "null",
    serde_json::Value::Bool(_) => "boolean",
    serde_json::Value::Number(_) => "number",
    serde_json::Value::String(_) => "string",
    serde_json::Value::Array(_) => "array",
    serde_json::Value::Object(_) => "object",
  }
}

/// 会话**中**的词表更新按 provider 限额裁剪并告警。
///
/// 与建连前不同,这里没有可以 fail 的返回路径(上行泵已经在跑),故取"裁剪 + 显式告警"——
/// 告警至少让裁剪是可发现的。返回值为空表示"不变更"。
fn clamp_context(items: &[String]) -> Vec<ContextItem> {
  let kept = items.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
  if kept.len() > MAX_CONTEXT_ITEMS {
    tracing::warn!(
      provider = PROVIDER_NAME,
      given = kept.len(),
      kept = MAX_CONTEXT_ITEMS,
      "context update exceeds the provider limit; keeping the most recent items"
    );
  }
  let start = kept.len().saturating_sub(MAX_CONTEXT_ITEMS);
  kept[start..]
    .iter()
    .map(|s| {
      if s.chars().count() > MAX_CONTEXT_ITEM_CHARS {
        tracing::warn!(
          provider = PROVIDER_NAME,
          limit = MAX_CONTEXT_ITEM_CHARS,
          "context item exceeds the provider length limit; truncating"
        );
        ContextItem { text: s.chars().take(MAX_CONTEXT_ITEM_CHARS).collect() }
      } else {
        ContextItem { text: (*s).to_string() }
      }
    })
    .collect()
}

// =========================================================================
// 协议消息(只覆盖需要的字段;未识别字段一律 ignore)
// =========================================================================

#[derive(Debug, Serialize)]
struct ClientMessage<P: Serialize> {
  header: ClientHeader,
  payload: P,
}

#[derive(Debug, Serialize)]
struct ClientHeader {
  action: &'static str,
  task_id: String,
  streaming: &'static str,
}

#[derive(Debug, Serialize)]
struct RunTaskPayload {
  task_group: &'static str,
  task: &'static str,
  function: &'static str,
  model: String,
  parameters: RunTaskParameters,
  input: TaskInput,
}

#[derive(Debug, Serialize)]
struct RunTaskParameters {
  format: &'static str,
  sample_rate: u32,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  vocabulary_id: Vec<String>,
  #[serde(skip_serializing_if = "Vec::is_empty")]
  language_hints: Vec<String>,
  disfluency_removal_enabled: bool,
  punctuation_prediction_enabled: bool,
  inverse_text_normalization_enabled: bool,
  #[serde(flatten)]
  extra: serde_json::Map<String, serde_json::Value>,
}

/// `input` 载荷。Fun-ASR 与 Paraformer 的唯一结构差异:这里承载上下文增强词表。
#[derive(Debug, Default, Serialize)]
struct TaskInput {
  #[serde(skip_serializing_if = "Vec::is_empty")]
  context: Vec<ContextItem>,
}

/// 一条上下文增强项。机制是**词表匹配式修正**:`text` 须包含待识别原词,
/// 纯语义描述的纠正效果有限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ContextItem {
  text: String,
}

#[derive(Debug, Deserialize)]
struct ServerMessage {
  header: ServerHeader,
  #[serde(default)]
  payload: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct ServerHeader {
  #[serde(default, rename = "task_id")]
  _task_id: Option<String>,
  event: String,
  #[serde(default)]
  error_code: Option<String>,
  #[serde(default)]
  error_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResultGeneratedPayload {
  output: ResultOutput,
  /// 刻意留成未解析的 `Value`,由调用处宽容解析。
  ///
  /// 与 `output` 同在一个 `from_value` 里时,provider 改一次 usage 的字段类型(实测见过
  /// `duration` 回浮点秒)就会让**整包**解析失败 —— 那是 `Protocol` 致命错误,一次成功的
  /// 转写会因为拿不到用量而全部丢失。计量是 best-effort,识别结果不是。
  #[serde(default)]
  usage: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct TaskFinishedPayload {
  #[serde(default)]
  usage: Option<ResultUsage>,
}

/// provider 回的用量。`duration` 以**秒**计(音频时长),是 STT 的计量维度。
///
/// `f64` 而非 `u64`:provider 回过小数秒,而用整数类型接会让整条 usage 解析失败。
#[derive(Debug, Deserialize)]
struct ResultUsage {
  #[serde(default)]
  duration: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ResultOutput {
  sentence: ResultSentence,
}

#[derive(Debug, Deserialize)]
struct ResultSentence {
  #[serde(default)]
  begin_time: Option<u64>,
  #[serde(default)]
  end_time: Option<u64>,
  text: String,
  #[serde(default)]
  sentence_end: bool,
  #[serde(default)]
  confidence: Option<f32>,
  #[serde(default)]
  words: Vec<ResultWord>,
}

#[derive(Debug, Deserialize)]
struct ResultWord {
  #[serde(default)]
  begin_time: Option<u64>,
  #[serde(default)]
  end_time: Option<u64>,
  text: String,
  #[serde(default)]
  punctuation: Option<String>,
}

#[derive(Debug, Serialize)]
struct ContinueTaskPayload {
  input: TaskInput,
}

#[derive(Debug, Serialize)]
struct FinishTaskPayload {
  input: TaskInput,
}

// =========================================================================
// SpeechToText 实现
// =========================================================================

#[async_trait]
impl SpeechToText for FunAsrRealtime {
  fn provider_name(&self) -> &'static str {
    PROVIDER_NAME
  }

  fn model(&self) -> &str {
    &self.model
  }

  async fn transcribe_realtime(
    &self,
    uplink: SttUplinkStream,
    config: AudioStreamConfig,
  ) -> Result<SttEventStream, SpeechToTextError> {
    // 调用方配置层面的错误在这里就返回;建连 / 协议错误留到首次 poll——见 trait 文档的
    // 「失败时机」。这样"拿到 Ok(stream) 却从不 poll"不会悬着一条已建立的跨境连接。
    if config.channels != 1 {
      return Err(SpeechToTextError::ConfigInvalid(format!(
        "fun-asr-realtime accepts mono audio only, got channels={}",
        config.channels
      )));
    }
    if config.sample_rate == 0 {
      return Err(SpeechToTextError::ConfigInvalid("sample_rate must be non-zero".into()));
    }
    validate_model_for_region(&self.model, self.region)?;
    validate_context_items(&config.context_items)?;
    validate_provider_options(&config.provider_options)?;
    let format = provider_audio_format(config.encoding)?;

    let endpoint = self.endpoint();
    let model = self.model.clone();
    let region = self.region;
    let credentials = self.credentials.clone();
    let start_timeout = self.start_timeout;
    let idle_timeout = self.idle_timeout;

    let stream = try_stream! {
      // 1) 握手:加 Authorization Bearer
      let mut req = endpoint
        .as_str()
        .into_client_request()
        // 这是本 crate 的常量拼错,不是调用方的配置问题。
        .map_err(|e| SpeechToTextError::Other(anyhow::anyhow!("invalid websocket endpoint {endpoint}: {e}")))?;
      let mut auth = HeaderValue::from_str(&format!("Bearer {}", credentials.api_key))
        .map_err(|e| SpeechToTextError::Auth(format!("invalid api key header: {e}")))?;
      // hyper / tungstenite 链路上任何一处 `{:?}` headers 都会打明文,除非标 sensitive。
      auth.set_sensitive(true);
      req.headers_mut().insert(AUTHORIZATION, auth);
      req.headers_mut().insert(
        USER_AGENT,
        HeaderValue::from_static(concat!("fusion-ai/", env!("CARGO_PKG_VERSION"), " fun-asr-realtime")),
      );
      if let Some(ws) = &credentials.workspace_id {
        req.headers_mut().insert(
          "X-DashScope-WorkSpace",
          HeaderValue::from_str(ws)
            .map_err(|e| SpeechToTextError::ConfigInvalid(format!("invalid workspace: {e}")))?,
        );
      }

      // 期限从**建连之前**起算:TCP / TLS / Upgrade 三段无界时,一台接受连接却不回 101 的
      // 中间设备就能让事件流永久悬挂 —— 上层请求挂着、界面停在"转写中",降级路径等不到
      // 触发点。这与 idle_timeout 要消除的失败同型,只是更早。
      let started_deadline = tokio::time::Instant::now() + start_timeout;
      let (ws_stream, _resp) = tokio::time::timeout_at(started_deadline, tokio_tungstenite::connect_async(req))
        .await
        .map_err(|_| SpeechToTextError::Timeout(format!("dashscope websocket connect exceeded {start_timeout:?}")))?
        .map_err(|e| match e {
        tokio_tungstenite::tungstenite::Error::Http(resp)
          if resp.status() == 401 || resp.status() == 403 =>
        {
          SpeechToTextError::Auth(format!("dashscope handshake rejected: {}", resp.status()))
        }
        other => SpeechToTextError::Network(format!("dashscope websocket connect failed: {other}")),
      })?;

      let task_id = Uuid::new_v4().simple().to_string();
      tracing::debug!(provider = PROVIDER_NAME, %task_id, %model, ?region, "stt session handshake complete");

      let (mut sink, mut source) = ws_stream.split();

      // 2) 发送 run-task
      let run_task = build_run_task(&task_id, model.clone(), &config, format);
      let run_task_json = serde_json::to_string(&run_task)
        .map_err(|e| SpeechToTextError::Protocol(format!("serialize run-task: {e}")))?;
      sink
        .send(Message::Text(run_task_json.into()))
        .await
        .map_err(|e| SpeechToTextError::Network(format!("send run-task: {e}")))?;

      // 3) 等 task-started。服务端在业务事件之前可能先发 ping —— 这里 MUST 循环跳过
      // 可忽略帧,否则一个 ping 就打死整次识别。期限沿用建连时那一个(不重新计时),否则
      // "服务端一直发 ping"或"握手慢"都能把总等待时间叠加成两倍。
      loop {
        let msg = tokio::time::timeout_at(started_deadline, source.next())
          .await
          .map_err(|_| SpeechToTextError::Timeout(format!("waiting task-started for {start_timeout:?}")))?
          .ok_or_else(|| SpeechToTextError::Protocol("websocket closed before task-started".into()))?
          .map_err(|e| SpeechToTextError::Network(format!("websocket error before task-started: {e}")))?;
        match parse_server_event(&msg)? {
          // 可忽略帧(ping / pong / raw / 未知事件)。
          None => continue,
          Some(ParsedEvent::Started) => {
            tracing::debug!(provider = PROVIDER_NAME, %task_id, "stt task-started");
            yield SttEvent::Started { provider_session_id: Some(task_id.clone()) };
            break;
          }
          Some(ParsedEvent::Failed { code, message }) => {
            let retryable = is_provider_retryable(&code);
            tracing::warn!(provider = PROVIDER_NAME, %task_id, %code, retryable, "stt failed before task-started");
            Err(SpeechToTextError::Provider {
              provider: PROVIDER_NAME.into(),
              code,
              message,
              retryable,
            })?;
            unreachable!("the `?` above already returned from the stream");
          }
          // PHI 纪律:不把事件内容(可能含转写文本)带进错误消息,只报事件类别。
          Some(other) => {
            Err(SpeechToTextError::Protocol(format!(
              "expected task-started, got {}", other.kind_name()
            )))?;
            unreachable!("the `?` above already returned from the stream");
          }
        }
      }

      // 4) 启动上行泵 task。用 AbortOnDropHandle 保证消费方提前 drop 本事件流
      // （客户端断连 / 取消）时上行泵一并 abort —— 否则 detach 的任务会一直持有
      // ws sink 并无限拉取(可能是活麦克风的)上行流,连接与流量泄漏。
      let mut uplink = uplink;
      let uplink_task_id = task_id.clone();
      let send_task = AbortOnDropHandle::new(tokio::spawn(async move {
        while let Some(item) = uplink.next().await {
          match item {
            SttUplink::Audio(frame) => {
              if frame.is_empty() {
                continue;
              }
              // tungstenite 的 Message::Binary 就是 bytes::Bytes —— 直接移动,零拷贝。
              // 多一次 to_vec 既是每帧一次 memcpy,也是多留一份未清零的 PHI 堆副本。
              if let Err(e) = sink.send(Message::Binary(frame)).await {
                let msg = format!("send audio frame: {e}");
                tracing::warn!(provider = PROVIDER_NAME, task_id = %uplink_task_id, error = %e, "stt uplink failed");
                return Err(msg);
              }
            }
            SttUplink::ContextUpdate(items) => {
              let context = clamp_context(&items);
              if context.is_empty() {
                // 空列表 = 不变更(provider 无"清空上下文"的表达)。文档已声明该语义。
                continue;
              }
              let continue_task = ClientMessage {
                header: ClientHeader {
                  action: "continue-task",
                  task_id: uplink_task_id.clone(),
                  streaming: "duplex",
                },
                payload: ContinueTaskPayload { input: TaskInput { context } },
              };
              let json = serde_json::to_string(&continue_task).map_err(|e| e.to_string())?;
              if let Err(e) = sink.send(Message::Text(json.into())).await {
                let msg = format!("send continue-task: {e}");
                tracing::warn!(provider = PROVIDER_NAME, task_id = %uplink_task_id, error = %e, "stt uplink failed");
                return Err(msg);
              }
            }
          }
        }
        // 上行流结束 = 音频说完,发 finish-task
        let finish = ClientMessage {
          header: ClientHeader {
            action: "finish-task",
            task_id: uplink_task_id.clone(),
            streaming: "duplex",
          },
          payload: FinishTaskPayload { input: TaskInput::default() },
        };
        let finish_json = serde_json::to_string(&finish).map_err(|e| e.to_string())?;
        if let Err(e) = sink.send(Message::Text(finish_json.into())).await {
          let msg = format!("send finish-task: {e}");
          // 这条最重要:发不出 finish-task 会让服务端永不回 task-finished,主循环随后只会
          // 看到"连接被关了",根因就在这里。MUST 在发生的当下留痕。
          tracing::warn!(provider = PROVIDER_NAME, task_id = %uplink_task_id, error = %e, "stt finish-task failed");
          return Err(msg);
        }
        Ok::<_, String>(())
      }));

      // 5) 持续读事件,合并 segments,直到 task-finished / task-failed
      let mut all_segments: Vec<TranscriptSegment> = Vec::new();
      // 结尾未终结的 partial 也进 `all_segments`,只是记住它的位置:服务端在 finish-task 后
      // 未 flush 就关连接时,最后一句只以 Partial 出现过——丢掉它,UI 上显示过的末句就不会进
      // 最终文本,用户以为已收录。
      //
      // 记位置而不是在旁边另存一份 segment:两处存储意味着「收尾时别忘了把 pending 补回去」
      // 这条纪律只活在一个人的记性里,而 truncate 让"临时的那一段"在被取代时自然消失。
      let mut partial_idx: Option<usize> = None;
      // provider 的 usage.duration 语义(累计 vs 每句)未经实测确认,故 result-generated
      // 上取 max、task-finished 上无条件覆盖(它是权威值)。两种假设下都不会更差。
      let mut duration_ms: Option<u64> = None;
      loop {
        let msg = tokio::time::timeout(idle_timeout, source.next())
          .await
          .map_err(|_| SpeechToTextError::Timeout(format!(
            "no frame from dashscope for {idle_timeout:?}"
          )))?;
        let Some(msg_result) = msg else {
          Err(SpeechToTextError::Network("websocket closed before task-finished".into()))?;
          unreachable!("the `?` above already returned from the stream");
        };
        let msg = msg_result.map_err(|e| {
          SpeechToTextError::Network(format!("websocket error during recognition: {e}"))
        })?;
        // 解析失败在这里是**致命**的:把它当可忽略帧跳过会静默丢掉全部识别结果,
        // 让一次完整的口述变成"识别为空",而流仍以成功结束。
        let Some(parsed) = parse_server_event(&msg)? else { continue };

        match parsed {
          ParsedEvent::Started => continue, // 重复事件忽略
          ParsedEvent::ResultGenerated { segment, duration_ms: seen } => {
            if let Some(seen) = seen {
              duration_ms = Some(duration_ms.map_or(seen, |cur| cur.max(seen)));
            }
            // 每段恰好复制一次:累积器与下游事件各需要一份所有权,这一份无从省去。
            if let Some(i) = partial_idx.take() {
              all_segments.truncate(i);
            }
            if segment.is_final {
              all_segments.push(segment.clone());
              yield SttEvent::SegmentFinal(segment);
            } else {
              partial_idx = Some(all_segments.len());
              all_segments.push(segment.clone());
              yield SttEvent::Partial(segment);
            }
          }
          ParsedEvent::Finished { duration_ms: seen } => {
            if seen.is_some() {
              duration_ms = seen;
            }
            // 结尾的 partial 已经在 `all_segments` 里(见 partial_idx),无需在此补回。
            let final_text: String = all_segments.iter().map(|s| s.text.as_str()).collect();
            tracing::debug!(
              provider = PROVIDER_NAME,
              %task_id,
              segments = all_segments.len(),
              duration_ms,
              "stt task-finished"
            );
            yield SttEvent::TaskFinished(TranscriptionResult {
              text: final_text,
              language: None,
              confidence: None,
              segments: all_segments,
              provider: PROVIDER_NAME.into(),
              model: model.clone(),
              provider_session_id: Some(task_id.clone()),
              audio_duration_ms: duration_ms,
            });
            // 已经产出最终结果。上行泵此刻的收尾错误 MUST NOT 变成终态 Err ——
            // `is_retryable()` 会让照文档实现的调用方重放一整段已成功的音频,
            // 在录入通道里意味着重复业务记录。降级为 warn。
            match tokio::time::timeout(Duration::from_secs(5), send_task).await {
              Ok(Ok(Ok(()))) => {}
              Ok(Ok(Err(msg))) => tracing::warn!(
                provider = PROVIDER_NAME, %task_id, error = %msg,
                "stt uplink reported an error after the transcript was final; not failing the session"
              ),
              Ok(Err(join)) => tracing::warn!(
                provider = PROVIDER_NAME, %task_id, error = %join, "stt uplink pump panicked"
              ),
              Err(_) => tracing::warn!(
                provider = PROVIDER_NAME, %task_id, "stt uplink pump did not finish within the drain window"
              ),
            }
            break;
          }
          ParsedEvent::Failed { code, message } => {
            let retryable = is_provider_retryable(&code);
            tracing::warn!(provider = PROVIDER_NAME, %task_id, %code, retryable, "stt task-failed");
            Err(SpeechToTextError::Provider {
              provider: PROVIDER_NAME.into(),
              code,
              message,
              retryable,
            })?;
            unreachable!("the `?` above already returned from the stream");
          }
        }
      }
    };

    Ok(Box::pin(stream))
  }
}

// =========================================================================
// 辅助
// =========================================================================

/// [`AudioEncoding`] → provider `format` 字面名。
///
/// **无转码即拒绝**:DashScope 的 `format: "pcm"` 语义是 16-bit signed LE,喂 f32 样本
/// provider 不报错、只会当 s16 解出噪声;`"opus"` 指裸 Opus 包,吃不下 WebM 容器。两者都会
/// 产出"链路跑通、无报错、转写是垃圾"的最难诊断故障 —— 与 g711 同样 fail-closed。
///
/// 与 [`validate_model_for_region`] 同为**可预检**的公共判据:调用方(网关)需要在自己那一层
/// 就能回答"这个编码这个 provider 收不收",否则只能把接受列表抄一份到网关、再抄一份到前端 ——
/// 三处副本,新增编码时必然漏掉其中一处。
pub fn provider_audio_format(encoding: AudioEncoding) -> Result<&'static str, SpeechToTextError> {
  match encoding {
    AudioEncoding::PcmS16Le => Ok("pcm"),
    AudioEncoding::Wav => Ok("wav"),
    AudioEncoding::Mp3 => Ok("mp3"),
    AudioEncoding::Aac => Ok("aac"),
    AudioEncoding::Amr => Ok("amr"),
    AudioEncoding::Opus => Ok("opus"),
    AudioEncoding::PcmF32Le => Err(SpeechToTextError::ConfigInvalid(
      "fun-asr-realtime pcm means 16-bit signed LE; convert PcmF32Le before sending".into(),
    )),
    AudioEncoding::WebmOpus => Err(SpeechToTextError::ConfigInvalid(
      "fun-asr-realtime opus means raw Opus packets, not a WebM container; demux before sending".into(),
    )),
    AudioEncoding::G711Ulaw | AudioEncoding::G711Alaw => {
      Err(SpeechToTextError::ConfigInvalid("fun-asr-realtime does not accept g711 audio".into()))
    }
  }
}

/// `provider_options` 中会与 [`RunTaskParameters`] 具名字段撞名的 key。
///
/// 撞名的后果是 `#[serde(flatten)]` 产出**重复 JSON key**(`to_string` 不会去重),provider
/// 取哪个未定义 —— `format` 撞名时甚至会让音频按错误格式解码。故一律拒绝而非静默丢弃:
/// 静默丢弃会让"我明明覆盖了 sample_rate"变成不可诊断问题。
const RESERVED_OPTION_KEYS: &[&str] = &[
  "format",
  "sample_rate",
  "vocabulary_id",
  "language_hints",
  "disfluency_removal_enabled",
  "punctuation_prediction_enabled",
  "inverse_text_normalization_enabled",
  // `build_run_task` 用 `AudioStreamConfig::domain_hint` 无条件覆盖同名 extra key,所以它
  // 与具名字段一样会静默吃掉调用方的值 —— 归入撞名拒绝,而不是让覆盖悄悄发生。
  "domain_hint",
];

/// `provider_options` 里可被识别为具名布尔覆盖的 key(这些 **不** 进 `extra`)。
const BOOL_OVERRIDE_KEYS: &[&str] =
  &["disfluency_removal_enabled", "punctuation_prediction_enabled", "inverse_text_normalization_enabled"];

fn build_run_task(
  task_id: &str,
  model: String,
  config: &AudioStreamConfig,
  format: &'static str,
) -> ClientMessage<RunTaskPayload> {
  let read_bool = |key: &str, default: bool| -> bool {
    config.provider_options.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
  };
  let itn_enabled = read_bool("inverse_text_normalization_enabled", true);
  let punctuation_enabled = read_bool("punctuation_prediction_enabled", true);
  let disfluency_removal = read_bool("disfluency_removal_enabled", false);

  let mut extra_params = serde_json::Map::new();
  if let serde_json::Value::Object(map) = config.provider_options.clone() {
    for (k, v) in map {
      // 撞名 key 由 `validate_provider_options` 在建连前拒绝;这里的过滤是最后一道防线,
      // 保证即使校验被绕过也不会产出重复 JSON key。
      if !RESERVED_OPTION_KEYS.contains(&k.as_str()) {
        extra_params.insert(k, v);
      }
    }
  }
  if let Some(domain) = config.domain_hint.clone() {
    extra_params.insert("domain_hint".into(), serde_json::Value::String(domain));
  }

  ClientMessage {
    header: ClientHeader { action: "run-task", task_id: task_id.to_string(), streaming: "duplex" },
    payload: RunTaskPayload {
      task_group: "audio",
      task: "asr",
      function: "recognition",
      model,
      parameters: RunTaskParameters {
        format,
        sample_rate: config.sample_rate,
        // provider 侧已注册词表的 ID(不是词条本身);会话内临时术语走 input.context。
        //
        // 注意:`vocabulary_id` 的 wire 类型(单值 vs 数组)尚未对着实跑的 provider 验证过,
        // 沿用了 Paraformer 实装期的数组形态。pilot 接入时 MUST 实测确认。
        vocabulary_id: config.vocabulary_ids.clone(),
        language_hints: primary_language_subtags(&config.language_hints),
        disfluency_removal_enabled: disfluency_removal,
        punctuation_prediction_enabled: punctuation_enabled,
        inverse_text_normalization_enabled: itn_enabled,
        extra: extra_params,
      },
      input: TaskInput { context: clamp_context(&config.context_items) },
    },
  }
}

#[derive(Debug)]
enum ParsedEvent {
  Started,
  ResultGenerated { segment: TranscriptSegment, duration_ms: Option<u64> },
  Finished { duration_ms: Option<u64> },
  Failed { code: String, message: String },
}

impl ParsedEvent {
  /// 事件类别名 —— 用于错误消息。**刻意不用 `{:?}`**:`ResultGenerated` 里含转写全文,
  /// Debug 打印会把 PHI 送进错误链。
  fn kind_name(&self) -> &'static str {
    match self {
      Self::Started => "task-started",
      Self::ResultGenerated { .. } => "result-generated",
      Self::Finished { .. } => "task-finished",
      Self::Failed { .. } => "task-failed",
    }
  }
}

/// provider 的 `usage.duration` 以秒计;转 ms 供调用方计量。
///
/// 负数与非有限值按"没回用量"处理 —— 一个负的音频时长不是可信数据,记进计量比不记更糟。
fn usage_duration_ms(usage: Option<ResultUsage>) -> Option<u64> {
  usage
    .and_then(|u| u.duration)
    .filter(|secs| secs.is_finite() && *secs >= 0.0)
    .map(|secs| (secs * 1_000.0).round() as u64)
}

/// `serde_json::Error` → **不含任何 payload 值**的描述。
///
/// 这不是洁癖:`serde_json` 在类型不匹配(`Category::Data`)时会把实际值渲染进 Display ——
/// `invalid type: string "张奶奶体温三十八度", expected struct ResultSentence`。服务端消息体
/// 里就是转写文本,而该错误会一路进 RPC 错误体与服务端日志。故只取类别与位置:定位一个
/// 结构漂移足够了,而值本身从来不是诊断所必需的。
fn redacted_json_error(e: &serde_json::Error) -> String {
  let kind = match e.classify() {
    serde_json::error::Category::Io => "io error",
    serde_json::error::Category::Syntax => "malformed json",
    serde_json::error::Category::Data => "unexpected shape",
    serde_json::error::Category::Eof => "truncated json",
  };
  format!("{kind} at line {} column {}", e.line(), e.column())
}

/// 解析一帧服务端消息。
///
/// - `Ok(Some(ev))` —— 业务事件。
/// - `Ok(None)` —— **可忽略**的非业务帧(ping / pong / raw / 未知事件名)。
/// - `Err(..)` —— 真协议错误(整包 JSON 解析失败、payload 结构不匹配、意外二进制帧)或连接关闭。
///
/// 这个三分是刻意的:早先把两类合并成同一个 `Protocol` 错误,主循环只能一律 `continue`,
/// 结果是 payload 结构一变就静默丢掉全部识别结果 —— 流仍以成功结束、text 为空。
fn parse_server_event(msg: &Message) -> Result<Option<ParsedEvent>, SpeechToTextError> {
  // 借用而非 `to_string()`:每帧一份转写文本的堆副本,用完不清零、散落在堆上,而 serde
  // 从 `&str` 解析同样可行。
  let text: &str = match msg {
    Message::Text(t) => t.as_str(),
    // 服务端发二进制说明协议已经错位,继续读只会读到更多垃圾。
    Message::Binary(_) => return Err(SpeechToTextError::Protocol("unexpected binary frame from server".into())),
    Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => return Ok(None),
    Message::Close(frame) => {
      let reason = frame.as_ref().map(|f| format!("{} {}", f.code, f.reason)).unwrap_or_default();
      return Err(SpeechToTextError::Network(format!("websocket closed: {reason}")));
    }
  };

  // PHI 纪律:错误消息 MUST NOT 带 body —— 服务端消息体里就是转写文本。`{e}` 不满足这一点,
  // 见 [`redacted_json_error`]。
  let parsed: ServerMessage = serde_json::from_str(text).map_err(|e| {
    SpeechToTextError::Protocol(format!("parse {PROVIDER_NAME} server message failed: {}", redacted_json_error(&e)))
  })?;

  match parsed.header.event.as_str() {
    "task-started" => Ok(Some(ParsedEvent::Started)),
    "result-generated" => {
      let payload: ResultGeneratedPayload = serde_json::from_value(parsed.payload).map_err(|e| {
        SpeechToTextError::Protocol(format!("parse result-generated payload: {}", redacted_json_error(&e)))
      })?;
      let duration_ms = usage_duration_ms(serde_json::from_value::<ResultUsage>(payload.usage).ok());
      let segment = TranscriptSegment {
        text: payload.output.sentence.text,
        begin_ms: payload.output.sentence.begin_time,
        end_ms: payload.output.sentence.end_time,
        confidence: payload.output.sentence.confidence,
        words: payload
          .output
          .sentence
          .words
          .into_iter()
          .map(|w| TranscriptWord {
            text: w.text,
            begin_ms: w.begin_time,
            end_ms: w.end_time,
            punctuation: w.punctuation,
          })
          .collect(),
        is_final: payload.output.sentence.sentence_end,
      };
      Ok(Some(ParsedEvent::ResultGenerated { segment, duration_ms }))
    }
    "task-finished" => {
      // finished 的 payload 形状比 result-generated 松(output 可能是空对象),
      // 故解析失败不打穿流 —— 拿不到用量不该让一次成功的转写失败。
      let duration_ms = serde_json::from_value::<TaskFinishedPayload>(parsed.payload)
        .ok()
        .and_then(|p| usage_duration_ms(p.usage));
      Ok(Some(ParsedEvent::Finished { duration_ms }))
    }
    "task-failed" => Ok(Some(ParsedEvent::Failed {
      code: parsed.header.error_code.unwrap_or_else(|| "UNKNOWN".into()),
      message: parsed.header.error_message.unwrap_or_default(),
    })),
    // 未知事件名 = provider 新增了我们还不认识的事件,忽略即可(不是协议损坏)。
    other => {
      tracing::debug!(provider = PROVIDER_NAME, event = other, "ignoring unknown stt event");
      Ok(None)
    }
  }
}

fn is_provider_retryable(code: &str) -> bool {
  // DashScope 错误码以 `Throttling.` / `Network.` / `InternalError` 开头通常可重试。
  code.starts_with("Throttling")
    || code.starts_with("Network.")
    || code.starts_with("InternalError")
    || matches!(code, "RequestTimeout" | "ServiceUnavailable")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::speech_to_text::SttUplink;

  fn cfg() -> AudioStreamConfig {
    AudioStreamConfig::pcm_s16le_16k_mono_40ms()
  }

  fn provider() -> FunAsrRealtime {
    provider_in(DashScopeRegion::Beijing)
  }

  fn provider_in(region: DashScopeRegion) -> FunAsrRealtime {
    FunAsrRealtime::new(DashScopeCredentials { api_key: "x".into(), workspace_id: None }, region)
  }

  fn empty_uplink() -> SttUplinkStream {
    SttUplink::from_audio(futures::stream::empty::<bytes::Bytes>())
  }

  /// `SttEventStream` 无 `Debug`,故断言前先把 `Ok` 折叠掉。
  fn expect_err(result: Result<SttEventStream, SpeechToTextError>) -> SpeechToTextError {
    match result {
      Ok(_) => panic!("expected an error, got an open stream"),
      Err(e) => e,
    }
  }

  /// 生产路径走 `to_string`,`Value::Object` 会自动去重重复 key —— 用字符串再解析回来,
  /// 才和生产是同一条序列化路径。
  fn run_task_json(config: &AudioStreamConfig) -> serde_json::Value {
    let msg = build_run_task("task-1", DEFAULT_REALTIME_ASR_MODEL.to_string(), config, "pcm");
    let wire = serde_json::to_string(&msg).unwrap();
    serde_json::from_str(&wire).unwrap()
  }

  fn run_task_wire(config: &AudioStreamConfig) -> String {
    serde_json::to_string(&build_run_task("task-1", DEFAULT_REALTIME_ASR_MODEL.to_string(), config, "pcm")).unwrap()
  }

  // ---- 协议解析 ----

  #[test]
  fn parses_task_started() {
    let msg =
      Message::Text(r#"{"header":{"task_id":"abc","event":"task-started","attributes":{}},"payload":{}}"#.into());
    assert!(matches!(parse_server_event(&msg).unwrap(), Some(ParsedEvent::Started)));
  }

  #[test]
  fn parses_result_generated_partial() {
    let body = r#"{
      "header": {"task_id":"abc","event":"result-generated","attributes":{}},
      "payload": {"output":{"sentence":{
        "begin_time": 170, "end_time": null, "text": "张奶奶体温",
        "sentence_end": false,
        "words": [{"begin_time":170,"end_time":295,"text":"张","punctuation":null}]
      }}, "usage":{"duration":1}}
    }"#;
    match parse_server_event(&Message::Text(body.into())).unwrap() {
      Some(ParsedEvent::ResultGenerated { segment, duration_ms }) => {
        assert_eq!(segment.text, "张奶奶体温");
        assert!(!segment.is_final);
        assert_eq!(segment.words.len(), 1);
        assert_eq!(segment.words[0].text, "张");
        assert_eq!(duration_ms, Some(1_000), "usage.duration 以秒计,MUST 转成 ms");
      }
      other => panic!("expected ResultGenerated, got {other:?}"),
    }
  }

  #[test]
  fn ping_pong_and_unknown_events_are_ignorable_not_errors() {
    // 早先它们与真协议错误共用 Protocol 变体,主循环只能一律 continue —— 那正是静默
    // 丢结果的根源。
    assert!(parse_server_event(&Message::Ping(Vec::new().into())).unwrap().is_none());
    assert!(parse_server_event(&Message::Pong(Vec::new().into())).unwrap().is_none());
    let unknown = Message::Text(r#"{"header":{"event":"task-progress"},"payload":{}}"#.into());
    assert!(parse_server_event(&unknown).unwrap().is_none());
  }

  #[test]
  fn malformed_payload_is_a_hard_error_not_an_ignorable_frame() {
    // provider 改了 payload 结构 → 必须炸,不能静默把每条结果 continue 掉导致空转写。
    let bad_shape =
      Message::Text(r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":123}}}}"#.into());
    assert!(matches!(parse_server_event(&bad_shape), Err(SpeechToTextError::Protocol(_))));

    let not_json = Message::Text("<html>gateway error</html>".into());
    assert!(matches!(parse_server_event(&not_json), Err(SpeechToTextError::Protocol(_))));

    let binary = Message::Binary(vec![0u8, 1].into());
    assert!(matches!(parse_server_event(&binary), Err(SpeechToTextError::Protocol(_))));
  }

  #[test]
  fn parse_errors_never_echo_the_message_body() {
    // 服务端消息体里就是转写文本 —— 它 MUST NOT 进错误链。
    //
    // 三种形态都要覆盖,因为 serde_json 只在**类型不匹配**(Category::Data)时才把实际值渲染
    // 进 Display。早先这条测试把 PHI 放在未知顶层字段上 —— serde 直接忽略该字段,于是测试
    // 通过而真正会回显的路径从未被验证过。
    let leaks = [
      // ① sentence 整体是字符串而非对象 → "invalid type: string \"…\", expected struct ResultSentence"
      r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":"张奶奶体温三十八度"}}}"#,
      // ② 顶层就是一个 JSON 字符串标量。
      r#""张奶奶体温三十八度""#,
      // ③ text 字段类型不符,同一段文本出现在别处。
      r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":["张奶奶体温三十八度"]}}}}"#,
    ];
    for body in leaks {
      let err = parse_server_event(&Message::Text(body.into())).unwrap_err();
      let rendered = format!("{err}");
      assert!(!rendered.contains("张奶奶"), "transcript leaked into the error: {rendered}");
      // 仍要能定位问题:类别 + 行列。
      assert!(rendered.contains("line"), "error lost its position information: {rendered}");
    }
  }

  #[test]
  fn a_broken_usage_field_does_not_discard_the_transcript() {
    // 计量是 best-effort,识别结果不是。usage 与 output 同在一个 payload 里,若共用一次
    // 解析,provider 改一次 usage 的类型就会让整段转写以 Protocol 错误丢掉。
    let msg = Message::Text(
      r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":"体温三十八度","sentence_end":true}},"usage":{"duration":"not-a-number"}}}"#
        .into(),
    );
    match parse_server_event(&msg).unwrap() {
      Some(ParsedEvent::ResultGenerated { segment, duration_ms }) => {
        assert_eq!(segment.text, "体温三十八度");
        assert_eq!(duration_ms, None, "a broken usage must read as 'no usage', not as a value");
      }
      other => panic!("expected ResultGenerated, got {other:?}"),
    }
  }

  #[test]
  fn fractional_usage_seconds_are_kept() {
    // provider 回过小数秒;用整数类型接会让整条 usage 解析失败(并在合并解析时连转写一起丢)。
    let msg = Message::Text(
      r#"{"header":{"event":"result-generated"},"payload":{"output":{"sentence":{"text":"好","sentence_end":true}},"usage":{"duration":2.5}}}"#
        .into(),
    );
    match parse_server_event(&msg).unwrap() {
      Some(ParsedEvent::ResultGenerated { duration_ms, .. }) => assert_eq!(duration_ms, Some(2_500)),
      other => panic!("expected ResultGenerated, got {other:?}"),
    }
  }

  #[test]
  fn a_negative_usage_duration_is_treated_as_absent() {
    let msg =
      Message::Text(r#"{"header":{"event":"task-finished"},"payload":{"output":{},"usage":{"duration":-1}}}"#.into());
    match parse_server_event(&msg).unwrap() {
      Some(ParsedEvent::Finished { duration_ms }) => assert_eq!(duration_ms, None),
      other => panic!("expected Finished, got {other:?}"),
    }
  }

  #[test]
  fn parses_task_finished_with_usage_duration() {
    let msg = Message::Text(
      r#"{"header":{"task_id":"abc","event":"task-finished","attributes":{}},"payload":{"output":{},"usage":{"duration":7}}}"#
        .into(),
    );
    match parse_server_event(&msg).unwrap() {
      Some(ParsedEvent::Finished { duration_ms }) => assert_eq!(duration_ms, Some(7_000)),
      other => panic!("expected Finished, got {other:?}"),
    }
  }

  #[test]
  fn task_finished_without_usage_still_finishes() {
    // 拿不到用量不该让一次成功的转写失败 —— 计量 best-effort,转写不是。
    let msg = Message::Text(
      r#"{"header":{"task_id":"abc","event":"task-finished","attributes":{}},"payload":{"output":{},"usage":null}}"#
        .into(),
    );
    match parse_server_event(&msg).unwrap() {
      Some(ParsedEvent::Finished { duration_ms }) => assert_eq!(duration_ms, None),
      other => panic!("expected Finished, got {other:?}"),
    }
  }

  #[test]
  fn parses_task_failed() {
    let msg = Message::Text(
      r#"{"header":{"task_id":"abc","event":"task-failed","error_code":"CLIENT_ERROR","error_message":"bad param","attributes":{}},"payload":{}}"#
        .into(),
    );
    match parse_server_event(&msg).unwrap() {
      Some(ParsedEvent::Failed { code, message }) => {
        assert_eq!(code, "CLIENT_ERROR");
        assert_eq!(message, "bad param");
      }
      other => panic!("expected Failed, got {other:?}"),
    }
  }

  #[test]
  fn event_kind_name_never_exposes_the_transcript() {
    let seg = TranscriptSegment {
      text: "张奶奶体温三十八度".into(),
      begin_ms: None,
      end_ms: None,
      confidence: None,
      words: Vec::new(),
      is_final: false,
    };
    let ev = ParsedEvent::ResultGenerated { segment: seg, duration_ms: None };
    assert_eq!(ev.kind_name(), "result-generated");
    assert!(!ev.kind_name().contains("张"));
  }

  #[test]
  fn classifies_retryable_codes() {
    assert!(is_provider_retryable("Throttling.RateQuota"));
    assert!(is_provider_retryable("Throttling"));
    assert!(is_provider_retryable("InternalError"));
    assert!(is_provider_retryable("InternalError.Algo"), "带后缀的内部错误同样可重试");
    assert!(is_provider_retryable("RequestTimeout"));
    assert!(!is_provider_retryable("InvalidParameter"));
    assert!(!is_provider_retryable("AuthenticationFailed"));
  }

  // ---- region × model 校验 ----

  #[test]
  fn singapore_rejects_non_fun_asr_models() {
    // 新加坡地域的模型列表不含 Paraformer;放行会让音频出境后才失败。
    let err = validate_model_for_region("paraformer-realtime-v2", DashScopeRegion::Singapore).unwrap_err();
    match err {
      SpeechToTextError::ConfigInvalid(msg) => {
        assert!(msg.contains("singapore"), "{msg}");
        assert!(msg.contains("paraformer-realtime-v2"), "{msg}");
      }
      other => panic!("expected ConfigInvalid, got {other:?}"),
    }
  }

  #[test]
  fn singapore_accepts_fun_asr_family() {
    validate_model_for_region(DEFAULT_REALTIME_ASR_MODEL, DashScopeRegion::Singapore).unwrap();
    validate_model_for_region("fun-asr-realtime-2025", DashScopeRegion::Singapore).unwrap();
  }

  #[test]
  fn beijing_is_not_constrained_to_fun_asr() {
    // 北京地域两族都在售;本校验只挡"目标地域必然失败"的组合,不做产品选型。
    validate_model_for_region("paraformer-realtime-v2", DashScopeRegion::Beijing).unwrap();
    validate_model_for_region(DEFAULT_REALTIME_ASR_MODEL, DashScopeRegion::Beijing).unwrap();
  }

  #[tokio::test]
  async fn transcribe_rejects_region_model_mismatch_before_connecting() {
    // 校验 MUST 在建连之前:endpoint 不可达也应拿到 ConfigInvalid 而非 Network。
    let p = provider_in(DashScopeRegion::Singapore).with_model("paraformer-realtime-v2");
    match expect_err(p.transcribe_realtime(empty_uplink(), cfg()).await) {
      SpeechToTextError::ConfigInvalid(msg) => assert!(msg.contains("singapore"), "{msg}"),
      other => panic!("expected ConfigInvalid, got {other:?}"),
    }
  }

  // ---- region endpoint ----

  #[test]
  fn region_selects_the_matching_websocket_endpoint() {
    assert_eq!(
      DashScopeRegion::Singapore.websocket_endpoint(),
      "wss://dashscope-intl.aliyuncs.com/api-ws/v1/inference"
    );
    assert_eq!(DashScopeRegion::Beijing.websocket_endpoint(), "wss://dashscope.aliyuncs.com/api-ws/v1/inference");
    let p = provider_in(DashScopeRegion::Singapore);
    assert_eq!(p.region(), DashScopeRegion::Singapore);
    assert_eq!(p.model(), DEFAULT_REALTIME_ASR_MODEL);
    assert_eq!(p.provider_name(), PROVIDER_NAME);
    assert!(!p.supports_batch());
  }

  // ---- 建连前校验 ----

  #[tokio::test]
  async fn rejects_stereo_config() {
    let config = AudioStreamConfig { channels: 2, ..cfg() };
    match expect_err(provider().transcribe_realtime(empty_uplink(), config).await) {
      SpeechToTextError::ConfigInvalid(msg) => assert!(msg.contains("channels=2"), "{msg}"),
      other => panic!("expected ConfigInvalid, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn rejects_zero_sample_rate() {
    let config = AudioStreamConfig { sample_rate: 0, ..cfg() };
    assert!(matches!(
      expect_err(provider().transcribe_realtime(empty_uplink(), config).await),
      SpeechToTextError::ConfigInvalid(_)
    ));
  }

  #[test]
  fn rejects_encodings_that_would_silently_decode_to_noise() {
    // f32 当 s16、WebM 容器当裸 Opus —— provider 不报错,只产出垃圾转写。
    assert!(provider_audio_format(AudioEncoding::PcmF32Le).is_err(), "f32 MUST NOT pass through as pcm");
    assert!(provider_audio_format(AudioEncoding::WebmOpus).is_err(), "WebM container MUST NOT pass through as opus");
    assert!(provider_audio_format(AudioEncoding::G711Ulaw).is_err());
    assert!(provider_audio_format(AudioEncoding::G711Alaw).is_err());
    assert_eq!(provider_audio_format(AudioEncoding::PcmS16Le).unwrap(), "pcm");
    assert_eq!(provider_audio_format(AudioEncoding::Opus).unwrap(), "opus");
  }

  #[tokio::test]
  async fn rejects_over_limit_context_before_connecting() {
    let too_many =
      AudioStreamConfig { context_items: (0..MAX_CONTEXT_ITEMS + 1).map(|i| format!("t{i}")).collect(), ..cfg() };
    match expect_err(provider().transcribe_realtime(empty_uplink(), too_many).await) {
      SpeechToTextError::ConfigInvalid(msg) => assert!(msg.contains("context items"), "{msg}"),
      other => panic!("expected ConfigInvalid, got {other:?}"),
    }

    let too_long = AudioStreamConfig { context_items: vec!["体".repeat(MAX_CONTEXT_ITEM_CHARS + 1)], ..cfg() };
    match expect_err(provider().transcribe_realtime(empty_uplink(), too_long).await) {
      SpeechToTextError::ConfigInvalid(msg) => assert!(msg.contains("chars"), "{msg}"),
      other => panic!("expected ConfigInvalid, got {other:?}"),
    }
  }

  #[test]
  fn context_validation_accepts_exactly_the_limit() {
    let exact: Vec<String> = (0..MAX_CONTEXT_ITEMS).map(|i| format!("t{i}")).collect();
    validate_context_items(&exact).unwrap();
    validate_context_items(&["体".repeat(MAX_CONTEXT_ITEM_CHARS)]).unwrap();
    // 空白项不计入条数上限。
    let mut with_blanks = exact.clone();
    with_blanks.push("   ".to_string());
    validate_context_items(&with_blanks).unwrap();
  }

  // ---- run-task 序列化 ----

  #[test]
  fn run_task_carries_the_fun_asr_envelope() {
    let v = run_task_json(&cfg());
    assert_eq!(v["header"]["action"], "run-task");
    assert_eq!(v["header"]["streaming"], "duplex");
    assert_eq!(v["payload"]["model"], DEFAULT_REALTIME_ASR_MODEL);
    assert_eq!(v["payload"]["task_group"], "audio");
    assert_eq!(v["payload"]["task"], "asr");
    assert_eq!(v["payload"]["function"], "recognition");
    assert_eq!(v["payload"]["parameters"]["sample_rate"], 16_000);
    assert_eq!(v["payload"]["parameters"]["format"], "pcm");
  }

  #[test]
  fn empty_context_serializes_input_as_an_empty_object() {
    // 无上下文时 `input` MUST 退化为 `{}`（与旧 Paraformer 信封同形），
    // 而不是 `{"context":[]}` —— 空数组在 provider 侧的语义未定义。
    let v = run_task_json(&cfg());
    assert_eq!(v["payload"]["input"], serde_json::json!({}));
  }

  #[test]
  fn context_items_serialize_into_input_context() {
    let config = AudioStreamConfig {
      context_items: vec!["血氧饱和度".to_string(), "  ".to_string(), "利伐沙班".to_string()],
      ..cfg()
    };
    let v = run_task_json(&config);
    assert_eq!(
      v["payload"]["input"]["context"],
      serde_json::json!([{ "text": "血氧饱和度" }, { "text": "利伐沙班" }]),
      "空白项 MUST 被丢弃"
    );
  }

  #[test]
  fn vocabulary_ids_ride_their_own_parameter_not_context() {
    // 两条正交能力:已注册词表 id 走 vocabulary_id,临时术语走 input.context。
    let config = AudioStreamConfig { vocabulary_ids: vec!["vocab-1".to_string()], ..cfg() };
    let v = run_task_json(&config);
    assert_eq!(v["payload"]["parameters"]["vocabulary_id"], serde_json::json!(["vocab-1"]));
    assert_eq!(v["payload"]["input"], serde_json::json!({}));
  }

  #[test]
  fn provider_options_override_named_flags_and_pass_the_rest_through() {
    let config = AudioStreamConfig {
      provider_options: serde_json::json!({
        "punctuation_prediction_enabled": false,
        "max_sentence_silence": 800
      }),
      ..cfg()
    };
    let v = run_task_json(&config);
    assert_eq!(v["payload"]["parameters"]["punctuation_prediction_enabled"], false);
    assert_eq!(v["payload"]["parameters"]["inverse_text_normalization_enabled"], true, "未覆盖的保持默认");
    assert_eq!(v["payload"]["parameters"]["max_sentence_silence"], 800, "非保留 key 透传到 extra");
  }

  #[tokio::test]
  async fn colliding_provider_options_are_rejected_before_connecting() {
    for key in ["sample_rate", "format", "language_hints", "vocabulary_id"] {
      let config = AudioStreamConfig { provider_options: serde_json::json!({ key: "whatever" }), ..cfg() };
      match expect_err(provider().transcribe_realtime(empty_uplink(), config).await) {
        SpeechToTextError::ConfigInvalid(msg) => assert!(msg.contains(key), "{msg}"),
        other => panic!("expected ConfigInvalid for {key}, got {other:?}"),
      }
    }
    // 具名布尔覆盖是刻意支持的用法,不该被这条校验挡住。
    let ok =
      AudioStreamConfig { provider_options: serde_json::json!({ "punctuation_prediction_enabled": false }), ..cfg() };
    validate_provider_options(&ok.provider_options).unwrap();
  }

  #[test]
  fn domain_hint_collision_is_rejected_like_any_other_named_parameter() {
    // `build_run_task` 用 config.domain_hint 无条件覆盖同名 extra key —— 放行等于让调用方
    // 的值被静默吃掉,而"撞名一律拒绝"的规则说好了不这么干。
    let err = validate_provider_options(&serde_json::json!({ "domain_hint": "finance" })).unwrap_err();
    assert!(format!("{err}").contains("domain_hint"), "{err}");
  }

  #[test]
  fn a_named_boolean_override_given_a_non_boolean_is_rejected() {
    // `"false"`(字符串)是 JSON 配置里最常见的写法。放行的话:as_bool() 回 None → 退回默认
    // 值,同时该 key 又因在 RESERVED 里被剔出 extra —— 既没生效也没报错,不可诊断。
    for value in [serde_json::json!("false"), serde_json::json!(0), serde_json::json!(null)] {
      let options = serde_json::json!({ "punctuation_prediction_enabled": value });
      let err = validate_provider_options(&options).unwrap_err();
      let msg = format!("{err}");
      assert!(msg.contains("punctuation_prediction_enabled"), "{msg}");
      assert!(msg.contains("must be a boolean"), "{msg}");
    }
  }

  #[test]
  fn provider_options_that_are_not_an_object_are_rejected() {
    // build_run_task 只认 object,其余整体丢弃 —— 一份写成数组或 JSON 字符串的配置会让
    // **全部**参数静默失效,连 provider 都到不了。
    for options in [
      serde_json::json!([{ "max_sentence_silence": 800 }]),
      serde_json::json!("{\"max_sentence_silence\":800}"),
      serde_json::json!(42),
    ] {
      let err = validate_provider_options(&options).unwrap_err();
      assert!(format!("{err}").contains("must be a JSON object"), "{err}");
    }
    // 缺省(Null)是"不覆盖任何东西",合法。
    validate_provider_options(&serde_json::Value::Null).unwrap();
  }

  #[test]
  fn language_hints_are_reduced_to_primary_subtags() {
    // 调用方的 locale 可能是 `zh-CN`,而 provider 的取值表是 `zh` 这一级。带地区的 BCP-47 标签
    // MUST NOT 因此被拒,也 MUST NOT 原样下发。
    assert_eq!(primary_language_subtags(&["zh-CN".into(), "en-US".into()]), vec!["zh", "en"]);
    // 归一后重复的标签合并,顺序保持。
    assert_eq!(primary_language_subtags(&["zh-CN".into(), "zh-TW".into(), "en".into()]), vec!["zh", "en"]);
    // 空白项整条丢弃,不产出空标签。
    assert_eq!(primary_language_subtags(&["".into(), "  ".into(), "JA".into()]), vec!["ja"]);

    let config = AudioStreamConfig { language_hints: vec!["zh-CN".into()], ..cfg() };
    let v: serde_json::Value = serde_json::from_str(&run_task_wire(&config)).unwrap();
    assert_eq!(v["payload"]["parameters"]["language_hints"], serde_json::json!(["zh"]));
  }

  #[test]
  fn colliding_provider_options_never_produce_duplicate_wire_keys() {
    // 即使校验被绕过,build_run_task 的过滤仍是最后一道防线:
    // `#[serde(flatten)]` + `to_string` 不会去重,撞名 key 会真的出现两次。
    let config = AudioStreamConfig {
      provider_options: serde_json::json!({ "sample_rate": 8000, "format": "wav", "language_hints": ["ja"] }),
      ..cfg()
    };
    let wire = run_task_wire(&config);
    assert_eq!(wire.matches("\"sample_rate\"").count(), 1, "duplicate sample_rate on the wire: {wire}");
    assert_eq!(wire.matches("\"format\"").count(), 1, "duplicate format on the wire: {wire}");
    assert_eq!(wire.matches("\"language_hints\"").count(), 1, "duplicate language_hints on the wire: {wire}");
    let v: serde_json::Value = serde_json::from_str(&wire).unwrap();
    assert_eq!(v["payload"]["parameters"]["sample_rate"], 16_000, "具名字段 MUST 胜出");
    assert_eq!(v["payload"]["parameters"]["format"], "pcm");
  }

  #[test]
  fn reserved_option_keys_cover_every_named_parameter() {
    // 漏掉任一具名字段就会重新打开重复 key 的口子。
    let v = serde_json::to_value(build_run_task("t", "m".into(), &cfg(), "pcm")).unwrap();
    let named: Vec<String> = v["payload"]["parameters"].as_object().unwrap().keys().cloned().collect();
    for key in &named {
      assert!(RESERVED_OPTION_KEYS.contains(&key.as_str()), "named parameter {key} is not reserved");
    }
    for key in BOOL_OVERRIDE_KEYS {
      assert!(RESERVED_OPTION_KEYS.contains(key), "{key} must also be reserved");
    }
  }

  // ---- 上下文裁剪(会话中) ----

  #[test]
  fn clamp_context_keeps_the_most_recent_items() {
    let items: Vec<String> = (1..=8).map(|i| format!("item-{i}")).collect();
    let clamped = clamp_context(&items);
    assert_eq!(clamped.len(), MAX_CONTEXT_ITEMS);
    // 保留的 MUST 是最近的,不是最早的 —— 会话中最新确认的术语才是最有用的。
    assert_eq!(clamped[0].text, "item-4");
    assert_eq!(clamped[MAX_CONTEXT_ITEMS - 1].text, "item-8");
  }

  #[test]
  fn clamp_context_keeps_exactly_the_limit_untouched() {
    let items: Vec<String> = (1..=MAX_CONTEXT_ITEMS).map(|i| format!("item-{i}")).collect();
    assert_eq!(clamp_context(&items).len(), MAX_CONTEXT_ITEMS);
    let exact = "体".repeat(MAX_CONTEXT_ITEM_CHARS);
    assert_eq!(clamp_context(std::slice::from_ref(&exact))[0].text, exact);
  }

  #[test]
  fn clamp_context_truncates_by_chars_not_bytes() {
    // 中文 1 字 3 字节:按字节截会切出半个字符。
    let long: String = "体".repeat(MAX_CONTEXT_ITEM_CHARS + 50);
    let clamped = clamp_context(&[long]);
    assert_eq!(clamped.len(), 1);
    assert_eq!(clamped[0].text.chars().count(), MAX_CONTEXT_ITEM_CHARS);
  }

  #[test]
  fn clamp_context_drops_blank_items_entirely() {
    assert!(clamp_context(&["".to_string(), "   ".to_string()]).is_empty());
  }

  // ---- continue-task / finish-task 序列化 ----

  #[test]
  fn continue_task_carries_the_replacement_context() {
    let msg = ClientMessage {
      header: ClientHeader { action: "continue-task", task_id: "task-1".to_string(), streaming: "duplex" },
      payload: ContinueTaskPayload { input: TaskInput { context: clamp_context(&["利伐沙班".to_string()]) } },
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["header"]["action"], "continue-task");
    assert_eq!(v["header"]["task_id"], "task-1");
    assert_eq!(v["payload"]["input"]["context"], serde_json::json!([{ "text": "利伐沙班" }]));
  }

  #[test]
  fn finish_task_sends_an_empty_input() {
    let msg = ClientMessage {
      header: ClientHeader { action: "finish-task", task_id: "task-1".to_string(), streaming: "duplex" },
      payload: FinishTaskPayload { input: TaskInput::default() },
    };
    let v = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["header"]["action"], "finish-task");
    assert_eq!(v["payload"]["input"], serde_json::json!({}));
  }
}
