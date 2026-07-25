//! 阿里云百炼 DashScope provider。
//!
//! 当前覆盖:
//! - [`fun_asr::FunAsrRealtime`] —— Fun-ASR 实时流式 STT(WebSocket)
//! - [`qwen_tts::QwenTts`] —— Qwen-TTS / qwen3-tts-flash 同步合成(HTTP)
//!
//! 未来按需扩展 CosyVoice、Qwen-Audio、Qwen-VL 等子能力。

pub mod fun_asr;
pub mod qwen_tts;

pub use fun_asr::FunAsrRealtime;
pub use qwen_tts::QwenTts;

/// 会话级测试:对着本地假 DashScope 服务端跑完整的 `transcribe_realtime` 事件循环
/// （握手时序 / ping 容忍 / 上行泵分发 / finish-task 时序 / 空闲上限 / 提前取消）。
#[cfg(test)]
mod session_tests;

use std::env;

/// DashScope 部署地域。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DashScopeRegion {
  /// 北京(中国大陆默认)。
  #[default]
  Beijing,
  /// 新加坡(海外)。
  Singapore,
}

impl DashScopeRegion {
  /// 实时 WebSocket 推理 endpoint。Fun-ASR / CosyVoice 都走这个。
  pub fn websocket_endpoint(self) -> &'static str {
    match self {
      Self::Beijing => "wss://dashscope.aliyuncs.com/api-ws/v1/inference",
      Self::Singapore => "wss://dashscope-intl.aliyuncs.com/api-ws/v1/inference",
    }
  }

  /// 多模态生成同步 endpoint(Qwen-TTS / Qwen-Audio 等)。
  pub fn multimodal_endpoint(self) -> &'static str {
    match self {
      Self::Beijing => "https://dashscope.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation",
      Self::Singapore => "https://dashscope-intl.aliyuncs.com/api/v1/services/aigc/multimodal-generation/generation",
    }
  }
}

/// DashScope 凭据。`api_key` 从 `DASHSCOPE_API_KEY` 环境变量读取。
#[derive(Clone)]
pub struct DashScopeCredentials {
  pub api_key: String,
  pub workspace_id: Option<String>,
}

impl std::fmt::Debug for DashScopeCredentials {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("DashScopeCredentials")
      .field("api_key", &"<REDACTED>")
      .field("workspace_id", &self.workspace_id)
      .finish()
  }
}

impl DashScopeCredentials {
  pub fn from_env() -> Result<Self, std::env::VarError> {
    Ok(Self { api_key: env::var("DASHSCOPE_API_KEY")?, workspace_id: env::var("DASHSCOPE_WORKSPACE_ID").ok() })
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn debug_never_leaks_api_key() {
    let creds = DashScopeCredentials { api_key: "sk-dash-secret".into(), workspace_id: Some("ws-1".into()) };
    let dbg = format!("{creds:?}");
    assert!(!dbg.contains("sk-dash-secret"), "api_key leaked: {dbg}");
    assert!(dbg.contains("<REDACTED>") && dbg.contains("ws-1"));
  }
}
