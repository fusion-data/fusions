# hetuflow 编排扩展设计记录

> Status: framework design record
> Scope: `hetuflow-core` + `hetuflow-runtime` + `hetuflow-sqlx`

本文保留 `hetuflow` framework 编排扩展的设计决策。业务 flow、业务 UI、业务 callback、业务权限和具体产品触发条件不属于本文件。

## 1. 已纳入基础契约的能力

| 能力 | 状态 | 核心抽象 |
|---|---|---|
| ReworkLoop | Implemented | `ReturnedWaiting`、round、`RETURNED` transition、`AdvanceOutcome::Rework` |
| Merge | Implemented | no-op routing node、`ResolveTrace.merges_passed` |
| Topology lint | Implemented | read-only graph lint findings |
| ForEach | Implemented | dynamic fan-out、`BranchExpectation::Dynamic`、item payload |
| Runtime context write-back | Implemented | node `contextWrites` whitelist + transactional patched context |
| Mixed Parallel | Implemented | heterogeneous static branch kinds + join incoming signal superset |
| SubWorkflow | Implemented | child instance start + parent resume through normal signal path |
| Compensation | Deferred | reverse side-effect handler registry and saga audit |

## 2. shared abstractions

### 2.1 `resolve_target` + `ResolveTrace`

Routing nodes MUST share a single target resolver.

- Condition evaluates guard and continues resolving.
- Merge records `merges_passed` and continues resolving.
- ForEach records `for_each_fan_out` and returns dynamic branch targets.
- Rework uses the same resolver for `RETURNED` target validation.

`decide_advance` returns `(AdvanceOutcome, ResolveTrace)`. New routing metadata MUST extend `ResolveTrace`; it MUST NOT create a second resolver path.

### 2.2 `BranchExpectation`

Join expected count has two sources:

| Variant | Source |
|---|---|
| `Static(usize)` | ParallelSplit static branch count |
| `Dynamic(usize)` | ForEach fan-out event payload |

`JoinStrategy::is_satisfied(arrived, expected)` remains strategy-only logic.

### 2.3 event-log extension

All new workflow events use `namespace.snake_case.v1` and JSON payload. Schema changes are reserved for projection/storage fields, not event type registration.

Required event payloads MUST include node id and round when replay needs them.

## 3. design decisions

| ID | Decision | Result |
|---|---|---|
| OD-1 | Rework uses middle state instead of completed-then-reactivate for precise rework | Adopted: `returned_waiting` is non-terminal |
| OD-2 | Topology lint is read-only first | Adopted: application may promote to publish gate later |
| OD-3 | Merge is no-op routing, not join counter | Adopted: parallel convergence still uses ParallelJoin |
| OD-4 | Compensation excluded from current baseline | Adopted: deferred until reverse handlers and saga cases exist |
| OD-5 | Event names use `.v1` suffix | Adopted |
| OD-6 | ForEach branch type is derived from branch template kind | Adopted |
| OD-7 | Join incoming signals are `{APPROVED, COMPLETED, EVENT_RECEIVED}` | Adopted |
| OD-8 | SubWorkflow requires normal signal re-entry for parent resume | Adopted |

## 4. ReworkLoop contract

- `Returned` without `RETURNED` transition completes workflow with returned result.
- `Returned` with `RETURNED` transition resolves a precise rework target.
- Precise rework increments round at target activity creation.
- Whole resubmit increments round at reactivate time.
- Both modes are bounded by effective max round.

## 5. Merge contract

- exactly one approved outgoing transition;
- no activity row, timer or outbox;
- append merge-passed event;
- reject parallel branch convergence into Merge.

## 6. Topology lint contract

Findings:

| Code | Severity | Meaning |
|---|---|---|
| `dangling_edge` | error | transition references missing node |
| `unreachable_node` | error | node cannot be reached from Start |
| `deadlock_node` | error | non-End node cannot progress |
| `parallel_unclosed` | warning | split branch cannot reach a join |

Lint is a pure analysis helper. Runtime execution semantics remain in `decide_advance`.

## 7. ForEach contract

- `items_path` MUST evaluate to JSON array.
- empty arrays are valid and immediately satisfy join.
- fan-out count MUST be recorded before join evaluation.
- branch template MUST be an activity node kind, not a routing or nested node kind.
- `max_fanout` protects against unbounded runtime expansion.

## 8. Runtime context write-back contract

- `signal_payload` is generic JSON object supplied by adapter.
- only node-declared `contextWrites` keys may be merged.
- unknown extra payload keys are ignored.
- missing declared keys are fail-closed.
- patched context is used for the immediately following advance decision.

## 9. Mixed Parallel contract

- Static branch target kind may be Approval, Assignment, EventWait or Notification.
- branch completion signal depends on branch node kind.
- Notification branch completion must go through the same join helper as signal-driven activity completion.

## 10. SubWorkflow contract

- parent activity stores child instance id;
- child has its own snapshot and event-log;
- parent resumes through normal `signal_workflow(EVENT_RECEIVED)` semantics;
- duplicate resume attempts are idempotent through parent activity status and idempotency key.

## 11. Deferred Compensation

Compensation requires more than framework mechanics. Activation prerequisites:

- application has real side effects that must be reversed automatically;
- reverse handlers are idempotent and registered;
- definition validation can prove handler availability;
- audit can distinguish forward side-effect, compensation intent, success and dead-letter.

Until those prerequisites exist, framework MUST keep compensation disabled.
