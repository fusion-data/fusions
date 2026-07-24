//! hetuflow-service —— 工作流执行态的事务内编排 + worker 循环骨架。
//!
//! ## 在 crate 图里的位置
//!
//! ```text
//! hetuflow-core     领域类型 / 纯 helper / 端口 trait
//!   └─ hetuflow-runtime   图校验 · 推进决策 · CEL guard · replay reducer
//!        └─ hetuflow-sqlx   Postgres store（A1：接 caller 的事务，不自开、不注入 GUC）
//!             └─ hetuflow-service   ← 本 crate：把决策与存储组合成原子编排 + worker 骨架
//! ```
//!
//! 落点为什么是独立 crate 而不是 `hetuflow-sqlx` 内的模块：store crate 的契约是「在 caller 的
//! 事务上跑 SQL」，编排是另一层职责；合并会让只需要 store 的消费者被迫编译 runtime，并模糊
//! A1 边界。feature 图仍单调（`core` → `runtime` → `sqlx` → `service`），因此只开 `runtime`
//! 的消费者（如设计态编译器）的依赖闭包里既没有 `hetuflow-sqlx` 也没有本 crate。
//!
//! ## 三条纪律
//!
//! 1. **不开事务、不设会话变量**：请求路径的方法都接 caller 已在事务内的 `&DbxPostgres`；
//!    worker 需要跨事务时经 [`TxnRunner`] 端口由宿主给出。
//! 2. **事实先于投影**：任何状态变化都先 append event，再改投影行；`fold_events` 必须能从
//!    event-log 重建 canonical 投影（事件 payload 的键是契约，见 `service.rs` 头注）。
//! 3. **副作用先记 intent**：通知与业务回调一律进 outbox，投递由 worker 异步完成；
//!    `side_effects_executed` 只在 handler 确认成功后翻转。
//!
//! ## 本期未接线（fail-closed，不是静默降级）
//!
//! - `SubWorkflow` 节点：需要「按 flow_type 解析 definition」的端口，store 契约未提供；
//!   调度到该 kind 会显式报错而不是把父实例挂死。
//! - Compensation / saga：框架规格 §9.8 显式 deferred。

pub mod command;
pub mod ports;
pub mod service;
pub mod worker;

pub use command::{ProjectionVerification, SignalCommand, SignalOutcome, StartCommand, StartOutcome, TerminalCallback};
pub use ports::{CallbackRegistry, NotificationDispatcher, TxnFuture, TxnRunner};
pub use service::{MAX_ROUND_HARD_LIMIT, WorkflowConfig, WorkflowService, timer_kind};
pub use worker::{OutboxDispatcher, TimerPoller, WorkerConfig};
