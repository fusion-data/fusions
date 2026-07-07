//! CEL guard evaluation —— Phase E DAG 条件分支求值器。
//!
//! 设计选型（A4：首选 CEL；2026-05-22 锁定）：
//! - 表达式语言：[CEL](https://github.com/google/cel-spec)（Common Expression Language），
//!   pure-Rust 实现来自 `cel` crate v0.13。
//! - 根变量：`context` 绑 workflow 实例的 `context` JSON（业务数据如 `amount` / `level`）。
//! - 沙箱：CEL 默认不暴露 IO / 时间 / 随机；表达式必须 deterministic（满足 T2 replay）。
//!
//! ## 性能注记（未来优化路径）
//!
//! 当前实现走「`serde_json::Value` → `add_variable` 物化为 `cel::Value`」简单路径，
//! 每次 advance 评估一次。审批 advance 频率人为驱动（~每信号一次，秒/分级），
//! materialize 成本 µs 级 + Program 编译 µs 级，无热点。
//!
//! 若未来 Condition 在 hot-path 评估（如 timer scanner 大批 Condition 检查、跨实例
//! 聚合规则），可按 <https://blog.howardjohn.info/posts/cel-fast/> 升级为：
//! 1. 缓存 compiled `Program`（按表达式串 keyed；snapshot 不可变，guard 串永不变）；
//! 2. 实装 `DynamicType` / `VariableResolver` trait，从原生 Rust 类型（或借用的 JSON
//!    引用）懒访问字段，避开整 JSON 物化（21ns vs 147ns、250K→400K QPS 量级提速）。
//!
//! 当前 v1 不需要。

use cel::parser::ParseErrors;
use cel::{Context, Program, Value};

/// 把 CEL parse 错误压成 `line:col msg; line:col msg` 形式，避免 Debug 暴露
/// `source: None` / `expr_id` / `SourceInfo` 等内部字段。
fn format_parse_errors(e: ParseErrors) -> String {
  e.errors
    .iter()
    .map(|pe| format!("line {}:{} {}", pe.pos.0, pe.pos.1, pe.msg))
    .collect::<Vec<_>>()
    .join("; ")
}

/// 评估 CEL guard：以 `context_value` 绑根变量 `context`，要求表达式返回 bool。
///
/// `context_value = None` → 绑空对象（guard 引用 `context.xxx` 会得 NULL / missing
/// key 视 CEL 语义而定）。错误一律映射为字符串，由调用方决定如何处理（service
/// 把 CEL 错当作 engine error，进入 AdvanceOutcome::Error）。
pub fn eval_guard(expression: &str, context_value: Option<&serde_json::Value>) -> Result<bool, String> {
  let program = Program::compile(expression).map_err(|e| format!("CEL parse error: {}", format_parse_errors(e)))?;
  let mut ctx = Context::default();
  let ctx_value = context_value.cloned().unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
  ctx.add_variable("context", ctx_value).map_err(|e| format!("CEL bind error: {e:?}"))?;
  let result = program.execute(&ctx).map_err(|e| format!("CEL eval error: {e:?}"))?;
  match result {
    Value::Bool(b) => Ok(b),
    other => Err(format!("guard expression must return bool, got: {other:?}")),
  }
}

/// 仅编译表达式（不绑变量、不执行）—— definition INSERT 时 eager 校验 guard 语法。
pub fn validate_expression(expression: &str) -> Result<(), String> {
  Program::compile(expression)
    .map(|_| ())
    .map_err(|e| format!("CEL parse error: {}", format_parse_errors(e)))
}

/// G2 ForEach：求值 `items_path` CEL 表达式，要求返回 JSON 数组（绑根变量 `context`）。
///
/// 与 `eval_guard` 同沙箱/同根变量绑定，但要求结果是 list 而非 bool：
/// 求值结果转回 `serde_json::Value`，仅当为 `Array` 时返回其元素 `Vec`；
/// 非数组 / parse 错 / eval 错 → `Err`（service 当 engine error，流程进 ERRORED）。
pub fn eval_array(
  expression: &str,
  context_value: Option<&serde_json::Value>,
) -> Result<Vec<serde_json::Value>, String> {
  let program = Program::compile(expression).map_err(|e| format!("CEL parse error: {}", format_parse_errors(e)))?;
  let mut ctx = Context::default();
  let ctx_value = context_value.cloned().unwrap_or(serde_json::Value::Object(serde_json::Map::new()));
  ctx.add_variable("context", ctx_value).map_err(|e| format!("CEL bind error: {e:?}"))?;
  let result = program.execute(&ctx).map_err(|e| format!("CEL eval error: {e:?}"))?;
  let json = result.json().map_err(|e| format!("CEL result to json error: {e:?}"))?;
  match json {
    serde_json::Value::Array(items) => Ok(items),
    other => Err(format!("items_path expression must return an array, got: {other}")),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parse_only_validation() {
    assert!(validate_expression("context.amount > 100").is_ok());
    assert!(validate_expression("context.amount >>").is_err());
  }

  #[test]
  fn eval_with_context() {
    let ctx = serde_json::json!({"amount": 200, "level": "high"});
    assert!(eval_guard("context.amount > 100", Some(&ctx)).unwrap());
    assert!(!eval_guard("context.amount > 500", Some(&ctx)).unwrap());
    assert!(eval_guard("context.level == 'high'", Some(&ctx)).unwrap());
  }

  #[test]
  fn eval_no_context_missing_field() {
    // 缺 context 时绑空对象；context.amount 未定义 → 表达式行为按 CEL 规范处理
    // （多数会报 no such attribute / no such key）—— 视为 eval 错（service 当 engine error）。
    let _ = eval_guard("context.amount > 100", None);
  }

  #[test]
  fn eval_non_bool_returns_err() {
    let ctx = serde_json::json!({"amount": 100});
    let err = eval_guard("context.amount", Some(&ctx)).unwrap_err();
    assert!(err.contains("must return bool"), "got: {err}");
  }
}
