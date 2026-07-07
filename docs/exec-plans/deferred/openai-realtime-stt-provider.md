# fusion-ai OpenAI Realtime STT provider（触发驱动）

> Status: Deferred
> Scope: `fusion-ai::speech_to_text` provider adapter

本文只记录 `fusion-ai` crate 内 OpenAI Realtime transcription provider 的 adapter 设计。区域路由、凭证读取、voice session、browser WebSocket、业务 metric 和 fallback UX 属于消费方应用。

## 1. 目标

- 新增 `providers/openai_realtime` module。
- 实现 `SpeechToText` trait。
- 支持 OpenAI Realtime transcription GA WebSocket protocol。
- 将 provider event 映射为 `SttEvent`。
- 支持 16k PCM 输入到 provider-required 24k PCM 的 adapter-side conversion。

## 2. 非目标

- 不决定业务区域路由。
- 不读取数据库、配置中心或 encrypted credential store。
- 不实现 browser recording、voice session 或 gateway proxy。
- 不实现 automatic provider fallback。

## 3. module shape

```text
fusion-ai/src/providers/openai_realtime/
  mod.rs
  transcription.rs
```

Public type sketch:

```rust
pub struct OpenAiRealtimeCredentials {
    pub api_key: String,
    pub endpoint: Option<String>,
}

pub struct OpenAiRealtimeTranscription {
    credentials: Arc<OpenAiRealtimeCredentials>,
    config: OpenAiRealtimeConfig,
}
```

## 4. protocol contract

- Connect to `wss://api.openai.com/v1/realtime` unless endpoint override is supplied.
- Header: `Authorization: Bearer <api_key>`.
- No legacy beta header and no `intent=transcription` query.
- Send `session.update` with `session.type = "transcription"`.
- Audio input format: `audio/pcm` at 24 kHz mono.
- Send audio via JSON text frame `input_audio_buffer.append { audio: base64(...) }`.
- Finish stream via `input_audio_buffer.commit`.

## 5. `SttEvent` mapping

| Provider event | `SttEvent` |
|---|---|
| `session.created` / `session.updated` | `Started` |
| `conversation.item.input_audio_transcription.delta` | `Partial` |
| `conversation.item.input_audio_transcription.completed` | `SegmentFinal` |
| final completed after commit | `TaskFinished` |
| `error` | `Error` + provider error classification |

Provider event ordering across speech turns is not guaranteed; adapter MUST group partial / final text by provider item id when needed.

## 6. audio conversion

- Adapter input remains `AudioFrameStream` with `AudioStreamConfig`.
- If input is 16 kHz PCM s16le mono, adapter MUST resample to 24 kHz PCM before upload.
- Unsupported encodings or channel counts MUST return `SpeechToTextError::ConfigInvalid`.
- Resampling must preserve frame ordering and respect backpressure.

## 7. language hints and capabilities

- `language` is optional provider hint.
- No language hint means provider auto-detection.
- OpenAI Realtime transcription does not provide hotword / prompt behavior equivalent to providers that support vocabulary injection; applications must not rely on STT hotwords for correctness.

## 8. validation

- mock WebSocket server verifies session update frame;
- adapter uploads base64 24k PCM frames;
- delta / completed / error event parsing tests;
- retryable vs non-retryable error classification;
- 16k to 24k conversion sample test;
- no network tests in default unit suite.

## 9. activation triggers

- consumer application needs OpenAI Realtime transcription in production;
- provider protocol and model choice are revalidated against current official docs;
- sample corpus confirms latency / finalization / accuracy are acceptable for target deployment.
