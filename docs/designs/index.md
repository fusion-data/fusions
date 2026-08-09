# 设计文档索引

`designs/` 收录 `fusions` 子仓的框架级设计。本文档只面向库维护者和消费方集成者，不描述任何业务系统的产品需求。

## 文档类型

| 文档 | 类型 | 摘要 |
|---|---|---|
| [framework-conventions](./framework-conventions.md) | 框架约定 SSOT | `fusion-*` / `fusion-sql*` 横切约定：错误语义、安全默认（携密 Debug 脱敏 / ORDER BY 实体列名单）、并发资源（client 复用 / abort-on-drop）、事务感知路由、DI API 命名（panic/`try_` 配对）、宏路径卫生、**框架业务无关（§7）** |
| [workflow/hetuflow](./workflow/hetuflow.md) | 框架规格 | `hetuflow-core` / `hetuflow-runtime` / `hetuflow-sqlx` / `hetuflow` 的 durable workflow 契约 |
| [workflow/hetuflow-projection-traceability](./workflow/hetuflow-projection-traceability.md) | 框架设计 | event-log 可追溯、projection 可重建、reducer / drift / rebuild / retention 边界 |
| [workflow/hetuflow-orchestration-gaps](./workflow/hetuflow-orchestration-gaps.md) | 框架设计记录 | rework loop、Merge、Topology lint、ForEach、Mixed Parallel、SubWorkflow、Compensation 设计决策 |

## 归属规则

- `hetuflow` 文档只定义 workflow kernel 的通用模型与 crate 责任。
- 业务系统必须在自己的仓库中定义 flow type、业务对象、权限码、UI、通知模板、callback handler 和数据归属。
- 本目录中的 SQL 表名、trait 名和 event type 是框架契约；具体 schema 文件、RPC 服务和 UI 页面由消费方 adapter 决定。
