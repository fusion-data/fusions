# fusions 文档索引

本目录收录 `fusions` 子仓内 crate 的框架级文档。内容边界：只描述库 / 框架契约、crate 责任、通用扩展点、provider adapter 与验证 gate；不承载任何消费方业务系统、产品规格、应用授权策略或应用专属实现。

## 阅读顺序

1. [../README.md](../README.md) - workspace、crate 清单与开发命令。
2. [designs/index.md](./designs/index.md) - 框架设计文档索引。
3. [designs/workflow/hetuflow.md](./designs/workflow/hetuflow.md) - `hetuflow-*` workflow 框架规格。
4. [exec-plans/deferred/](./exec-plans/deferred/) - 触发驱动的框架级后续方案。

## 文档边界

| 保留在本仓 | 留在消费仓 |
|---|---|
| `fusion-*`、`fusion-sql`、`hetuflow*`、`fusion-ai` 的通用 API / crate 设计 | 应用流程、产品规格、UI 页面、权限码、组织 / scope 策略 |
| framework-level BDD、crate 责任、feature 组合、provider adapter 形状 | gateway 装配、业务 RPC、业务 schema、业务集成测试 |
| 与具体业务无关的执行计划、deferred 方案、迁移策略 | 业务系统、产品线、业务流程等应用侧方案 |

消费方可以链接本目录作为框架 SSOT，但业务语义必须在消费仓自己的规格或设计文档中定义。框架代码层的业务无关规则见 [framework-conventions §7](./designs/framework-conventions.md#7-框架业务无关消费方标识符零泄漏)。
