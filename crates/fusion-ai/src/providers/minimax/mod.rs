//! MiniMax provider（platform.minimaxi.com，中国平台 API）。
//!
//! 当前覆盖: [`tts::MinimaxTts`] —— T2A V2 合成(非流式 + SSE 流式)与
//! voice_clone 声音复刻(files/upload + voice_clone 两步)。

pub mod tts;

pub use tts::{MinimaxTts, T2aAudio, T2aRequest};
