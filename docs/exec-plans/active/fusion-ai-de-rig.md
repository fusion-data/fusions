# fusion-ai 脱钩 rig —— OpenAI 兼容层本地化（Responses API 优先）

> Status: Active
> 负责人: Yang Jing
> 目标版本: fusions 0.4.0（破坏性：`AiError` 变体收敛、rig re-export 与 factory 路径删除）
> 关联: 原 deferred 计划 `fusion-ai-model-gateway-core.md` 已删除——其 P1/P4（rig optionalization → removal）由本计划承接；P2/P3（model_gateway canonical 抽象层）否决（无多 provider 归一化的真实消费方，属提前优化）；其 P3 的职责边界并入本计划非目标（见 §2）。
> 调研依据: 2026-08-14 agent-rs / rig 依赖面 / hetuos 调用面三轮调研（对话留档），外部支持矩阵见 §4.1。

## 1. 背景与决策

fusion-ai 的生产路径（`llm/`、`speech_to_text/`、`providers/dashscope/`、`video_generation/`）已全部自研、零 rig 引用；rig 只剩四块绑定：`client.rs` 的 `ClientFactory`（19 provider 薄壳，**零业务消费**）、`providers/openai_compatible/`（fork 自 rig 的 provider 形态，wire 自研但类型/trait/SSE 绑 rig）、`error.rs` 的两个 rig 错误变体、`graph_flow/context.rs` 的消息桥接（零消费）。同时 rig umbrella 强制带入 `rig-bedrock`（aws-config）、`rig-lancedb`、`rig-milvus`、`rig-fastembed` 等未使用传递依赖，且 `rig-postgres 0.40 → sqlx 0.9` 的版本锁阻塞 sqlx 升级（`Cargo.toml` workspace 段注释已有记录）。

决策（2026-08-14，用户拍板）：

1. **脱钩 rig，收尾自研**——不引入 agent-rs（`ZSeven-W/agent-rs`）作为依赖：其 0.1.0 pre-release、单一作者、provider 仅 3 家、无 embedding/音频，成熟度与能力面都不满足生产要求；仅借鉴其设计（统一流事件枚举、能力位、工具分级）。
2. **openai_compatible 保留并类型本地化**（它是 DeepSeek / Moonshot / Qwen / OpenAI 四端的唯一统一 wire，且流式能力只此一处），**不删除**。
3. **API 形态默认 Responses，显式可切 Chat Completions**（见 §4.1）。

## 2. 目标 / 非目标

**目标**

1. `cargo tree -p fusion-ai` 不再出现 rig（含全部传递依赖）。
2. openai_compatible 公共 API 零 rig 类型；行为与现状等价（fixture 快照对齐，见 P0）。
3. `creative-ai-service` 的 rig 依赖迁移到 fusion-ai（本仓 workspace 同步去 rig）。
4. 错误分级语义（上游瞬态 → 503 / 本地缺陷 → 500）保持，锚点测试改写后仍绿。

**非目标（含裁决理由）**

| 项 | 理由 |
| --- | --- |
| 不实现 rig 的 Agent / AgentBuilder / ExtractorBuilder / Prompt / Chat 泛型机器 | 零消费者；multi-turn agent loop 未来如需要，参考 agent-rs 的 QueryLoop 相位状态机另立项 |
| 不给 `llm/LlmChatProvider` 加流式 API | 无消费者（YAGNI）；流式经 openai_compatible 满足。解除条件：hetu-ai 侧出现 NLU 流式需求 |
| 不合并 `llm/wire_openai_compat` 与 `openai_compatible/completion` 两套 chat wire | 前者是 hetu-ai 生产路径，动它超出脱钩范围；合一与否待首个双消费方出现再裁 |
| 不升级 sqlx 0.9 | rig-postgres 删除后约束自然消失，升级独立立项 |
| 不新增 Anthropic / Gemini 原生 wire | `llm/providers/{anthropic,gemini}` 维持 stub，等真实需求 |
| fusion-ai 只提供 wire / transform 原语；凭证真相源、路由策略、应用预算、usage ledger、northbound OpenAI 兼容 server 一律留在消费方应用 | 原 deferred 计划 P3 的边界裁决，继续有效 |
| 不给 hetu-core 增加 fusion-ai 依赖 | 违反 hetuos layering gate（`check-crate-layering.sh` 白名单，SSOT `hetuos/docs/designs/ai-platform-layering.md`）；hetuos 侧共享代码的收敛一律走「fusion-ai 中性参数 API + bin 侧薄 proto 映射」形态（见 §4.4） |
| 不统一 provider id 命名（`"dashscope"` vs `LlmProviderId::Qwen`） | hetuos 的 provider 字符串承担 DB CHECK 约束与凭证 channel 语义，改动牵连 DB 种子与 catalog，独立评估 |

## 3. 现状依赖面（调研结论，代码不直接表达的部分）

- **业务消费方只有一家碰 rig 原生 API**：本仓 `bins/creative-ai-service/src/infra/llm.rs`（`RigLlmProvider`，rig `Agent` + `Prompt` + `StreamingPrompt`，DeepSeek / Moonshot / Qwen 三分支）。其 L1 端口 `creative_llm::LlmProvider` 不渗 rig（端口模式已就位）。实际只用：单轮 chat、流式 text delta、终态 usage、thinking 关闭注入、max_tokens 分档。**不用** tool calling / multi-turn loop。
- **embedding 零耦合**：本仓与 hetuos 的 embedding 全走 `creative_rag::embedding::EmbeddingClient`（自研 crate），fusion-ai 的 `factory::ClientFactory::embeddings` / `embeddings.rs` 无业务消费。
- **hetuos 整仓零 rig 使用**：经 git submodule（pinned commit 与 hetu-creative 同源）+ path 依赖消费 fusion-ai 0.3.0；直接依赖方仅 `bins/hetu-ai` 与 `bins/hetu-infra` 两个 bin（约 30 个 API 触点），全走自研层（`llm::factory::build_provider` / `MeteredLlmProvider` / `FunAsrRealtime` / `speech_to_text::`），无 rig 直接引用、无绕过 fusion-ai 的自研 LLM HTTP/SSE。其 workspace 的 `rig` / `rig-postgres` 声明为冗余。调用方技术债见 §4.4。
- **fusions 仓内消费**：examples（rig Chat / ImageGenerationClient 原生 API）+ `crates/fusions/src/error.rs` 的 `AiError → DataError` 分级 match（3 处）。
- **openai_compatible 的 rig 绑定结构**：wire 全自研，绑定的是 rig 的类型系统（`completion::{CompletionRequest, CompletionModel, Usage, Message}`、`message::*`、`OneOrMany`、`http_client::sse`、telemetry ext）与 client trait（`CompletionClient` / `EmbeddingsClient` / `TranscriptionClient` / `ImageGenerationClient` / `AudioGenerationClient` / `VerifyClient`）。

## 4. 目标形态

### 4.1 API 形态矩阵与默认策略

| 端点 | base_url | Responses API | 采形 |
| --- | --- | --- | --- |
| OpenAI 官方 | `https://api.openai.com/v1` | ✅ | 默认 Responses |
| DashScope 兼容（Qwen） | `https://dashscope.aliyuncs.com/compatible-mode/v1` | ✅（`/responses` 已支持） | 默认 Responses |
| DeepSeek | `https://api.deepseek.com` | ✅（无状态子集） | 默认 Responses |
| Moonshot / Kimi | `https://api.moonshot.cn/v1` | ❌（仅 `/chat/completions`） | 唯一强制 Chat Completions |

- `Client::completion_model()` 默认返回 Responses 模型（沿用现状默认语义），显式切 Chat Completions（实现形态：`Client::chat_completions_model()` 工厂方法，语义与原设想的 `.completions_api()` 切换等价）——Moonshot 等端点必须显式切。
- 依据（2026-08-14 验证）：[百炼 OpenAI Responses 兼容](https://help.aliyun.com/zh/model-studio/compatibility-with-openai-responses-api)、[DeepSeek Responses API](https://api-docs.deepseek.com/zh-cn/guides/responses_api)、[Kimi API overview](https://platform.kimi.com/docs/api/overview)。

### 4.2 本地类型层（openai_compatible 内新建）

```text
providers/openai_compatible/
  types.rs     // Message / UserContent / AssistantContent / ToolDefinition / ToolCall /
               //   ToolChoice / Usage / OneOrMany / ImageDetail …（fork rig 同名类型，演进自主）
  errors.rs    // OpenAiCompatError（见 §4.3）
  client.rs    // 去 rig client trait，保留具体方法 + VerifyClient 能力并入
  completion/      // Chat Completions（类型本地化，wire 不动）
  responses_api/   // Responses（类型本地化，wire 不动）
  embedding.rs / image_generation.rs / image_edit.rs / audio_generation.rs / transcription.rs
  client_wrapper.rs // 删除（与 Client 重复，存在意义仅为 impl rig trait）
```

- 请求类型 MUST 保留 extra-body 注入点（chat completions 的 `additional_params` 等价物）——thinking 关闭（DeepSeek `{"thinking":{"type":"disabled"}}`、Qwen `{"enable_thinking":false}`、Kimi `extra_body.thinking`）全靠它，creative-ai-service 迁移的前置条件。
- SSE 解析从 `rig::http_client::sse` 换 `eventsource-stream`（rig-core 同款轻量实现）+ `reqwest` stream feature。**终止形态两种都要兼容**：OpenAI 兼容流以 `data: [DONE]` 结束，DeepSeek Responses 流以 `response.completed / incomplete / failed` 事件终态且**没有** `[DONE]`。
- `fusions::ai::rig` re-export 删除；`pub use {async_trait, bytes, futures}` 保留（STT 公共 API 依赖）。

### 4.3 错误模型

```rust
pub enum AiError {          // 收敛后
  Custom(String),
  OpenAiCompat(#[from] OpenAiCompatError),
}
pub enum OpenAiCompatError {
  Http { status: u16, message: String },  // provider 非 2xx
  Transport(String),                      // 连接层（reqwest send 失败）
  ResponseParse(String),                  // 反序列化 / SSE 帧非法
  RequestBuild(String),                   // 请求构造
  Stream(String),                         // 流中途错误
}
// OpenAiCompatError::is_upstream_transient()：Transport / Http(5xx / 429) → true
```

- `crates/fusions/src/error.rs` 的 `AiError → DataError` 分级改用 `is_upstream_transient()`，503/500 语义不变（[framework-conventions](../../designs/framework-conventions.md) §1「上游瞬态 vs 本地缺陷」条款不动，测试锚点 `ai_error_maps_transient_upstream_to_service_unavailable` 改写数据构造）。
- `FactoryError` 随 `ClientFactory` 删除；`DefaultProvider`（服务 rig factory 路径的 19-provider enum）删除，文档口径由 `llm::LlmProviderId` 独占。
- `graph_flow/context.rs`：内部 chat history 与 `get_rig_messages` / `get_last_rig_messages` 换本地 `types.rs::Message`（改名 `get_messages` / `get_last_messages`，零消费者直接改）。

### 4.4 hetuos 调用方收敛（fusion-ai 侧最小增量 API）

hetuos 消费面本身零 rig，脱钩对其是纯回归验证；但其调用方存在三处结构重复 / 拧巴，随本次重构一并收敛。fusion-ai 侧只加**中性参数构造 API**（不含 proto、不破坏 layering），proto 映射留在 bin 侧：

| # | 现状（hetuos） | 收敛形态 |
| --- | --- | --- |
| 1 | `build_llm_config` 双份实现：`hetu-ai` chat/infra/credentials（3 vendor）与 `hetu-infra` message/ai_probe（5 vendor）各写一遍 proto→`LlmProviderConfig` 转换 + `parse_dashscope_region` 别名表（singapore/intl）+ api_key 校验 + 各自测试，约 150 行结构重复 | fusion-ai `llm::factory` 增命名构造器（接受 region 字符串等中性参数，别名归一内置）+ `DashScopeRegion` 解析归一；两个 bin 各留薄 proto→参数映射。probe 侧「固定 timeout、永不 base_url override」是有意差异，保留为参数而非分叉 |
| 2 | STT 借道 chat 枚举：speech/infra/stt_route 先造 `LlmProviderConfig::Qwen`（携带 chat 专属 `default_chat_model`）再解构丢弃，喂 `FunAsrRealtime` | fusion-ai 提供一等 STT 构造路径（如 `FunAsrRealtime` 级工厂接受 credentials + region + model），STT 路由不再经 chat config enum |
| 3 | `DEFAULT_MODEL_*` 常量 7 处 import（credentials / ai_probe），未用 fusion-ai 已有的 `LlmProviderConfig::provider_default_model` 单点表；`ai_route` 的 `FALLBACK_MODEL` 是 fusion-ai `DEFAULT_MODEL_DEEPSEEK` 的字面量镜像 | 调用方改引 fusion-ai 单点表 / 常量，测试断言同步 |

**hetuos 侧行为不变约束**（重构 MUST 保持，均有测试锚点）：`LlmProviderConfig` variant 形状（credentials / ai_probe / residency / stt_route 四处 match 它）；`Qwen.base_url_override` 只承载部署形态注入（residency 集成测试靠它打本地假服务端）；wire 层按 `tool_choice` 自动关 thinking 的语义。

## 5. 阶段拆分

> 每阶段收口判据：阶段内 `cargo test` / `cargo check` 全绿才进下一阶段。fusions 是独立 workspace，fusions 侧命令在 `repos/fusions/` 下执行；本仓 / hetuos 侧在各自仓库根执行。

### P0 行为快照（fixture 基线）

在 rig 尚未移除的当前代码上，为 openai_compatible 建 golden fixture 测试（`crates/fusion-ai/tests/`，dev-dep 引入 `wiremock`）：

- Chat Completions：请求体形状（messages / tools / tool_choice / additional_params 注入）、非流式响应解析、流式 SSE（含 `[DONE]`）、错误体（4xx/5xx → `OpenAiCompatError` 雏形分类）。
- Responses：`input`/`instructions` 转换、`response.completed` 事件终态解析、流式 text delta 聚合、usage 提取。
- 多模态（image / image_edit / audio / transcription / embedding）：请求 multipart 形状与响应解析。
- 携密纪律：fixture 断言 Debug 输出不含 api_key（沿用 `config_debug_never_leaks_api_key` 模式）。

fixture 按端点方言组织样例（OpenAI 官方 / DashScope / DeepSeek / Kimi 各一），不只测「我们发的请求形状」，也测各端点响应方言的解析——这是多端点兼容的 provider parity 快照，后续加端点先加方言样例。

**验收**：`cargo test -p fusion-ai` fixture 套件全绿（此即重构行为基线；P1-P3 期间该套件保持绿 = 行为等价）。

### P1 openai_compatible 地基 + Chat Completions 本地化

- 新建 `types.rs` / `errors.rs`；`client.rs` 去 rig client trait（换具体方法：`completion_model` / `embedding_model` / `transcription_model` / `image_generation_model` / `audio_generation_model` / `verify`）。
- `completion/`（含 streaming）全部 rig 类型换本地；SSE 换 eventsource-stream（workspace 新增依赖；确认 reqwest 开 `stream` feature）。
- workspace 加 `eventsource-stream`。

**验收**：fixture 的 Chat Completions 段全绿；`cargo check -p fusion-ai --no-default-features` 绿。

### P2 Responses API 本地化

- `responses_api/`（含 streaming）rig 类型换本地；`CompletionRequest` ↔ 本地 `Message` 的互转改为本地 `Message` ↔ `types.rs::Message`。
- SSE 终止双形态（`[DONE]` 与 `response.completed`）进 fixture。

**验收**：fixture 的 Responses 段全绿。

### P3 多模态面本地化

- `embedding.rs` / `image_generation.rs` / `image_edit.rs` / `audio_generation.rs` / `transcription.rs` 换本地类型与错误；multipart 构造从 `rig::http_client::multipart` 换 `reqwest::blocking`/async multipart（reqwest `multipart` feature）。
- 删 `client_wrapper.rs`。

**验收**：fixture 全量绿；openai_compatible 模块内 `grep -rn "rig::"` 为零。

### P4 死面删除（rig 依赖归零）

- 删 `crates/fusion-ai/src/{client.rs, agents.rs, embeddings.rs}`、`lib.rs` 的 `pub use rig` / `factory` 模块 / `DefaultProvider`；`graph_flow/context.rs` 桥接换本地类型。
- `error.rs` 收敛为 §4.3 形态；`crates/fusions/src/error.rs` 分级改写（含测试）。
- examples 全量重写为本地 API（`complex_example` / `recommendation_flow` / `example-openai_compatible` / `example-gen_image` / `image_edit_demo`）。
- workspace 声明删除：`repos/fusions/Cargo.toml` 的 `rig` / `rig-postgres`（sqlx 注释同步更新为「解锁，升级另立项」）、`crates/fusions/Cargo.toml` 的 `rig`。

**验收**：
- `cargo tree -p fusion-ai | grep -c rig` = 0（Oracle：cargo tree）；`cargo metadata` 确认 aws-config / rig-* 全消失。
- `cargo test -p fusion-ai`、`cargo test -p fusions --features full`、examples `cargo check` 全绿。

### P5 creative-ai-service 迁移（分两步，风险隔离）

- **P5a 等价迁移**：`RigLlmProvider` → fusion-ai openai_compatible 适配器（三家全部 Chat Completions 形态，行为与现状一致：thinking 注入、max_tokens 分档、usage 兜底估算、`build_prompt_text` 拼接语义不变）。本仓 `Cargo.toml` 删 `rig` 声明。
- **P5b 切 Responses（真机 gate）**：DeepSeek、Qwen 切 Responses 默认形态（Kimi 保持 Chat Completions）。thinking 关闭在 Responses 形态的等价参数（DeepSeek `reasoning.effort` / Qwen 对应开关）以真机验证为准，验证通过才切；不过则该端点留在 Chat Completions 并在本计划记录原因。

  **真机验证结论（2026-08-14，smoke 见 `crates/fusion-ai/tests/p5b_responses_smoke.rs`，`-- --ignored --nocapture` 运行记录）**：
  - **Qwen 切 Responses** ✅：`reasoning.effort="none"` 完全关闭思考（`reasoning_tokens=0`，百炼官方口径 effort 优先于 enable_thinking）。非流式 1.04s（input=59/total=75），流式 0.93s（input=54/total=94），终态 usage 由 `response.completed` 携带。
  - **DeepSeek 留在 Chat Completions** ❌：Responses 形态的 effort 子集仅 `minimal/low/medium/high`（无 `none`），`minimal` 仍产出 reasoning token（非流式 reasoning_tokens=15；流式 total=402 vs 正文约 100 token）——`thinking:{type:disabled}` 的完全关闭语义在 Responses 形态无等价参数，planner 路径关 thinking 的目标（planner-refactor.md §4）无法达成。

**验收**：
- P5a：本仓 `cargo check -p creative-ai-service` 绿 + 既有单测绿；`cargo tree -p creative-ai-service` 无 rig。
- P5b：真机 smoke（`DASHSCOPE_API_KEY` / `DEEPSEEK_API_KEY` 跑一次 chat + stream，比对延迟与 usage）——evidence 形态：终端运行记录或标记 `#[ignore]` 的集成测试输出。

### P6 收尾与回流

- hetuos workspace 删 `rig` / `rig-postgres` 冗余声明；`cargo check`（消费 fusion-ai 的 bins）绿。
- skill 回流：`.agents/skills/fusions/SKILL.md` 与 `references/fusion-ai.md`（在 hetu-creative 仓）——rig re-export / ClientFactory / AgentConfig / `DefaultProvider` 段落改写为本地 API；`fusions` skill 的「19+ providers via rig」口径改为「OpenAI 兼容 wire + DashScope 原生」。
- `deny.toml` / cargo-deny 跑一次确认无新许可问题。

### P7 hetuos 调用方收敛（依赖 P4 完成后的 fusion-ai API 面）

- fusion-ai 侧：按 §4.4 增补中性构造 API（`llm::factory` 命名构造器 + region 别名归一 + STT 一等构造路径），均带单测。
- hetuos 侧（`/Users/ybx/hetus/hetuos`）：
  - `bins/hetu-ai/src/modules/chat/infra/credentials.rs` 与 `bins/hetu-infra/src/modules/message/ai_probe.rs` 的 `build_llm_config` 收敛到共享形态（proto 映射留 bin 侧，差异显式参数化）；
  - `bins/hetu-ai/src/modules/speech/infra/stt_route.rs` 改走 STT 一等构造，消除「造 chat config 再解构丢弃」；
  - `DEFAULT_MODEL_*` 7 处 import 与 `FALLBACK_MODEL` 镜像常量改引 fusion-ai 单点表。
- 顺序约束：hetuos 消费 submodule pinned commit，fusion-ai 增补合入 fusions 后 hetuos 需同步 bump submodule 指针再消费。

**验收**：
- fusion-ai 新增 API 单测绿（`cargo test -p fusion-ai`，在 `repos/fusions/` 下）。
- hetuos：`cargo test -p hetu-ai -p hetu-infra` 绿；`make check`（contracts + layering gate）绿——**gate 白名单零改动**是「未给 crate 层加依赖」的机器证据。
- 集成链路：hetuos `tests/suites/ai-chat.test.ts`（本地 OpenAI 兼容 mock server，PutCredential → PutRoute → Complete → usage 落库 → GetMyUsage 全链路）绿——它直接验证 `base_url_override` 直通与 Metered 装饰器行为未回退。

## 6. 验收总表

| # | 验收项 | Oracle | Evidence 形态 |
| --- | --- | --- | --- |
| 1 | fusion-ai 零 rig | `cargo tree -p fusion-ai`（repos/fusions 下） | 命令输出（grep rig 为空） |
| 2 | 行为等价 | P0 fixture 套件在 P1-P3 全程绿 | `cargo test -p fusion-ai` 输出 |
| 3 | 错误分级不回退 | `ai_error_maps_transient_upstream_to_service_unavailable`（改写后） | `cargo test -p fusions --features full` 输出 |
| 4 | 本仓去 rig | `cargo tree -p creative-ai-service` 无 rig；单测绿 | 命令输出 |
| 5 | qwen/deepseek Responses 真机可用 | P5b smoke（chat + stream + usage） | 运行记录 / `#[ignore]` 集成测试输出 |
| 6 | hetuos 脱钩不受损 | `cargo check`（hetuos 根，bins 覆盖）+ `cargo test -p hetu-ai -p hetu-infra` | 命令输出 |
| 7 | hetuos layering 不变 | `make check`（hetuos 根），gate 白名单零 diff | 命令输出 + git diff（白名单脚本） |
| 8 | hetuos AI 全链路不回退 | `ai-chat.test.ts` 集成套件（本地 mock provider 端到端） | vitest 输出 |
| 9 | skill 口径同步 | fusion-ai.md 无 rig-first API 残留 | diff |

## 7. 风险与已知兼容坑

| 风险 | 处置 |
| --- | --- |
| DashScope Responses 要求 assistant 消息 `content` 字段必须存在（缺失 → 400，社区案例 agentscope-java#1270） | 本地 `Message` 序列化保证 assistant content 恒序列化（空串）；fixture 加 DashScope 形态样例 |
| DeepSeek Responses 是无状态子集（无 `previous_response_id` / `store`），不支持图片输入，未知参数静默忽略 | 本地请求类型不暴露这些字段；「静默忽略」意味着打错参数不报错——fixture 断言我们下发的字段集合 |
| SSE 终止形态分歧（`[DONE]` 有无） | 解析器两种终态都收；fixture 双形态覆盖 |
| thinking 关闭在 Responses 形态的参数等价性未证实 | P5b 真机 gate，不过则不切（见 P5b） |
| rig umbrella 移除引发 Cargo.lock 大面积变动 | P4 一次收口，三个 workspace 全量 `cargo check` + `cargo test` |
| `verify()`（原 VerifyClient）语义迁移 | P3 并入 `Client::verify()`，fixture 覆盖 401/5xx 分支 |
| hetuos 经 submodule pinned commit 消费 fusion-ai，P1-P4 期间的中间态不会自动传导 | P7 前显式 bump submodule；P4 收口时 hetuos 的 lock 里 rig 才会消失，验收 #6 以 bump 后为准 |
| `Qwen.base_url_override` 语义漂移会静默打断 hetuos residency 集成测试的本地假服务端链路 | `base_url_override` 仅承载部署形态注入的既有口径不变；`ai-chat.test.ts` 作回归 gate（验收 #8） |
| P7 收敛若把 proto→config 转换塞进 hetu-core 或 fusion-ai，会分别撞 layering gate / 引入 proto 依赖 | §4.4 形态强制：中性参数 API + bin 侧薄映射；`make check` 白名单零改动为机器证据（验收 #7） |

## 8. 下一步

1. P0 动工：建 `crates/fusion-ai/tests/` + wiremock fixture 套件。
2. P5b 的 DeepSeek / Qwen thinking 参数等价性可提前并行真机验证（不阻塞 P0-P4）。
3. P7 的 fusion-ai 中性构造 API 可与 P1-P4 并行设计（不动 rig 相关代码路径），但 hetuos 侧改造 MUST 在 P4 收口 + submodule bump 之后。
