# hetuflow event-log 可追溯与 projection 可重建

> Status: framework design
> Scope: `hetuflow-runtime::replay` + `hetuflow-sqlx` store contract

本文定义 `hetuflow` 框架层的 event-log replay、projection drift 和 rebuild 边界。消费方负责把这些能力暴露为 RPC、CLI、UI 或运营流程。

## 1. 事实源

| 维度 | 规范 |
|---|---|
| event-log | append-only、seq 单调、按 workflow instance 有序、payload self-describing |
| projection | instance / activity / outbox / timer 等查询投影，可由 event-log 或 outbox 状态核验 |
| definition snapshot | instance start 时绑定，replay MUST 使用该 snapshot，不使用 live definition |
| scope | store 接收 generic `ScopeFilter`；具体 scope 来源由 adapter 决定 |

## 2. reducer

`hetuflow-runtime` owns deterministic replay:

```rust
pub fn fold_events(
    snapshot: &DefinitionSnapshot,
    events: &[EventRecord],
) -> ReplayedProjection;
```

Reducer requirements:

- MUST be pure and deterministic.
- MUST process events ordered by `seq ASC`.
- MUST cover every framework event type used by current runtime.
- MUST treat projection fields as canonical only when they can be derived from event-log payload.
- MUST not read database, clock, network, application config or external services.

## 3. canonical projection subset

| Projection field group | Source | Canonical |
|---|---|---|
| instance status / result / timestamps | workflow lifecycle events | Yes |
| current activity / activity status / round | activity scheduled/completed/skipped/failed events | Yes |
| context initial value | workflow started payload | Yes if payload includes it |
| runtime context write-back | `workflow.context_updated.v1` | Yes if payload includes changed keys |
| side effects executed | outbox state | Cross-check, not event-log-only |
| reviewer notes / soft display fields | signal payload | Canonical only when payload carries them |

Non-canonical fields MAY be reported as advisory drift but MUST NOT make rebuild overwrite unknown facts.

## 4. drift report

Drift report compares:

1. load replay input: definition snapshot + ordered event records;
2. `fold_events(snapshot, events)`;
3. live projection rows;
4. canonical diff.

Report output SHOULD include scanned count, matched count and mismatched summaries. It MUST NOT mutate projection.

## 5. rebuild apply

Rebuild apply restores live projection to replayed canonical projection.

Requirements:

- MUST run inside one write transaction.
- MUST lock the target workflow instance before overwrite.
- SHOULD default to terminal instances only.
- MAY support active instances only with explicit adapter-level safety gate.
- MUST write an admin/audit event through the consuming adapter.
- MUST be idempotent when event-log is unchanged.

## 6. retention / archive

Retention is deferred until event-log volume creates operational pressure.

When implemented:

- archive only terminal instances whose event-log replay matches live projection;
- prefer archive table move before destructive purge;
- keep dry-run as default for operator workflows;
- partition by application-chosen ownership / scope fields only through adapter contract.

## 7. tests

- one reducer fixture per event type;
- compound replay fixture covering Merge, ForEach, rework, Mixed Parallel and SubWorkflow;
- drift injection detects mismatch;
- rebuild apply fixes drift and is idempotent;
- scope-filtered list/load functions fail closed under empty visibility.
