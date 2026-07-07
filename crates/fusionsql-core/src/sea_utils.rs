use crate::filter::FilterNodeOptions;
use sea_query::{Iden, IdenStatic, SimpleExpr, Value};

/// String sea-query `Iden` wrapper
#[derive(Debug)]
pub struct StringIden(pub String);

impl Iden for StringIden {
  fn unquoted(&self, s: &mut dyn std::fmt::Write) {
    // 写入 `String` 的 `fmt::Write` 实际不会失败（不会返回 Err），
    // 这里显式忽略返回值即可，无需 panic 也无需打印。
    let _ = s.write_str(&self.0);
  }
}

/// Static str sea-query `Iden` wrapper
#[derive(Debug, Clone, Copy)]
pub struct SIden(pub &'static str);

impl Iden for SIden {
  fn unquoted(&self, s: &mut dyn std::fmt::Write) {
    // 写入 `String` 的 `fmt::Write` 实际不会失败（不会返回 Err），
    // 这里显式忽略返回值即可，无需 panic 也无需打印。
    let _ = s.write_str(self.0);
  }
}

impl IdenStatic for SIden {
  fn as_str(&self) -> &'static str {
    self.0
  }
}

/// Convert a FilterNode value T into a sea-query SimpleExpr as long as T implements Into<sea_query::Value>
pub fn into_node_value_expr<T>(val: T, node_options: &FilterNodeOptions) -> SimpleExpr
where
  T: Into<Value>,
{
  let mut expr = SimpleExpr::Value(val.into());
  if let Some(cast_as) = node_options.cast_as.as_ref() {
    expr = expr.cast_as(StringIden(cast_as.to_string()));
  }
  expr
}
