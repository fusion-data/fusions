# 框架横切约定（错误语义 / 安全默认 / 并发资源 / DI API / 宏卫生）

> 基线：2026-07-22 评审修复定稿。本文是 `fusion-*` / `fusionsql*` crate 横切约定的 SSOT；条款与实现同源，验证锚点指向对应回归测试。业务语义一律留在消费仓。

## 1. 错误语义

| 约定 | 条款 |
|---|---|
| 501 vs 503 | `Unimplemented` / `codes::NOT_IMPLEMENTED`（`system.not_implemented`）是**永久性失败**，MUST NOT 映射为可重试的 503（`SERVICE_UNAVAILABLE`）；Connect ↔ `DataError` 双向映射 MUST 保留 `Unimplemented` 往返 |
| 装配缺陷 ≠ 认证失败 | `ModelManager` 未 `with_ctx` 即做上下文相关操作 → `SqlError::CtxMissing` → 500（`INTERNAL_ERROR`）。MUST NOT 复用 401/`Unauthorized`——会把服务端装配 bug 伪装成客户端认证问题，误导排障 |
| 上游瞬态 vs 本地缺陷 | `AiError` 映射分级：上游 HTTP / Provider 错误 → 503（可重试）；请求构造 / 响应解析 / 工厂装配缺陷 → 500（重试无意义）。MUST NOT 把整类错误压成单一 500 |
| 错误链 | 跨 crate `From<X> for DataError` 实现 MUST 用 `with_source` / `internal(.., source)` 保留源错误链；MUST NOT 只留 `to_string()` |

验证锚点：`crates/fusions/src/error.rs` `#[cfg(test)]`（`not_implemented_code_survives_connect_round_trip` / `ai_error_maps_transient_upstream_to_service_unavailable` / `sql_ctx_missing_maps_to_internal_not_unauthorized`）。

## 2. 安全默认

| 约定 | 条款 |
|---|---|
| 携密类型 Debug | 持有 `api_key` / secret / credential 的类型 MUST NOT `#[derive(Debug)]`；MUST 手写脱敏实现（敏感字段打印 `<REDACTED>`）。判据：`tracing::debug!(?value)` / 错误日志会把派生 Debug 的明文密钥落盘 |
| ORDER BY 名单 | 客户端提交的分页排序列 MUST 校验（opt-out 安全默认）：无显式名单时按实体列集合 `HasFields::field_names()`；`with_order_by_allowlist` 为显式覆盖（收紧或放开 join / 计算列）。服务端默认排序（`BmcConfig.order_bys`）是受信配置，不经校验。判据：任意列名排序 = ORDER BY oracle 侧信道 + schema 探测 + 无索引慢排序面 |
| 半可信数值输入 | 来自外部 provider 的数值（如 OAuth `expires_in`）参与时间 / 大小运算 MUST 用 `saturating_*`；判据：恶意超大值 debug 溢出 panic、release 回绕 |

验证锚点：`llm/wire_openai_compat.rs` / `providers/dashscope/mod.rs` / `client.rs` 各自的 `debug_never_leaks_api_key`；`crates/fusionsql/src/base/utils.rs` `#[cfg(test)]` + `crates/fusionsql/tests/test_sqlite_order_by_entity_columns.rs`。

## 3. 并发与资源

| 约定 | 条款 |
|---|---|
| HTTP client 复用 | `reqwest::Client` MUST 构建一次并缓存复用（连接池）；MUST NOT 每请求重建。per-request 超时走 `RequestBuilder::timeout` 覆盖，不重建 client |
| 辅助任务取消安全 | `tokio::spawn` 的辅助任务持有连接 / 流（WebSocket sink、音频流等）时，其 handle MUST 具备 abort-on-drop 语义（`tokio_util::task::AbortOnDropHandle`）。判据：裸 `JoinHandle` drop 只 detach，消费方提前取消 → 任务持连接无限拉流泄漏 |
| 流式解析容错 | 解析外部 provider 流式分块（SSE tool_call 等）时，不合规分块 MUST 跳过（`debug!` 记录），MUST NOT `expect`/`unwrap` 打穿流任务 |

验证锚点：`wire_openai_compat.rs` `with_timeout_resets_cached_client`；`fun_asr.rs` 2c/2e 注释锚点。

## 4. 数据访问

| 约定 | 条款 |
|---|---|
| 事务感知路由 | 一切 CRUD（含 `count` / `count_on`）MUST 经 `Dbx*` 的事务感知 `fetch_*` / `execute`；MUST NOT 对连接池直接 `sqlx::query(...).fetch_*(pool)`。判据：SQLite file 库绕过事务读到旧快照 / 撞写锁；`:memory:` 各连接是独立数据库 |
| 审计 0 哨兵 | `impl ModelContext for Ctx` 在无 user id 时 `audit_user_id()` 返回 `0`（system / 未归因哨兵）。依赖精确归因的消费方 MUST 自定义 `AppContext: ModelContext` 并把 audit actor 设为必填 |

验证锚点：`crates/fusionsql/tests/test_sqlite_count_txn.rs`。

## 5. DI 与公共 API 命名

| 约定 | 条款 |
|---|---|
| panic / fallible 配对 | 每个访问器按返回形态提供一对：panic 版短名（`component` / `component_arc` / `add_component` / `add_config_source`，MUST 标 `#[track_caller]` 并在 doc 指向 fallible 兄弟）+ fallible 版 `try_` 前缀。MUST NOT 用 `get_` 前缀返回 `Result`（与 std `get → Option` 直觉冲突；旧名已 `#[deprecated]` 委托） |
| 命名如实 | 方法名 MUST 反映行为：跑服务循环到关机的方法叫 `serve()` 而非 `build()`（`WebServerBuilder::build` 已 deprecated） |
| builder 链一致 | builder 方法 MUST 统一返回 `&mut Self`（或统一 `Self`），MUST NOT 混入 `&Self` 断链 |
| shutdown hooks | `add_shutdown_hook` 注册的钩子在 `Application::await_shutdown` 内按注册顺序执行；单个失败记日志不阻断其余。进程不调 `await_shutdown` 则钩子不执行（文档已声明） |
| 分页 wire 契约 | `Page` / `Paged` / `PageResult` / `OrderBy(s)` 统一 camelCase（`orderBys` / `hasMore`）；`Page` 保留 `order_bys` serde alias 兼容旧入参。构造器 MUST 提供 `new_with_has_more`，`with_has_more` MUST `#[must_use]`。判据：`new()` 默认 `has_more=false` 是"加载更多永不出现"的静默陷阱 |

验证锚点：`crates/fusion-core/tests/test_shutdown_hooks.rs`；`fusionsql-core/src/page/` 各 `#[cfg(test)]`。

## 6. 宏卫生

| 约定 | 条款 |
|---|---|
| `macro_rules!` 路径 | 导出宏内部引用本 crate 项 MUST 用 `$crate::` 路径（如 `submit_component!`）；MUST NOT 硬编码聚合 crate 名 |
| derive 生成路径 | `#[derive(Component)]` / `#[derive(Configuration)]` 生成代码默认引用 `::fusions::core`；直依 `fusion-core` 的消费方用容器属性 `#[fusions(crate = "::fusion_core")]` 覆盖。`FilterNodes` / `Fields` / sea-value derive 生成代码 MUST 用 `::fusionsql::...` / `::fusionsql::sea_query::...` 绝对路径（`fusionsql` 已 `pub use sea_query`），下游 MUST NOT 被迫自带同名直接依赖 |
| 属性解析诊断 | proc-macro 属性解析失败 MUST 经 `syn::Error → to_compile_error()` 发正常编译诊断；MUST NOT `unwrap()` panic（报 "proc-macro panicked" 无 span 难定位） |
| protected 命名 | trait 方法前导下划线（如 `DbBmc::_bmc_config`）= protected 约定：实现方提供、仅供框架函数读取，业务代码 MUST NOT 直接调用 |

## 7. 框架业务无关（消费方标识符零泄漏）

> 术语：**fusions** = 本仓所有 crate；**消费方 (consumer)** = 任何依赖 fusions 的应用 crate / bin / 项目；**消费方标识符** = 产品名 / 服务名 / proto 包名 / cookie 名 / 库名 / 表名 / 环境变量前缀 / 角色码 / 错误码前缀 / metric 名 / 默认配置值等专属于某消费方的名称。文档归仓边界见 [../README.md](../README.md#文档边界)。

fusions 是业务无关的 lib / framework，同时服务多个消费方（如 hetuos、hetu-creative）。框架代码 MUST NOT 硬编码、MUST NOT 在默认值里固化任何消费方标识符。

**Apply（新增 / 修改 fusions 代码时逐条核对）**：

| 载体 | MUST |
|---|---|
| 标识符 | 类型 / 函数 / 模块 / crate 名 MUST 用框架中立词；消费方专有名只出现在消费方传给框架的配置（如 `AuthConfig`、`ContextValidationConfig`）里 |
| 默认值 | `Configurable` 默认配置、`*Config::DEFAULT`、`default.toml`、`DEFAULT_*` 常量 MUST 取框架中立值；消费方特化（如 cookie 名、RPC 包名）由消费方构造时覆盖 |
| 日志 / metric | 框架发出的 `log::*` / `tracing::*` 文本与 metric 名 MUST 用 `fusion_*` / crate 自有前缀；消费方身份由 OTLP resource / Loki service label 等**可观测性管道**区分，不塞进 per-request config |
| 测试 fixture | `#[cfg(test)]` 的示例 RPC 路径 / cookie 名 / service 名 MUST 用中立占位（`myapp.*.v1.*Service` / `access_token`）；示例 DB 连接串用 `postgres://user:password@host/db` |
| 注释 / doc | `///` / `//!` / README / 示例 MUST NOT 出现任何消费方的真实产品名 / 服务名 / 库名；指代消费方时用「消费方 (consumer)」统称，举例用 `<consumer>` / `<app>` 占位 |

**MUST NOT**：
- 在框架代码路径（含 `debug_assert!` 消息、`panic!` 消息、`#[error(...)]` 文本）写入消费方 crate 路径（如 `<consumer>_core::db::...`）或消费方产品名
- 把消费方 proto 包名 / cookie 名 / DB 实例名写进框架默认配置或文档示例
- 用「某一消费方的现状」作为框架默认值（「因为 hetuos 现在是 X，所以默认 X」→ ❌）

**冲突 / Stop**：当一条框架默认值与某消费方约定一致时，MUST 验证它对**其他消费方也中立**后才保留；只对一个消费方合理 → MUST 改为该消费方传参，框架默认取通用值。

**验证锚点**：
- `grep -rniE 'hetuos|hetu-creative|hylx|careos' --include='*.rs' --include='*.toml' --include='*.md' .` → MUST 零命中（`hylx` / `careos` 是历史消费方名，已于 2026-08 清除）
- `crates/fusion-rpc/src/auth_middleware.rs::test_default_cookie_token_name_is_business_agnostic` —— 守护 `AuthConfig::DEFAULT.cookie_token_name = "access_token"`（框架默认中立）
- 同文件 L200/L207 的 `metric=fusion_rpc.auth.*` —— 框架日志 metric 前缀中立范例

## 易错点速查

| 症状 | 根因 → 条款 |
|---|---|
| 客户端反复重试一个永远失败的 RPC | `Unimplemented` 被映射成 503 → §1 |
| 日志里出现 `api_key: "sk-..."` | 携密类型派生 Debug → §2 |
| 每次 LLM 调用都 TCP+TLS 握手 | client 每请求重建 → §3 |
| 客户端断开后 WS 连接 / 音频流不释放 | 裸 JoinHandle detach → §3 |
| SQLite 事务内分页 total 恒为旧值 / 报 busy | count 直查连接池 → §4 |
| 按响应里不存在的列排序竟然成功 | 无 ORDER BY 名单（旧 opt-in 行为）→ §2 |
| `created_by = 0` 的记录 | Ctx 无 user id 的哨兵写入 → §4 |
| 下游 `#[derive(Component)]` 报 "use of undeclared crate `fusions`" | 未走伞 crate 且未配 `#[fusions(crate = ...)]` → §6 |
| 前端收到 `order_bys` 但响应是 `hasMore` | 请求侧漏配 camelCase（已统一）→ §5 |
