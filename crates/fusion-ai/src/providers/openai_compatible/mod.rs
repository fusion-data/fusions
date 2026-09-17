//! OpenAI 兼容 provider（类型本地化，fusion-ai-de-rig.md §4.2）
//!
//! # Example
//! ```
//! use fusion_ai::providers::openai_compatible::Client;
//!
//! let client = Client::new("YOUR_API_KEY");
//! // Chat Completions 形态（通用 / 老 API）；Responses API 形态走 responses_model()
//! let chat = client.completion_model("gpt-4o");
//! let responses = client.responses_model("gpt-4o");
//! ```
pub mod client;
pub mod completion;
pub mod embedding;
pub mod errors;
pub mod responses_api;
pub mod types;

#[cfg(feature = "audio")]
#[cfg_attr(docsrs, doc(cfg(feature = "audio")))]
pub mod audio_generation;
#[cfg(feature = "image")]
#[cfg_attr(docsrs, doc(cfg(feature = "image")))]
pub mod image_generation;

#[cfg(feature = "image")]
#[cfg_attr(docsrs, doc(cfg(feature = "image")))]
pub mod image_edit;

pub mod transcription;

pub use client::*;
pub use completion::*;
pub use embedding::*;
pub use errors::*;

#[cfg(feature = "audio")]
pub use audio_generation::{TTS_1, TTS_1_HD};

#[cfg(feature = "image")]
pub use image_edit::{ImageEditModel, Usage as ImageEditUsage};
#[cfg(feature = "image")]
pub use image_generation::*;
pub use streaming::*;
pub use transcription::*;
