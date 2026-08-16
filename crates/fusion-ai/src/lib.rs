mod error;
pub mod graph_flow;
pub mod json_utils;
pub mod llm;
pub mod providers;
pub mod speech_to_text;
pub mod utils;
#[cfg(feature = "video")]
pub mod video_generation;

/// Re-exports of the crates that appear in this crate's **public** API surface
/// (`SttUplink::Audio(Bytes)`, `SttUplinkStream`, `#[async_trait]` on `SpeechToText` /
/// `LlmChatProvider`).
///
/// Downstream code implementing those traits MUST use these rather than declaring its own
/// dependency: a version skew produces the classic `expected Bytes, found Bytes` diagnostic, which
/// reads as a compiler bug to anyone who has not hit it before.
///
/// These are upstream dependencies passed through verbatim and are **not** covered by this
/// crate's SemVer guarantees.
pub use {async_trait::async_trait, bytes, futures};

pub use error::*;
