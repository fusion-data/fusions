# hetuflow 工作流框架规格

> Status: 现行框架规格
> Scope: `hetuflow-core`、`hetuflow-runtime`、`hetuflow-sqlx`、`hetuflow`

`hetuflow` 是 Postgres-first durable workflow framework。它提供 workflow definition、event log、activity projection、timer、outbox、graph validation、advance decision 与 replay/rebuild 基础能力。

本规格是框架级 SSOT。消费方业务系统负责定义业务 flow、业务对象、权限、UI、callback handler、通知模板、scope 来源和 RPC 边界。

## 1. crate 边界

| Crate | 职责 |
|---|---|
| `hetuflow-core` | 领域类型、状态枚举、节点 / transition / event type、错误、纯 helper、adapter-facing trait |
| `hetuflow-runtime` | definition validation、graph lint、advance decision、CEL guard、replay reducer |
| `hetuflow-sqlx` | Postgres store trait 与默认实现、event-log / projection / outbox / timer 读写 |
| `hetuflow` | 聚合 crate 与 feature 入口 |

框架 crate MUST NOT 依赖消费方业务 crate、业务 proto、业务权限模型、业务通知系统或业务 UI。

## 2. 目标

- Workflow MUST 以 append-only event-log 作为历史事实源；当前状态只表达事实源的最新 projection。
- Workflow MUST 支持 caller idempotency，重复 start、signal、timer fire、outbox delivery 不得制造重复业务结果。
- Workflow definition MUST 可版本化、可校验、可 replay。
- Timer MUST 可恢复、可重试、可取消；业务 SLA MUST NOT 依赖进程内 sleep。
- Side effect MUST 先记录为 outbox intent，再异步投递。
- Framework MUST expose generic hooks; application adapters decide auth, scope, callback routing, notification delivery and audit policy.

## 3. 非目标

- MUST NOT 内置任何业务域语义、业务权限码、业务角色、业务状态机或 UI 流程。
- MUST NOT 替代消费方业务系统自己的状态机。
- MUST NOT 为第三方 provider、通知 channel 或业务 callback 制定消费方特定路由规则。
- MUST NOT 允许 application 绕过自身授权、审计和隔离策略；框架只提供承载点。

## 4. 核心概念

| 概念 | 规范 |
|---|---|
| Definition | 工作流定义，描述节点、边、guard、timer、callback、notification、assignment 与终态结果。 |
| Instance | 某个业务对象上的 workflow 实例。active instance SHOULD 按业务 key 去重。 |
| Activity | 节点运行实例。Activity MUST 有明确状态、输入、输出和幂等边界。 |
| Signal | 外部输入事件，用于推进等待中的 workflow。Signal caller MUST 提供幂等键。 |
| Timer | 持久化时间触发点，用于提醒、升级、超时和计划触发。 |
| Outbox | 副作用投递意图。框架保证投递状态可追踪，不承诺业务侧 exactly-once。 |
| Event log | append-only 历史事实源，projection 与 drift report MUST 可由其重建。 |

## 5. 执行语义

```mermaid
flowchart LR
    A[Start request] --> B{Active instance exists?}
    B -- yes --> C[Return existing / idempotent result]
    B -- no --> D[Validate definition and context]
    D --> E[Append workflow facts]
    E --> F[Update current projection]
    F --> G[Create timer / outbox intents]
    G --> H[Async workers deliver side effects]
    I[Signal request] --> J[Validate target activity and idempotency]
    J --> E
```

- Start MUST validate definition before writing instance state.
- Signal MUST target a valid active activity or a valid business correlation chosen by the adapter.
- Timer fired MUST follow definition timeout behavior and remain idempotent.
- Outbox delivery success MAY complete side-effect activity only after the adapter confirms delivery success.
- Rejected / Returned / Completed / EventReceived semantics are determined by `hetuflow-runtime` and the node transition graph.

## 6. 节点模型

| 节点 | 规范 |
|---|---|
| Start | Definition entry. |
| Approval | Human or external approval gate. Framework stores role code as opaque string. |
| Condition | CEL-based routing node. MUST have default branch or provable full coverage. |
| Timer | Durable wait / timeout node. |
| Notification | Notification intent. Provider and template are adapter-owned. |
| BusinessCallback | Business side-effect intent. Handler routing is adapter-owned. |
| Assignment | Work item intent. Queue / role semantics are adapter-owned. |
| EventWait | Waits for external event correlation. Buffering/replay policy is adapter-owned. |
| ParallelSplit / ParallelJoin | Static branch fan-out and join with explicit strategy. |
| Merge | Serial no-op routing node; does not create activity row. |
| ForEach | Dynamic fan-out over CEL-evaluated array; joins through ParallelJoin. |
| SubWorkflow | Starts and waits for a child workflow instance. |
| End | Terminal workflow result. |

## 7. validation

- Definition MUST declare stable node ids and transitions.
- Definition MUST have valid Start / End topology.
- Definition MUST NOT contain dangling edges, unreachable nodes, deadlock nodes or unsupported nested structures.
- Expressions MUST be deterministic; parse or evaluation failure MUST be validation / runtime error, not silent branch selection.
- Running instance MUST use its bound definition snapshot; later definition changes MUST NOT rewrite in-flight semantics.

## 8. event-log 与 projection

- event type MUST use `namespace.snake_case.v1`.
- event payload MUST be self-describing enough for replay: include relevant node id, round, result, item index / payload and correlation facts where applicable.
- Projection rows are query optimization, not the source of truth.
- Drift report compares replayed canonical projection with live projection.
- Rebuild apply MUST restore projection to event-log-consistent state and MUST be auditable.

Detailed reducer / rebuild design: [hetuflow-projection-traceability.md](./hetuflow-projection-traceability.md).

## 9. 编排扩展契约

### 9.1 ReworkLoop

- Node declaring `RETURNED` transition enters precise rework: runtime resolves target, creates a new active activity at `round + 1`, and sets instance to `returned_waiting`.
- Node without `RETURNED` transition keeps whole-workflow resubmit semantics: completed with returned result, then adapter may reactivate from start.
- Effective round limit = `min(definition.max_round, framework global max)`.
- Rework target MUST be a valid upstream target declared by definition.

### 9.2 Merge

- Merge MUST have exactly one approved outgoing transition.
- Merge MUST NOT create activity rows, timers or outbox entries.
- Passing through Merge MUST append `workflow.merge_passed.v1`.
- Parallel branches MUST join with ParallelJoin, not Merge.

### 9.3 Topology lint

- `lint_definition` SHOULD expose dangling edge, unreachable node, deadlock node and unclosed parallel findings.
- Lint is read-only unless the consuming adapter explicitly promotes it to publish gate.

### 9.4 ForEach

- ForEach MUST declare `items_path`, `branch_template`, `join_target` and optional `max_fanout`.
- Runtime fan-out count MUST be recorded in `workflow.for_each_fanned_out.v1`.
- Join expectation for ForEach MUST be dynamic and round-scoped.
- Empty array is valid and SHOULD immediately satisfy the join path.

### 9.5 Runtime context write-back

- Runtime context defaults to Start-provided immutable input.
- Write-back is allowed only for fields explicitly listed in node `contextWrites`.
- Adapter MUST validate payload shape and value domain before write-back.
- Write-back MUST happen in the same transaction as activity completion and subsequent advance.

### 9.6 Mixed Parallel

- ParallelSplit branch target kind MAY be Approval, Assignment, EventWait or Notification.
- Branch activity type MUST be derived from branch node kind.
- Join incoming signals are `{APPROVED, COMPLETED, EVENT_RECEIVED}`.

### 9.7 SubWorkflow

- SubWorkflow starts child instance and keeps parent activity active until child terminal result is observed.
- Parent resume MUST re-enter normal signal path, not a simplified worker-only advance path.
- Definition validation MUST reject SubWorkflow nodes without a resumable EVENT_RECEIVED transition.

### 9.8 Compensation

Compensation / saga remains deferred. It MUST NOT be enabled until a consuming application defines idempotent reverse handlers and validates registration at definition publish time.

Detailed design record: [hetuflow-orchestration-gaps.md](./hetuflow-orchestration-gaps.md).

## 10. application adapter 责任

Application adapter owns:

- proto / HTTP / CLI surface;
- caller auth, permission and scope decisions;
- mapping trusted context into store filters;
- business callback handler registry;
- notification delivery integration;
- admin mutation authorization and audit sink;
- UI, reporting, and product-specific workflow semantics.

Framework crates only expose deterministic decisions and storage primitives.
