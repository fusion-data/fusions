//! 火山引擎（Volcengine）provider。
//!
//! 当前覆盖: [`speech::DoubaoSpeech`] —— 豆包语音 V3(openspeech 域)声音复刻 +
//! 单向流式合成(chunked JSON 行)。

pub mod speech;

pub use speech::{DoubaoClonedVoice, DoubaoSpeech, ResourceId, UnidirectionalRequest};
