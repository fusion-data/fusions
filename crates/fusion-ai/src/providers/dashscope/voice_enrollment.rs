//! 阿里云 DashScope **Qwen-TTS 声音复刻**（voice enrollment，HTTP）。
//!
//! 协议：`POST .../services/audio/tts/customization`，`model=qwen-voice-enrollment`，
//! `input.action = create / list / delete`。create 的样本音频两种形态：
//! base64 Data URL（`data:audio/mpeg;base64,...`，支持 wav/mpeg/mp4）或公网 URL。
//! 返回 `output.voice`（可直接作为 Qwen-TTS 合成的 voice 参数，target_model 必须
//! 与合成模型完全一致）。计费 0.01 元/个，创建失败不计费。
//!
//! CosyVoice 系复刻（`model=voice-enrollment`）只接受公网 URL，不在本客户端
//! 覆盖范围——需要 vendor 侧拉取公网音频，与 base64 直传形态是两条链路。
//!
//! 参考：<https://help.aliyun.com/zh/model-studio/voice-clone-design-http-api>

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};

use crate::providers::dashscope::{DashScopeCredentials, DashScopeRegion};
use crate::providers::speech::{SpeechError, detect_audio_container};

/// 声音复刻音色管理端点（北京地域 dashscope 域名形态；workspace 专属域名形态
/// 也可用，本客户端不依赖 workspace）。
const CUSTOMIZATION_PATH: &str = "/api/v1/services/audio/tts/customization";

/// Qwen-TTS 声音复刻客户端（create / list / delete）。
#[derive(Debug, Clone)]
pub struct QwenVoiceEnrollment {
  credentials: Arc<DashScopeCredentials>,
  region: DashScopeRegion,
  /// base_url 覆盖（默认按 region；测试指向 mock server）。
  base_url_override: Option<String>,
  http: reqwest::Client,
}

impl QwenVoiceEnrollment {
  pub fn new(credentials: DashScopeCredentials) -> Result<Self, SpeechError> {
    let http = reqwest::Client::builder()
      .timeout(Duration::from_secs(120))
      .build()
      .map_err(|e| SpeechError::RequestBuild(format!("build reqwest client: {e}")))?;
    Ok(Self {
      credentials: Arc::new(credentials),
      region: DashScopeRegion::default(),
      base_url_override: None,
      http,
    })
  }

  pub fn with_region(mut self, region: DashScopeRegion) -> Self {
    self.region = region;
    self
  }

  /// 覆盖 base_url（测试注入 mock server；None 恢复按 region）。
  pub fn with_base_url(mut self, base_url: Option<String>) -> Self {
    self.base_url_override = base_url;
    self
  }

  fn base_url(&self) -> String {
    let host = match self.region {
      DashScopeRegion::Beijing => "https://dashscope.aliyuncs.com",
      DashScopeRegion::Singapore => "https://dashscope-intl.aliyuncs.com",
    };
    self.base_url_override.clone().unwrap_or_else(|| host.to_string())
  }

  async fn post_action(&self, input: serde_json::Value) -> Result<serde_json::Value, SpeechError> {
    let body = serde_json::json!({ "model": "qwen-voice-enrollment", "input": input });
    let response = self
      .http
      .post(format!("{}{CUSTOMIZATION_PATH}", self.base_url()))
      .header(AUTHORIZATION, format!("Bearer {}", self.credentials.api_key))
      .header(CONTENT_TYPE, "application/json")
      .json(&body)
      .send()
      .await
      .map_err(SpeechError::from)?;
    let status = response.status();
    let raw = response.text().await.unwrap_or_default();
    if !status.is_success() {
      return Err(SpeechError::Http { status: status.as_u16(), message: raw });
    }
    let parsed: serde_json::Value =
      serde_json::from_str(&raw).map_err(|e| SpeechError::protocol(format!("parse response: {e}; body={raw}")))?;
    // DashScope 错误可出现在 HTTP 200 body（code 非空 + message）
    if let Some(code) = parsed.get("code").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) {
      let message = parsed.get("message").and_then(|m| m.as_str()).unwrap_or_default();
      return Err(classify_enrollment_error(code, message));
    }
    Ok(parsed)
  }

  /// 创建克隆音色：样本音频 bytes → 复刻 → 可用于 Qwen-TTS 合成的 voice 名。
  pub async fn create(&self, req: CreateVoiceRequest<'_>) -> Result<EnrolledVoice, SpeechError> {
    if req.audio_bytes.is_empty() {
      return Err(SpeechError::request_build("enrollment audio sample is empty"));
    }
    if req.preferred_name.is_empty()
      || req.preferred_name.len() > 16
      || !req
        .preferred_name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
      return Err(SpeechError::request_build(format!(
        "preferred_name must be 1-16 chars of [0-9A-Za-z_], got {:?}",
        req.preferred_name
      )));
    }
    // base64 Data URL 形态（音频容器按 magic bytes 探测，探测不出按 mp3 兜底）
    let mime = req.audio_mime.unwrap_or_else(|| detect_audio_container(req.audio_bytes).mime());
    let data_url = format!(
      "data:{mime};base64,{}",
      base64::engine::general_purpose::STANDARD.encode(req.audio_bytes)
    );
    let mut input = serde_json::json!({
      "action": "create",
      "target_model": req.target_model,
      "preferred_name": req.preferred_name,
      "audio": { "data": data_url },
    });
    if let Some(text) = req.text {
      input["text"] = serde_json::Value::String(text.to_string());
    }
    if let Some(language) = req.language {
      input["language"] = serde_json::Value::String(language.to_string());
    }
    let parsed = self.post_action(input).await?;
    let output = parsed
      .get("output")
      .ok_or_else(|| SpeechError::protocol(format!("response missing output; body={parsed}")))?;
    let voice = output
      .get("voice")
      .and_then(|v| v.as_str())
      .filter(|v| !v.is_empty())
      .ok_or_else(|| SpeechError::protocol(format!("response missing output.voice; body={parsed}")))?;
    Ok(EnrolledVoice {
      voice: voice.to_string(),
      target_model: output
        .get("target_model")
        .and_then(|m| m.as_str())
        .unwrap_or(req.target_model)
        .to_string(),
      // 音频质量不佳 / 与文本不匹配时 vendor 降级（no_merged_segments /
      // no_valid_asr_segments 等）——克隆仍可用，调用方按需告警
      fallback_mode: output.get("fallback_mode").and_then(|f| f.as_bool()),
      fallback_reason: output
        .get("fallback_reason")
        .and_then(|r| r.as_str())
        .map(String::from),
    })
  }

  /// 列出克隆音色（分页）。
  pub async fn list(&self, page_index: u32, page_size: u32) -> Result<VoiceList, SpeechError> {
    let parsed = self
      .post_action(serde_json::json!({
        "action": "list",
        "page_index": page_index,
        "page_size": page_size,
      }))
      .await?;
    let output = parsed
      .get("output")
      .ok_or_else(|| SpeechError::protocol(format!("response missing output; body={parsed}")))?;
    let voices = output
      .get("voice_list")
      .and_then(|v| v.as_array())
      .map(|items| {
        items
          .iter()
          .map(|item| VoiceListItem {
            voice: item.get("voice").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            target_model: item
              .get("target_model")
              .and_then(|m| m.as_str())
              .unwrap_or_default()
              .to_string(),
            gmt_create: item
              .get("gmt_create")
              .and_then(|t| t.as_str())
              .map(String::from),
          })
          .collect()
      })
      .unwrap_or_default();
    Ok(VoiceList {
      total_count: output.get("total_count").and_then(|t| t.as_i64()).unwrap_or_default(),
      voices,
    })
  }

  /// 删除克隆音色。
  pub async fn delete(&self, voice: &str) -> Result<(), SpeechError> {
    if voice.is_empty() {
      return Err(SpeechError::request_build("voice is empty"));
    }
    self
      .post_action(serde_json::json!({ "action": "delete", "voice": voice }))
      .await?;
    Ok(())
  }
}

/// create 请求。
#[derive(Debug, Clone, Copy)]
pub struct CreateVoiceRequest<'a> {
  /// 样本音频 bytes（mp3/wav/m4a；推荐 10-20s，≤60s）。
  pub audio_bytes: &'a [u8],
  /// 显式指定样本 MIME（None 按 magic bytes 探测）。
  pub audio_mime: Option<&'a str>,
  /// 驱动该音色的合成模型，MUST 与后续合成接口 model 完全一致
  /// （如 `qwen3-tts-vc-2026-01-22`）。
  pub target_model: &'a str,
  /// 音色名偏好（1-16 位 `[0-9A-Za-z_]`；最终 voice 名由 vendor 生成）。
  pub preferred_name: &'a str,
  /// 样本音频对应文本（可选，辅助提升复刻效果）。
  pub text: Option<&'a str>,
  /// 样本语种（`zh`/`en`/…；缺省 zh）。
  pub language: Option<&'a str>,
}

/// create 产物。
#[derive(Debug, Clone)]
pub struct EnrolledVoice {
  /// 合成接口的 voice 参数（可直接使用）。
  pub voice: String,
  pub target_model: String,
  /// vendor 降级标记（None = 响应未携带；Some(false) = 正常）。
  pub fallback_mode: Option<bool>,
  pub fallback_reason: Option<String>,
}

/// list 产物。
#[derive(Debug, Clone)]
pub struct VoiceList {
  pub total_count: i64,
  pub voices: Vec<VoiceListItem>,
}

#[derive(Debug, Clone)]
pub struct VoiceListItem {
  pub voice: String,
  pub target_model: String,
  pub gmt_create: Option<String>,
}

/// DashScope enrollment 错误码分类（对齐 qwen_tts 的码表语义）。
fn classify_enrollment_error(code: &str, message: &str) -> SpeechError {
  SpeechError::Vendor {
    code: code.to_string(),
    message: message.to_string(),
    rate_limited: code.starts_with("Throttling"),
    transient: code.starts_with("InternalError")
      || code.starts_with("ServiceUnavailable")
      || code.starts_with("Timeout"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn creds() -> DashScopeCredentials {
    DashScopeCredentials { api_key: "sk-test".into(), workspace_id: None }
  }

  #[tokio::test]
  async fn create_sends_data_url_and_parses_voice() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .and(wiremock::matchers::path(CUSTOMIZATION_PATH))
      .and(wiremock::matchers::header("authorization", "Bearer sk-test"))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "output": {
          "voice": "qwen3-tts-vc-2026-01-22-myclone-ab12",
          "target_model": "qwen3-tts-vc-2026-01-22",
          "fallback_mode": false
        },
        "usage": { "count": 1 },
        "request_id": "req-1"
      })))
      .expect(1)
      .mount(&server)
      .await;

    let client = QwenVoiceEnrollment::new(creds()).unwrap().with_base_url(Some(server.uri()));
    // wav magic bytes（RIFF）→ data:audio/wav;base64
    let sample = b"RIFFxxxxWAVEfmt ".to_vec();
    let enrolled = client
      .create(CreateVoiceRequest {
        audio_bytes: &sample,
        audio_mime: None,
        target_model: "qwen3-tts-vc-2026-01-22",
        preferred_name: "myclone",
        text: None,
        language: None,
      })
      .await
      .unwrap();
    assert_eq!(enrolled.voice, "qwen3-tts-vc-2026-01-22-myclone-ab12");
    assert_eq!(enrolled.fallback_mode, Some(false));

    // 请求体校验：data URL 前缀 + magic 探测 wav
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let data = body["input"]["audio"]["data"].as_str().unwrap();
    assert!(data.starts_with("data:audio/wav;base64,"), "mime probed from RIFF magic: {data}");
    assert_eq!(body["model"], "qwen-voice-enrollment");
    assert_eq!(body["input"]["action"], "create");
    assert_eq!(body["input"]["preferred_name"], "myclone");
  }

  #[tokio::test]
  async fn create_rejects_invalid_preferred_name() {
    let client = QwenVoiceEnrollment::new(creds()).unwrap();
    // 空串 / 含空格 / 超 16 字符 / 含非法字符 -
    for bad in ["", "has space", "a2345678901234567", "a- dash"] {
      let result = client
        .create(CreateVoiceRequest {
          audio_bytes: b"RIFF",
          audio_mime: None,
          target_model: "m",
          preferred_name: bad,
          text: None,
          language: None,
        })
        .await;
      assert!(matches!(result, Err(SpeechError::RequestBuild(_))), "name {bad:?}");
    }
  }

  #[tokio::test]
  async fn create_classifies_vendor_error_codes() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "code": "Throttling.RequestRateQuota",
        "message": "too fast",
        "request_id": "r"
      })))
      .mount(&server)
      .await;
    let client = QwenVoiceEnrollment::new(creds()).unwrap().with_base_url(Some(server.uri()));
    let result = client
      .create(CreateVoiceRequest {
        audio_bytes: b"RIFF",
        audio_mime: None,
        target_model: "m",
        preferred_name: "ok",
        text: None,
        language: None,
      })
      .await;
    assert!(matches!(&result, Err(SpeechError::Vendor { code, .. }) if code == "Throttling.RequestRateQuota"));
    assert!(result.unwrap_err().is_rate_limited());
  }

  #[tokio::test]
  async fn http_error_carries_body() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(
        wiremock::ResponseTemplate::new(401).set_body_string(r#"{"code":"InvalidApiKey","message":"bad key"}"#),
      )
      .mount(&server)
      .await;
    let client = QwenVoiceEnrollment::new(creds()).unwrap().with_base_url(Some(server.uri()));
    let result = client
      .create(CreateVoiceRequest {
        audio_bytes: b"RIFF",
        audio_mime: None,
        target_model: "m",
        preferred_name: "ok",
        text: None,
        language: None,
      })
      .await;
    assert!(matches!(&result, Err(SpeechError::Http { status: 401, .. })));
    assert!(!result.unwrap_err().is_retryable());
  }

  #[tokio::test]
  async fn delete_sends_voice_action() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
      .respond_with(
        wiremock::ResponseTemplate::new(200).set_body_json(
          serde_json::json!({ "output": { "voice": "v1" }, "usage": { "count": 0 }, "request_id": "r" }),
        ),
      )
      .expect(1)
      .mount(&server)
      .await;
    let client = QwenVoiceEnrollment::new(creds()).unwrap().with_base_url(Some(server.uri()));
    client.delete("v1").await.unwrap();
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["input"]["action"], "delete");
    assert_eq!(body["input"]["voice"], "v1");
  }
}
