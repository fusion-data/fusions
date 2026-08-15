# OpenAI-Compatible Provider

OpenAI 兼容 wire 层（类型本地化、零 rig 依赖——fusion-ai-de-rig.md）：一个 `Client`
打所有 OpenAI 兼容端点，Chat Completions 与 Responses 双 API 形态，外加多模态
（embedding / transcription / image generation / image edit / audio generation）。

## API 形态（fusion-ai-de-rig.md §4.1）

- `Client::completion_model()` 默认 **Responses** 形态（`/responses`）
- `Client::chat_completions_model()` 显式切 **Chat Completions**（`/chat/completions`）——
  Moonshot / Kimi 等无 `/responses` 的端点必须走这条
- 端点支持矩阵（OpenAI / DashScope / DeepSeek 有 `/responses`；Moonshot 无）与外部依据
  见 fusion-ai-de-rig.md §4.1

## 行为基线

fixture 快照（`crates/fusion-ai/tests/`，wiremock）是行为基线：请求体形状、流式 SSE
双终态（`data: [DONE]` 与 `response.completed` 事件）、错误体分级（`OpenAiCompatError`）、
四端点方言样例（OpenAI 官方 / DashScope / DeepSeek / Kimi 各一）。后续加端点先加方言样例。

## 端点方言注意事项

- DashScope Responses 要求 assistant 消息 `content` 字段必须存在（缺失 → 400）：本地
  `Message` 序列化保证 assistant content 恒序列化
- DeepSeek Responses 是无状态子集：不支持 `previous_response_id` / `store` / 图片输入，
  未知参数静默忽略——请求类型不暴露这些字段
- DeepSeek 不支持空数组 `tools`（`tools: []` 会报错）：不传 tools 时请求体省略该字段
- 智谱 GLM 响应缺少 `object` 字段：解析不依赖该字段
