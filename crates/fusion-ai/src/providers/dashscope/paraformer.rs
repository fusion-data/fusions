//! 阿里云 DashScope **Paraformer 实时 v2** 流式语音识别。
//!
//! 协议:`wss://dashscope.aliyuncs.com/api-ws/v1/inference`
//! 控制消息 JSON:`run-task` / `finish-task`,音频以二进制帧上传,
//! 事件 `task-started` / `result-generated` / `task-finished` / `task-failed`。
//!
//! 参考:<https://help.aliyun.com/zh/model-studio/websocket-for-paraformer-real-time-service>

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
  AudioEncoding, AudioFrameStream, AudioStreamConfig, SpeechToText, SpeechToTextError, SttEvent, SttEventStream,
  TranscriptSegment, TranscriptWord, TranscriptionResult,
};

const PROVIDER_NAME: &str = "paraformer_realtime_v2";
const DEFAULT_MODEL: &str = "paraformer-realtime-v2";

/// Paraformer 实时 v2 client。
#[derive(Debug, Clone)]
pub struct ParaformerRealtimeV2 {
  credentials: Arc<DashScopeCredentials>,
  region: DashScopeRegion,
  model: String,
  /// `task-started` 等待超时。
  start_timeout: Duration,
}

impl ParaformerRealtimeV2 {
  pub fn new(credentials: DashScopeCredentials) -> Self {
    Self {
      credentials: Arc::new(credentials),
      region: DashScopeRegion::default(),
      model: DEFAULT_MODEL.to_string(),
      start_timeout: Duration::from_secs(10),
    }
  }

  pub fn with_region(mut self, region: DashScopeRegion) -> Self {
    self.region = region;
    self
  }

  pub fn with_model(mut self, model: impl Into<String>) -> Self {
    self.model = model.into();
    self
  }

  pub fn with_start_timeout(mut self, timeout: Duration) -> Self {
    self.start_timeout = timeout;
    self
  }
}

// =========================================================================
// 协议消息(只覆盖一期需要的字段;未识别字段一律 ignore)
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
  input: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct RunTaskParameters {
  format: String,
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

#[derive(Debug, Serialize)]
struct FinishTaskPayload {
  input: serde_json::Value,
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

// =========================================================================
// SpeechToText 实现
// =========================================================================

#[async_trait]
impl SpeechToText for ParaformerRealtimeV2 {
  fn provider_name(&self) -> &'static str {
    PROVIDER_NAME
  }

  async fn transcribe_realtime(
    &self,
    audio: AudioFrameStream,
    config: AudioStreamConfig,
  ) -> Result<SttEventStream, SpeechToTextError> {
    // 校验:Paraformer 不支持双声道 / 非整数采样率
    if config.channels != 1 {
      return Err(SpeechToTextError::ConfigInvalid(format!(
        "paraformer-realtime-v2 仅支持单声道,收到 channels={}",
        config.channels
      )));
    }

    let endpoint = self.region.websocket_endpoint();
    let model = self.model.clone();
    let credentials = self.credentials.clone();
    let start_timeout = self.start_timeout;

    // 1) 握手:加 Authorization Bearer
    let mut req = endpoint
      .into_client_request()
      .map_err(|e| SpeechToTextError::ConfigInvalid(format!("invalid websocket endpoint {endpoint}: {e}")))?;
    let auth_value = format!("Bearer {}", credentials.api_key);
    req.headers_mut().insert(
      AUTHORIZATION,
      HeaderValue::from_str(&auth_value)
        .map_err(|e| SpeechToTextError::Auth(format!("invalid api key header: {e}")))?,
    );
    req.headers_mut().insert(
      USER_AGENT,
      HeaderValue::from_static(concat!("fusion-ai/", env!("CARGO_PKG_VERSION"), " paraformer-realtime-v2")),
    );
    if let Some(ws) = &credentials.workspace_id {
      req.headers_mut().insert(
        "X-DashScope-WorkSpace",
        HeaderValue::from_str(ws).map_err(|e| SpeechToTextError::ConfigInvalid(format!("invalid workspace: {e}")))?,
      );
    }

    let (ws_stream, _resp) = tokio_tungstenite::connect_async(req).await.map_err(|e| match e {
      tokio_tungstenite::tungstenite::Error::Http(resp) if resp.status() == 401 || resp.status() == 403 => {
        SpeechToTextError::Auth(format!("dashscope handshake rejected: {}", resp.status()))
      }
      other => SpeechToTextError::Network(format!("dashscope websocket connect failed: {other}")),
    })?;

    let task_id = Uuid::new_v4().simple().to_string();
    let task_id_for_stream = task_id.clone();

    // 2) 构造 run-task payload。
    // 从 provider_options 读取 boolean override(参数与 RunTaskParameters 同名字段同义),
    // 不存在则用 provider 默认值。剩余 keys 透传到 extra。
    const RESERVED_KEYS: &[&str] = &[
      "inverse_text_normalization_enabled",
      "punctuation_prediction_enabled",
      "disfluency_removal_enabled",
      "vocabulary_words",
      "vocabulary_id",
    ];
    let read_bool = |key: &str, default: bool| -> bool {
      config.provider_options.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
    };
    let itn_enabled = read_bool("inverse_text_normalization_enabled", true);
    let punctuation_enabled = read_bool("punctuation_prediction_enabled", true);
    let disfluency_removal = read_bool("disfluency_removal_enabled", false);

    let mut extra_params = serde_json::Map::new();
    if let serde_json::Value::Object(map) = config.provider_options.clone() {
      for (k, v) in map {
        if !RESERVED_KEYS.contains(&k.as_str()) {
          extra_params.insert(k, v);
        }
      }
    }
    if let Some(domain) = config.domain_hint.clone() {
      extra_params.insert("domain_hint".into(), serde_json::Value::String(domain));
    }
    if !config.hotwords.is_empty() {
      // Paraformer 走 vocabulary 注册;一期 spike 直接走 prompt 字段透传热词
      extra_params.insert(
        "vocabulary_words".into(),
        serde_json::Value::Array(config.hotwords.iter().map(|w| serde_json::Value::String(w.clone())).collect()),
      );
    }

    let format = match config.encoding {
      AudioEncoding::PcmS16Le | AudioEncoding::PcmF32Le => "pcm".to_string(),
      AudioEncoding::Wav => "wav".to_string(),
      AudioEncoding::Mp3 => "mp3".to_string(),
      AudioEncoding::Aac => "aac".to_string(),
      AudioEncoding::Amr => "amr".to_string(),
      AudioEncoding::Opus | AudioEncoding::WebmOpus => "opus".to_string(),
      AudioEncoding::G711Ulaw | AudioEncoding::G711Alaw => {
        return Err(SpeechToTextError::ConfigInvalid("paraformer-realtime-v2 不支持 g711 编码".into()));
      }
    };

    let run_task = ClientMessage {
      header: ClientHeader { action: "run-task", task_id: task_id.clone(), streaming: "duplex" },
      payload: RunTaskPayload {
        task_group: "audio",
        task: "asr",
        function: "recognition",
        model,
        parameters: RunTaskParameters {
          format,
          sample_rate: config.sample_rate,
          vocabulary_id: Vec::new(),
          language_hints: config.language_hints.clone(),
          disfluency_removal_enabled: disfluency_removal,
          punctuation_prediction_enabled: punctuation_enabled,
          inverse_text_normalization_enabled: itn_enabled,
          extra: extra_params,
        },
        input: serde_json::json!({}),
      },
    };

    // 3) 用 async-stream 把 ws + audio 上传 + 事件解析合并到一个 SttEventStream
    let stream = try_stream! {
      let (mut sink, mut source) = ws_stream.split();

      // 3a) 发送 run-task
      let run_task_json = serde_json::to_string(&run_task)
        .map_err(|e| SpeechToTextError::Protocol(format!("serialize run-task: {e}")))?;
      sink
        .send(Message::Text(run_task_json.into()))
        .await
        .map_err(|e| SpeechToTextError::Network(format!("send run-task: {e}")))?;

      // 3b) 等 task-started
      let started_msg = tokio::time::timeout(start_timeout, source.next())
        .await
        .map_err(|_| SpeechToTextError::Timeout(format!("waiting task-started for {start_timeout:?}")))?
        .ok_or_else(|| SpeechToTextError::Protocol("websocket closed before task-started".into()))?
        .map_err(|e| SpeechToTextError::Network(format!("websocket error before task-started: {e}")))?;

      let started_event = parse_server_event(&started_msg, PROVIDER_NAME)?;
      match &started_event {
        ParsedEvent::Started { .. } => {
          yield SttEvent::Started { provider_session_id: Some(task_id_for_stream.clone()) };
        }
        ParsedEvent::Failed { code, message } => {
          let retryable = is_provider_retryable(code);
          Err(SpeechToTextError::Provider {
            provider: PROVIDER_NAME.into(),
            code: code.clone(),
            message: message.clone(),
            retryable,
          })?;
          // `unreachable!` only satisfies `try_stream!`'s type inference; the
          // preceding `?` has already returned from the stream.
          unreachable!()
        }
        other => {
          Err(SpeechToTextError::Protocol(format!(
            "expected task-started, got {other:?}"
          )))?;
          // `unreachable!` only satisfies `try_stream!`'s type inference; the
          // preceding `?` has already returned from the stream.
          unreachable!()
        }
      }

      // 3c) 启动 audio sender task。用 AbortOnDropHandle 保证消费方提前 drop 本
      // 事件流（客户端断连 / 取消）时 sender 一并 abort —— 否则 detach 的任务会
      // 一直持有 ws sink 并无限拉取(可能是活麦克风的)音频流,连接与流量泄漏。
      let mut audio = audio;
      let task_id_for_finish = task_id_for_stream.clone();
      let send_task = AbortOnDropHandle::new(tokio::spawn(async move {
        while let Some(frame) = audio.next().await {
          if frame.is_empty() {
            continue;
          }
          if let Err(e) = sink.send(Message::Binary(frame.to_vec().into())).await {
            return Err(format!("send audio frame: {e}"));
          }
        }
        // 所有帧发完,发 finish-task
        let finish = ClientMessage {
          header: ClientHeader {
            action: "finish-task",
            task_id: task_id_for_finish.clone(),
            streaming: "duplex",
          },
          payload: FinishTaskPayload { input: serde_json::json!({}) },
        };
        let finish_json = serde_json::to_string(&finish).map_err(|e| e.to_string())?;
        sink
          .send(Message::Text(finish_json.into()))
          .await
          .map_err(|e| format!("send finish-task: {e}"))?;
        Ok::<_, String>(())
      }));

      // 3d) 持续读事件,合并 segments,直到 task-finished / task-failed
      let mut all_segments: Vec<TranscriptSegment> = Vec::new();
      loop {
        let msg_opt = source.next().await;
        let Some(msg_result) = msg_opt else {
          Err(SpeechToTextError::Network(
            "websocket closed before task-finished".into(),
          ))?;
          break;
        };
        let msg = msg_result.map_err(|e| {
          SpeechToTextError::Network(format!("websocket error during recognition: {e}"))
        })?;
        let parsed = match parse_server_event(&msg, PROVIDER_NAME) {
          Ok(ev) => ev,
          Err(SpeechToTextError::Protocol(_)) => continue, // 非业务事件忽略(ping/pong/close 已由 parse_server_event 处理)
          Err(e) => Err(e)?,
        };

        match parsed {
          ParsedEvent::Started { .. } => continue, // 重复事件忽略
          ParsedEvent::ResultGenerated(segment) => {
            if segment.is_final {
              all_segments.push(segment.clone());
              yield SttEvent::SegmentFinal(segment);
            } else {
              yield SttEvent::Partial(segment);
            }
          }
          ParsedEvent::Finished => {
            let final_text: String = all_segments.iter().map(|s| s.text.clone()).collect::<Vec<_>>().join("");
            yield SttEvent::TaskFinished(TranscriptionResult {
              text: final_text,
              language: None,
              confidence: None,
              segments: all_segments,
              provider: PROVIDER_NAME.into(),
              provider_session_id: Some(task_id_for_stream.clone()),
            });
            break;
          }
          ParsedEvent::Failed { code, message } => {
            let retryable = is_provider_retryable(&code);
            yield SttEvent::Error { code: code.clone(), message: message.clone(), retryable };
            Err(SpeechToTextError::Provider {
              provider: PROVIDER_NAME.into(),
              code,
              message,
              retryable,
            })?;
            break;
          }
        }
      }

      // 3e) 确保 audio sender task 已退出;把它的错误向上冒
      match tokio::time::timeout(Duration::from_secs(5), send_task).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(msg))) => Err(SpeechToTextError::Network(msg))?,
        Ok(Err(join)) => Err(SpeechToTextError::Other(anyhow::anyhow!("audio sender panic: {join}")))?,
        Err(_) => {} // 事件流已结束但 sender 未退出;timeout drop 掉 AbortOnDropHandle → sender 被 abort
      }
    };

    Ok(Box::pin(stream))
  }
}

// =========================================================================
// 辅助
// =========================================================================

#[derive(Debug)]
enum ParsedEvent {
  Started { _attributes: serde_json::Value },
  ResultGenerated(TranscriptSegment),
  Finished,
  Failed { code: String, message: String },
}

fn parse_server_event(msg: &Message, provider: &'static str) -> Result<ParsedEvent, SpeechToTextError> {
  let text = match msg {
    Message::Text(t) => t.to_string(),
    Message::Binary(_) => return Err(SpeechToTextError::Protocol("unexpected binary frame from server".into())),
    Message::Ping(_) | Message::Pong(_) => return Err(SpeechToTextError::Protocol("ping/pong frame".into())),
    Message::Close(frame) => {
      let reason = frame.as_ref().map(|f| format!("{} {}", f.code, f.reason)).unwrap_or_default();
      return Err(SpeechToTextError::Network(format!("websocket closed: {reason}")));
    }
    Message::Frame(_) => return Err(SpeechToTextError::Protocol("raw frame".into())),
  };

  let parsed: ServerMessage = serde_json::from_str(&text)
    .map_err(|e| SpeechToTextError::Protocol(format!("parse {provider} server message failed: {e}; body={text}")))?;

  match parsed.header.event.as_str() {
    "task-started" => Ok(ParsedEvent::Started { _attributes: parsed.payload }),
    "result-generated" => {
      let payload: ResultGeneratedPayload = serde_json::from_value(parsed.payload)
        .map_err(|e| SpeechToTextError::Protocol(format!("parse result-generated payload: {e}")))?;
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
      Ok(ParsedEvent::ResultGenerated(segment))
    }
    "task-finished" => Ok(ParsedEvent::Finished),
    "task-failed" => Ok(ParsedEvent::Failed {
      code: parsed.header.error_code.unwrap_or_else(|| "UNKNOWN".into()),
      message: parsed.header.error_message.unwrap_or_default(),
    }),
    other => Err(SpeechToTextError::Protocol(format!("unknown event: {other}"))),
  }
}

fn is_provider_retryable(code: &str) -> bool {
  // DashScope 错误码以 `Throttling.` / `Network.` / `InternalError` 开头通常可重试
  matches!(
    code,
    "Throttling"
      | "Throttling.RateQuota"
      | "Throttling.AllocationQuota"
      | "Throttling.SystemBusy"
      | "InternalError"
      | "RequestTimeout"
      | "ServiceUnavailable"
  ) || code.starts_with("Throttling.")
    || code.starts_with("Network.")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_task_started() {
    let msg =
      Message::Text(r#"{"header":{"task_id":"abc","event":"task-started","attributes":{}},"payload":{}}"#.into());
    let ev = parse_server_event(&msg, PROVIDER_NAME).unwrap();
    matches!(ev, ParsedEvent::Started { .. });
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
    let ev = parse_server_event(&Message::Text(body.into()), PROVIDER_NAME).unwrap();
    match ev {
      ParsedEvent::ResultGenerated(seg) => {
        assert_eq!(seg.text, "张奶奶体温");
        assert!(!seg.is_final);
        assert_eq!(seg.words.len(), 1);
        assert_eq!(seg.words[0].text, "张");
      }
      _ => panic!("expected ResultGenerated"),
    }
  }

  #[test]
  fn parses_task_finished() {
    let msg = Message::Text(
      r#"{"header":{"task_id":"abc","event":"task-finished","attributes":{}},"payload":{"output":{},"usage":null}}"#
        .into(),
    );
    let ev = parse_server_event(&msg, PROVIDER_NAME).unwrap();
    matches!(ev, ParsedEvent::Finished);
  }

  #[test]
  fn parses_task_failed() {
    let msg = Message::Text(
      r#"{"header":{"task_id":"abc","event":"task-failed","error_code":"CLIENT_ERROR","error_message":"bad param","attributes":{}},"payload":{}}"#
        .into(),
    );
    let ev = parse_server_event(&msg, PROVIDER_NAME).unwrap();
    match ev {
      ParsedEvent::Failed { code, message } => {
        assert_eq!(code, "CLIENT_ERROR");
        assert_eq!(message, "bad param");
      }
      _ => panic!("expected Failed"),
    }
  }

  #[test]
  fn rejects_stereo_config() {
    let provider = ParaformerRealtimeV2::new(DashScopeCredentials { api_key: "x".into(), workspace_id: None });
    let cfg = AudioStreamConfig { channels: 2, ..AudioStreamConfig::pcm_s16le_16k_mono_40ms() };
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let result = rt.block_on(async {
      let frames = futures::stream::empty::<bytes::Bytes>();
      provider.transcribe_realtime(Box::pin(frames), cfg).await
    });
    match result {
      Err(SpeechToTextError::ConfigInvalid(msg)) => assert!(msg.contains("channels=2")),
      Err(other) => panic!("expected ConfigInvalid, got {other:?}"),
      Ok(_) => panic!("expected ConfigInvalid, got Ok stream"),
    }
  }

  #[test]
  fn classifies_retryable_codes() {
    assert!(is_provider_retryable("Throttling.RateQuota"));
    assert!(is_provider_retryable("Throttling"));
    assert!(is_provider_retryable("InternalError"));
    assert!(is_provider_retryable("RequestTimeout"));
    assert!(!is_provider_retryable("InvalidParameter"));
    assert!(!is_provider_retryable("AuthenticationFailed"));
  }
}
