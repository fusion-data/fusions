# fusion-ai Model Gateway core 重构（触发驱动）

> Status: Deferred
> Scope: `fusion-ai` only

本文只记录 `fusion-ai` crate 内 provider transform core 的后续方向。业务 gateway host、凭证真相源、route policy、usage ledger、HTTP northbound API 和产品计费均属于消费方应用，不进入本仓文档。

## 1. 目标

- 建立 provider-neutral canonical request / response / stream chunk 类型。
- 把 provider adapter、stream normalization、error normalization、usage extraction 从应用层收回 `fusion-ai`。
- 将 `rig-core` 降级为 optional compatibility surface，最终允许默认构建不依赖 `rig-core`。
- 保持现有 `LlmChatProvider` / `MeteredLlmProvider` 路径可渐进迁移。

## 2. 非目标

- 不实现业务 Model Gateway service。
- 不存储 provider credential、route rule、application budget 或 token ledger。
- 不定义 API key / virtual key 身份体系。
- 不提供 northbound OpenAI-compatible HTTP server。

## 3. 目标模块

```text
fusion-ai/src/model_gateway/
  types.rs      // canonical request / response / stream / usage
  provider.rs   // ProviderAdapter trait + capabilities + error kind
  registry.rs   // provider catalog and adapter lookup
  stream.rs     // stream / SSE / chunk normalization helpers
  cost.rs       // usage and cost metadata extraction
  compat.rs     // optional OpenAI-compatible wire conversion
```

## 4. 分期

### P0. dependency and API audit

- list all users of `fusion_ai::rig`, `ClientFactory`, `agents`, `embeddings` and OpenAI-compatible wrappers;
- classify usage as runtime hot path, example, test or compat API;
- add golden fixtures for chat, tool calling, streaming, usage and provider error mapping.

### P1. `rig-core` optionalization

- add `rig-compat` feature;
- gate `pub use rig`, `ClientFactory`, agents, embeddings and rig-specific wrappers;
- keep old compat tests under `--features rig-compat`;
- ensure `cargo test -p fusion-ai --no-default-features` builds.

### P2. canonical transform core

- add `ModelRequest`, `ModelResponse`, `ModelStreamChunk`, `ModelUsage`;
- add `ProviderAdapter` trait;
- implement OpenAI-compatible adapter first;
- add adapter capability matrix;
- snapshot-test provider parity.

### P3. application adapter handoff

`fusion-ai` exposes transform primitives only. Consuming applications construct credentials, route decisions, HTTP clients, retry/fallback policy and usage sinks.

### P4. remove default `rig-core`

- default feature tree does not include `rig-core`;
- legacy users migrate to `rig-compat` or a separate compatibility crate;
- docs and examples stop using rig-first APIs.

## 5. validation

- `cargo test -p fusion-ai --no-default-features`
- `cargo test -p fusion-ai --features rig-compat`
- adapter fixture tests for chat / tools / streaming / usage / provider errors
- `cargo tree -p fusion-ai` confirms default dependency tree intent

## 6. activation triggers

- `rig-core` blocks dependency upgrades or release stability;
- non OpenAI-compatible providers become production path;
- multiple applications need shared provider normalization;
- provider stream / usage normalization starts duplicating across consumers.
