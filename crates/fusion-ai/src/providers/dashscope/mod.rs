//! 阿里云百炼 DashScope provider。
//!
//! 当前覆盖:
//! - [`ParaformerRealtimeV2`] —— Paraformer 实时 v2 流式 STT(WebSocket)
//! - [`QwenTts`] —— Qwen-TTS / qwen3-tts-flash 同步合成(HTTP)
//!
//! 未来按需扩展 CosyVoice、Qwen-Audio、Qwen-VL 等子能力。

pub mod paraformer;
pub mod qwen_tts;

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
  /// 实时 WebSocket 推理 endpoint。Paraformer / CosyVoice 都走这个。
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
#[derive(Debug, Clone)]
pub struct DashScopeCredentials {
  pub api_key: String,
  pub workspace_id: Option<String>,
}

impl DashScopeCredentials {
  pub fn from_env() -> Result<Self, std::env::VarError> {
    Ok(Self { api_key: env::var("DASHSCOPE_API_KEY")?, workspace_id: env::var("DASHSCOPE_WORKSPACE_ID").ok() })
  }
}
