//! 通用流式语音识别(STT)抽象。
//!
//! 区别于批量文件转写(单次 HTTP `multipart/form-data` 调用,见
//! [`crate::providers::openai_compatible::transcription`]),本模块面向**双向流 /
//! 长连接**的实时 STT 协议 (WebSocket / gRPC streaming),适配阿里云 Fun-ASR 实时、
//! OpenAI Realtime transcription、讯飞实时转写、sherpa-onnx 等。
//!
//! 会话是**双向**的:上行不只是音频,还包括会话中的控制指令(如上下文增强词表更新),
//! 故 [`SpeechToText::transcribe_realtime`] 收的是 [`SttUplinkStream`] 而非纯音频流。
//! 只推音频的调用方用 [`SttUplink::from_audio`] 提升即可;音频 + 控制两条流用
//! [`SttUplink::merge`] 合并。
//!
//! # PHI 纪律
//!
//! 音频与转写文本是**受保护健康信息**。本模块的公共类型一律手写 `Debug`,只暴露形状与长度,
//! **绝不**暴露音频字节或转写内容 —— 下游一句 `tracing::debug!(?x)` 或一次 panic 的 payload
//! 打印就会把 PHI 写进日志,而那是最难事后追回的一类泄漏。需要看内容时 MUST 显式访问字段。
//!
//! 设计参考 `docs/designs/ai/voice-ai-tech-design.md` 附录 A。

use std::fmt;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

/// 音频编码格式。流式 STT 通常只支持 PCM/Opus,文件 batch 接口接受更多格式。
///
/// **本枚举只描述调用方手上的字节是什么**,不承诺任一 provider 都收得下:各 provider 在
/// 建连前拒绝自己不支持的编码(MUST NOT 静默把 f32 当 s16、把容器当裸流直通 —— 那会产出
/// "链路跑通但转写是噪声"的最难诊断故障)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AudioEncoding {
  /// 16-bit 有符号小端 PCM。
  PcmS16Le,
  /// 32-bit 浮点小端 PCM。**与 `PcmS16Le` 不可互换**——多数 provider 的 `pcm` 指 s16le。
  PcmF32Le,
  G711Ulaw,
  G711Alaw,
  /// 裸 Opus 包(非容器)。
  Opus,
  /// WebM 容器封装的 Opus。**与 `Opus` 不可互换**——容器不是裸流。
  WebmOpus,
  Wav,
  Mp3,
  Aac,
  Amr,
}

// 这里刻意**没有** `as_provider_str`:一个「字面名映射」在两个 provider 之前无从归纳,而唯一
// 实装的 provider 需要的恰恰不是它 —— `PcmF32Le → "pcm"` / `WebmOpus → "opus"` 正是
// `fun_asr::provider_audio_format` 要 fail-closed 拒绝的两组混淆。真要下发什么名字,由各
// provider 自己的可预检函数回答。

/// 实时 STT 会话的音频流配置。
///
/// # 演进纪律
///
/// 本结构的字段会随 provider 能力增长,**新增字段视为破坏性变更**。之所以不加
/// `#[non_exhaustive]`:那会同时禁掉 `..base` 更新语法,而它正是本结构最主要的用法。
/// 调用方 SHOULD 用 `AudioStreamConfig { channels: 2, ..AudioStreamConfig::pcm_s16le_16k_mono_40ms() }`
/// 这种写法构造,以免每次字段增补都要改代码。
#[derive(Clone)]
pub struct AudioStreamConfig {
  pub encoding: AudioEncoding,
  pub sample_rate: u32,
  pub channels: u16,
  /// 单帧时长(ms)。
  ///
  /// **仅供调用方自查 / 审计**:本值既不下发给 provider,实现也不会据此重新切帧。真实分帧
  /// 完全取决于调用方往 [`SttUplinkStream`] 里推什么。0 = 未声明。
  pub frame_duration_ms: u16,
  /// 语言候选,BCP-47 标签,如 `["zh", "en"]` 或 `["zh-CN"]`。
  ///
  /// 只认主语言子标签的 provider 由实现取主子标签下发(`zh-CN` → `zh`),调用方无需自行裁剪;
  /// 带地区的标签 MUST NOT 因此被拒。
  pub language_hints: Vec<String>,
  /// **Provider 侧已注册词表的 ID 列表**,不是词条本身。
  ///
  /// 命名易误读,故明说:填 `"利伐沙班"` 不会生效 —— 那是词,不是词表 ID。会话内的临时术语
  /// 走 [`Self::context_items`]。无词表机制的 provider 上本字段是静默 no-op。
  ///
  /// **未实测**:DashScope 侧 `vocabulary_id` 的 wire 形态(数组 vs 单值)沿用 Paraformer
  /// 实装期的数组形态,尚未对着实跑的 provider 验证。首次接入真实凭证时 MUST 实测确认 ——
  /// 形态不符的表现是 provider 报 `InvalidParameter`,或更糟:静默不生效。
  pub vocabulary_ids: Vec<String>,
  /// 上下文增强词表(词表匹配式修正:每条 MUST 包含待识别原词,纯语义描述效果有限)。
  /// 与 [`Self::vocabulary_ids`] 是两条正交能力,可同时使用。
  ///
  /// **条数与单条长度上限由 provider 决定**(如 DashScope Fun-ASR:最多 5 条、每条 ≤400 字符),
  /// 超限在建连前以 [`SpeechToTextError::ConfigInvalid`] 拒绝 —— 静默裁剪会让上下文增强
  /// 变成"有时生效有时不生效"的玄学。会话中可经 [`SttUplink::ContextUpdate`] 整体替换。
  ///
  /// 隐私:MUST NOT 放真实对象名册(姓名 / 床号全集)—— 这份列表会离开本进程送到 provider。
  pub context_items: Vec<String>,
  /// 业务领域提示,例如 `"healthcare_nursing"`。
  pub domain_hint: Option<String>,
  /// Provider 特有扩展参数(走 `serde_json::Value` 透传)。
  ///
  /// 与 provider 具名参数同名的 key 会被拒绝或忽略(各 provider 自行声明),MUST NOT 指望用
  /// 它覆盖 [`Self::sample_rate`] 一类已有字段。
  pub provider_options: serde_json::Value,
}

/// 手写 `Debug`:`context_items` 与 `provider_options` 是**会离开本进程送到 provider** 的内容,
/// 前者按约定只放领域术语,但同一份词表在 [`SttUplink::ContextUpdate`] 上是脱敏的 —— 两条路径
/// 对同样的数据给出不同的打印结果,等于给 `tracing::debug!(?config)` 留了一个后门。故本结构
/// 与本模块其余公共类型同规:只报形状与长度。
impl fmt::Debug for AudioStreamConfig {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("AudioStreamConfig")
      .field("encoding", &self.encoding)
      .field("sample_rate", &self.sample_rate)
      .field("channels", &self.channels)
      .field("frame_duration_ms", &self.frame_duration_ms)
      .field("language_hints", &self.language_hints)
      .field("vocabulary_ids_len", &self.vocabulary_ids.len())
      .field("context_items_len", &self.context_items.len())
      .field("has_domain_hint", &self.domain_hint.is_some())
      .field("has_provider_options", &!self.provider_options.is_null())
      .finish()
  }
}

impl AudioStreamConfig {
  /// PCM s16le 16k mono 40ms frame 默认配置,匹配浏览器 AudioWorklet 链路。
  ///
  /// 除音频参数外还预置 `language_hints = ["zh", "en"]`;做其他语种时 MUST 显式覆盖。
  pub fn pcm_s16le_16k_mono_40ms() -> Self {
    Self {
      encoding: AudioEncoding::PcmS16Le,
      sample_rate: 16_000,
      channels: 1,
      frame_duration_ms: 40,
      language_hints: vec!["zh".to_string(), "en".to_string()],
      vocabulary_ids: Vec::new(),
      context_items: Vec::new(),
      domain_hint: None,
      provider_options: serde_json::Value::Null,
    }
  }
}

/// 单段转录结果(对应阿里 result-generated 的一个 sentence)。
#[derive(Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TranscriptSegment {
  pub text: String,
  /// 相对**会话起点**的开始时间(ms)。
  pub begin_ms: Option<u64>,
  /// 相对会话起点的结束时间(ms);流式中间结果可能为 None。
  pub end_ms: Option<u64>,
  /// 段级置信度(provider 提供时填入)。
  pub confidence: Option<f32>,
  /// 词级切分(provider 提供时填入)。
  pub words: Vec<TranscriptWord>,
  /// 是否为该段的最终(`sentence_end=true`)。
  pub is_final: bool,
}

/// PHI 纪律:只暴露形状,不暴露转写内容。
impl fmt::Debug for TranscriptSegment {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("TranscriptSegment")
      .field("text", &format_args!("<{} chars redacted>", self.text.chars().count()))
      .field("begin_ms", &self.begin_ms)
      .field("end_ms", &self.end_ms)
      .field("confidence", &self.confidence)
      .field("words", &self.words.len())
      .field("is_final", &self.is_final)
      .finish()
  }
}

#[derive(Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TranscriptWord {
  pub text: String,
  pub begin_ms: Option<u64>,
  pub end_ms: Option<u64>,
  pub punctuation: Option<String>,
}

/// PHI 纪律:同 [`TranscriptSegment`]。
impl fmt::Debug for TranscriptWord {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("TranscriptWord")
      .field("text", &format_args!("<{} chars redacted>", self.text.chars().count()))
      .field("begin_ms", &self.begin_ms)
      .field("end_ms", &self.end_ms)
      .finish()
  }
}

/// 一次会话的最终转录结果(任务结束时合成)。
#[derive(Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TranscriptionResult {
  /// 拼接后的完整文本。
  pub text: String,
  pub language: Option<String>,
  pub confidence: Option<f32>,
  pub segments: Vec<TranscriptSegment>,
  /// Provider 短名(`fun_asr_realtime` / `openai_realtime_transcription` 等)。
  pub provider: String,
  /// 实际生效的模型名 —— 计量与审计标签需要它,而持 `dyn SpeechToText` 的调用方
  /// 拿不到 provider 的固有方法。
  pub model: String,
  /// Provider 会话 ID(task_id / session_id),便于审计追踪。
  pub provider_session_id: Option<String>,
  /// 本次会话的音频时长(ms)。STT 的可计费维度是**时长**而非 token。
  ///
  /// **粒度由 provider 决定**(DashScope 只到秒级,故该值恒为 1000 的整数倍),MUST NOT 当作
  /// 毫秒精度使用。provider 未回时长则为 `None`,此时调用方 MUST NOT 编造一个值。
  ///
  /// 会话失败不会产出 [`SttEvent::TaskFinished`],因此**失败会话没有时长可计** —— 部分用量
  /// 无法计量是 best-effort 计量的既定边界。
  pub audio_duration_ms: Option<u64>,
}

/// PHI 纪律:只暴露形状,不暴露转写内容。
impl fmt::Debug for TranscriptionResult {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("TranscriptionResult")
      .field("text", &format_args!("<{} chars redacted>", self.text.chars().count()))
      .field("language", &self.language)
      .field("confidence", &self.confidence)
      .field("segments", &self.segments.len())
      .field("provider", &self.provider)
      .field("model", &self.model)
      .field("provider_session_id", &self.provider_session_id)
      .field("audio_duration_ms", &self.audio_duration_ms)
      .finish()
  }
}

/// 流式 STT 事件。
///
/// # 错误只有一条通道
///
/// Provider 端错误 MUST 以流的 `Err(SpeechToTextError)` 上报,本枚举**不含**错误变体 ——
/// 「同一次失败既发事件又发 Err」会让消费方重复告警、重复计数,而「只发事件不发 Err」又让
/// 错误无法沿 `?` 传播。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum SttEvent {
  /// 服务端确认 session 建立。
  ///
  /// 注意:上行流在 [`SpeechToText::transcribe_realtime`] 调用时就已交给实现,**调用方无法
  /// "等 Started 再推音频"**。实现负责在 session 建立前不丢弃上行项。
  Started { provider_session_id: Option<String> },
  /// 增量识别结果(`sentence_end=false`)。
  Partial(TranscriptSegment),
  /// 段最终结果(`sentence_end=true`,可能后续仍有新段)。
  SegmentFinal(TranscriptSegment),
  /// 任务结束的合并结果。收到它之后流 MUST 结束。
  TaskFinished(TranscriptionResult),
}

/// 流式 STT 错误。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SpeechToTextError {
  /// 调用方给的配置本身不合法(声道数、编码、上下文超限、模型与地域不匹配……)。
  /// 改调用方的代码或配置即可,重试无用。
  #[error("invalid config: {0}")]
  ConfigInvalid(String),

  /// 本 provider 不具备该能力(如只支持流式、不支持批量)。与 [`Self::ConfigInvalid`] 分开是
  /// 为了让"换一个 provider"与"改自己的参数"成为两个可判定的分支。
  ///
  /// `provider` 与 [`Self::Provider`] 同名同型(`String`):同一个概念在同一个 enum 里出现两种
  /// 类型,消费者写起来就要为「哪个变体」而记不同的写法。
  #[error("provider {provider} does not support {capability}")]
  Unsupported { provider: String, capability: &'static str },

  #[error("auth failed: {0}")]
  Auth(String),

  #[error("network: {0}")]
  Network(String),

  /// Provider 的协议输出无法解析。**MUST NOT 与"可忽略的非业务帧"混用同一变体** ——
  /// 把解析失败当可忽略帧跳过会静默丢失识别结果。
  #[error("protocol: {0}")]
  Protocol(String),

  #[error("provider {provider} returned {code}: {message}")]
  Provider { provider: String, code: String, message: String, retryable: bool },

  #[error("timeout: {0}")]
  Timeout(String),

  // 这里刻意**没有** `Cancelled`:取消在本抽象里不是一种"错误",而是消费者 drop 事件流 ——
  // 流就此结束,没有终态错误可产出(上行泵的 abort 语义由 session 测试守住)。留一个没有任何
  // 构造点的变体,只会让每个消费者为一件不会发生的事写一条 match 分支。
  #[error(transparent)]
  Other(#[from] anyhow::Error),
}

impl SpeechToTextError {
  /// 调用方据此决定是否换 provider 重放。
  ///
  /// 注意:**只对未产出 [`SttEvent::TaskFinished`] 的会话有意义**。已经拿到最终结果之后
  /// 出现的收尾错误 MUST NOT 触发重放 —— 重放一段已成功的音频会产生重复业务记录。实现
  /// 保证不在 `TaskFinished` 之后再产出终态错误。
  pub fn is_retryable(&self) -> bool {
    matches!(self, Self::Network(_) | Self::Timeout(_) | Self::Provider { retryable: true, .. })
  }

  /// 这次失败是**调用方**造成的吗?
  ///
  /// 与 [`Self::is_retryable`] 正交,回答的是另一个问题:网关该报 `invalid_argument`(你的
  /// 请求有问题)还是 `internal`(我们坏了)。没有它,网关只能写
  /// `if is_retryable() { unavailable } else { internal }`,于是 provider 回的
  /// `InvalidParameter` 变成 500 —— 运维当作服务缺陷立单,而实际上要改的是租户配置。
  ///
  /// `Provider` 的归类跟随 provider 自己的错误码表:不可重试的 provider 错误几乎都是参数
  /// 或配额问题;可重试的是对侧的暂时状态,不是调用方的错。
  pub fn is_caller_fault(&self) -> bool {
    match self {
      Self::ConfigInvalid(_) | Self::Unsupported { .. } | Self::Auth(_) => true,
      Self::Provider { retryable, .. } => !retryable,
      Self::Network(_) | Self::Protocol(_) | Self::Timeout(_) | Self::Other(_) => false,
    }
  }
}

// 这里刻意**没有** `AudioFrameStream` 别名:上行流的公共形状是 [`SttUplinkStream`],纯音频
// 只是它的一个来源([`SttUplink::from_audio`] 收任意 `Stream<Item = Bytes>`,不要求装箱)。
// 多一个没有任何签名用到的别名,只是让读者多猜一次两者的关系。

/// 实时识别会话的**上行**项:音频帧,或会话中的控制指令。
///
/// 会话上行不止音频 —— 上下文增强词表可在识别过程中更新。把两者放同一条流上是刻意的:
/// provider 侧它们本就是同一条 WebSocket 上的先后消息,拆成两个入参会把顺序语义
/// (「这次更新在哪一帧之后生效」)交给调用方自己拼,而那正是最容易拼错的部分。
///
/// **刻意不加 `#[non_exhaustive]`**:唯一需要 `match` 本枚举的是 [`SpeechToText`] 的实现者,
/// 而控制通道恰恰最不该被 `_ => {}` 静默吞掉 —— 未来新增指令时,让各 provider 编译失败是
/// 特性不是缺陷。
#[derive(Clone)]
pub enum SttUplink {
  /// 一帧音频数据。空帧被实现忽略,MUST NOT 当作结束信号。
  Audio(Bytes),
  /// **整体替换**会话的上下文增强词表(非追加)。
  ///
  /// 空列表 = **不变更**(多数 provider 无"清空上下文"的表达);要换一批词就给新的一批。
  /// 隐私与限额约束同 [`AudioStreamConfig::context_items`],超限由实现裁剪并告警。
  ContextUpdate(Vec<String>),
}

/// PHI 纪律:音频字节与词表内容都不进 `Debug`。
impl fmt::Debug for SttUplink {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Audio(b) => f.debug_tuple("Audio").field(&format_args!("<{} bytes redacted>", b.len())).finish(),
      Self::ContextUpdate(items) => {
        f.debug_tuple("ContextUpdate").field(&format_args!("<{} items redacted>", items.len())).finish()
      }
    }
  }
}

impl SttUplink {
  /// 把纯音频帧流提升为上行流(不含任何控制指令)。
  ///
  /// ```ignore
  /// let events = provider.transcribe_realtime(SttUplink::from_audio(frames), cfg).await?;
  /// ```
  pub fn from_audio<S>(frames: S) -> SttUplinkStream
  where
    S: Stream<Item = Bytes> + Send + 'static,
  {
    Box::pin(frames.map(SttUplink::Audio))
  }

  /// 合并音频流与控制指令流为一条上行流(两侧就绪即发,任一侧结束不影响另一侧)。
  ///
  /// 全部结束后上行流结束,实现据此向 provider 发结束指令。
  pub fn merge<A, C>(audio: A, control: C) -> SttUplinkStream
  where
    A: Stream<Item = Bytes> + Send + 'static,
    C: Stream<Item = Vec<String>> + Send + 'static,
  {
    Box::pin(futures::stream::select(audio.map(SttUplink::Audio), control.map(SttUplink::ContextUpdate)))
  }

  /// 把一条**已经是** `SttUplink` 的流装箱成上行流。
  ///
  /// [`Self::from_audio`] 与 [`Self::merge`] 覆盖的是"两条独立来源"的形状;而当上行本就是
  /// 一条已保序的混合流(网关把音频与控制帧从同一个 WebSocket 上解出来时正是如此),
  /// 它们都不适用 —— 没有这个入口,调用方只能自己写 `Box::pin(..)`,把本模块的流类型细节
  /// 抄进业务代码。
  pub fn from_stream<S>(items: S) -> SttUplinkStream
  where
    S: Stream<Item = SttUplink> + Send + 'static,
  {
    Box::pin(items)
  }
}

/// 会话上行流的别名。只推音频的调用方用 [`SttUplink::from_audio`] 提升。
pub type SttUplinkStream = Pin<Box<dyn Stream<Item = SttUplink> + Send>>;

/// STT 事件流的别名。
pub type SttEventStream = Pin<Box<dyn Stream<Item = Result<SttEvent, SpeechToTextError>> + Send>>;

/// 通用流式 STT 抽象。Provider 实现按需选择是否支持批量。
#[async_trait]
pub trait SpeechToText: Send + Sync {
  /// Provider 短名,用于审计 / 指标标签,如 `"fun_asr_realtime"`。
  fn provider_name(&self) -> &'static str;

  /// 生效的模型名。持 `dyn SpeechToText` 的调用方靠它打计量与审计标签,MUST NOT 要求调用方
  /// 在业务侧另存一份(那必然与 provider 实际用的模型漂移)。
  fn model(&self) -> &str;

  /// 是否支持 [`Self::transcribe_batch`]。做 provider fallback 的调用方据此选择,
  /// MUST NOT 靠 call-and-catch 再匹配错误字符串。
  fn supports_batch(&self) -> bool {
    false
  }

  /// 启动一次实时识别 session。
  ///
  /// 调用方提供上行流(顺序、有限或无限),返回 STT 事件流。**上行流结束 = 音频说完**,
  /// 实现据此向 provider 发结束指令并等最终结果;要中途放弃则直接 drop 返回的事件流。
  ///
  /// 实现负责:鉴权握手、控制消息、帧编码、断流时关闭上游。
  ///
  /// # 失败时机
  ///
  /// 调用方**配置**层面的错误(编码 / 声道 / 上限 / 模型与地域不匹配)在本方法 `.await`
  /// 时即返回;建连、鉴权与协议错误在事件流**首次 poll** 时才产出 —— 拿到 `Ok(stream)`
  /// 不代表连接已建立,不 poll 就不会有任何网络动作。
  ///
  /// # Panics
  ///
  /// 返回的流 MUST 在 Tokio runtime 内 poll(实现内部用 `tokio::spawn` / `tokio::time`)。
  /// 在 `futures::executor::block_on` 一类非 Tokio 执行器上 poll 会 panic。
  async fn transcribe_realtime(
    &self,
    uplink: SttUplinkStream,
    config: AudioStreamConfig,
  ) -> Result<SttEventStream, SpeechToTextError>;

  /// 可选的批量文件接口(默认未实现,只为 OpenAI Whisper 等 batch-only provider 留口)。
  async fn transcribe_batch(
    &self,
    _audio: Bytes,
    _config: AudioStreamConfig,
  ) -> Result<TranscriptionResult, SpeechToTextError> {
    Err(SpeechToTextError::Unsupported {
      provider: self.provider_name().to_string(),
      capability: "batch transcription",
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn default_pcm_config_matches_browser_audio_worklet() {
    let cfg = AudioStreamConfig::pcm_s16le_16k_mono_40ms();
    assert_eq!(cfg.sample_rate, 16_000);
    assert_eq!(cfg.channels, 1);
    assert_eq!(cfg.frame_duration_ms, 40);
    assert_eq!(cfg.encoding, AudioEncoding::PcmS16Le);
    assert!(cfg.context_items.is_empty(), "上下文增强 MUST 默认为空:词表要由调用方显式给出");
    assert!(cfg.vocabulary_ids.is_empty());
  }

  #[tokio::test]
  async fn from_audio_lifts_every_frame_in_order() {
    let frames = futures::stream::iter(vec![Bytes::from_static(b"a"), Bytes::from_static(b"bb")]);
    let lifted: Vec<SttUplink> = SttUplink::from_audio(frames).collect().await;
    assert_eq!(lifted.len(), 2);
    match (&lifted[0], &lifted[1]) {
      (SttUplink::Audio(a), SttUplink::Audio(b)) => {
        assert_eq!(a.as_ref(), b"a");
        assert_eq!(b.as_ref(), b"bb");
      }
      other => panic!("expected two audio items, got {other:?}"),
    }
  }

  #[tokio::test]
  async fn merge_carries_both_sides() {
    let audio = futures::stream::iter(vec![Bytes::from_static(b"a")]);
    let control = futures::stream::iter(vec![vec!["term".to_string()]]);
    let merged: Vec<SttUplink> = SttUplink::merge(audio, control).collect().await;
    assert_eq!(merged.len(), 2);
    assert_eq!(merged.iter().filter(|i| matches!(i, SttUplink::Audio(_))).count(), 1);
    assert_eq!(merged.iter().filter(|i| matches!(i, SttUplink::ContextUpdate(_))).count(), 1);
  }

  #[test]
  fn retryable_classification() {
    assert!(SpeechToTextError::Network("disconnect".into()).is_retryable());
    assert!(SpeechToTextError::Timeout("rtt".into()).is_retryable());
    assert!(
      SpeechToTextError::Provider {
        provider: "x".into(),
        code: "rate_limit".into(),
        message: "throttle".into(),
        retryable: true,
      }
      .is_retryable()
    );
    assert!(!SpeechToTextError::Auth("bad key".into()).is_retryable());
    assert!(!SpeechToTextError::Unsupported { provider: "x".into(), capability: "batch" }.is_retryable());
    assert!(
      !SpeechToTextError::Provider {
        provider: "x".into(),
        code: "invalid".into(),
        message: "bad param".into(),
        retryable: false,
      }
      .is_retryable()
    );
  }

  // ---- PHI 纪律 ----

  #[test]
  fn uplink_debug_never_prints_audio_bytes_or_context_terms() {
    let audio = SttUplink::Audio(Bytes::from_static(b"\x01\x02SECRET-PCM"));
    let dbg = format!("{audio:?}");
    assert!(!dbg.contains("SECRET"), "audio bytes leaked: {dbg}");
    assert!(dbg.contains("12 bytes redacted"), "{dbg}");

    let ctx = SttUplink::ContextUpdate(vec!["利伐沙班".to_string()]);
    let dbg = format!("{ctx:?}");
    assert!(!dbg.contains("利伐沙班"), "context term leaked: {dbg}");
    assert!(dbg.contains("1 items redacted"), "{dbg}");
  }

  #[test]
  fn transcript_types_debug_never_print_the_text() {
    let word = TranscriptWord { text: "张".to_string(), begin_ms: Some(1), end_ms: Some(2), punctuation: None };
    let seg = TranscriptSegment {
      text: "张奶奶体温三十八度".to_string(),
      begin_ms: Some(0),
      end_ms: Some(100),
      confidence: Some(0.9),
      words: vec![word.clone()],
      is_final: true,
    };
    let result = TranscriptionResult {
      text: seg.text.clone(),
      language: None,
      confidence: None,
      segments: vec![seg.clone()],
      provider: "fun_asr_realtime".to_string(),
      model: "fun-asr-realtime".to_string(),
      provider_session_id: Some("task-1".to_string()),
      audio_duration_ms: Some(3_000),
    };
    for dbg in [format!("{word:?}"), format!("{seg:?}"), format!("{result:?}")] {
      assert!(!dbg.contains("张"), "transcript leaked: {dbg}");
      assert!(dbg.contains("redacted"), "{dbg}");
    }
    // A `TaskFinished` event is what a gateway would most naturally log — it must be clean too.
    let dbg = format!("{:?}", SttEvent::TaskFinished(result));
    assert!(!dbg.contains("张"), "transcript leaked through SttEvent: {dbg}");
    // Non-PHI metadata stays visible, otherwise the redaction defeats its own diagnostics purpose.
    assert!(dbg.contains("fun_asr_realtime") && dbg.contains("3000"), "{dbg}");
  }
}
