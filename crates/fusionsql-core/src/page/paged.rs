use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
#[cfg_attr(target_arch = "wasm32", derive(tsify::Tsify), tsify(into_wasm_abi, from_wasm_abi))]
pub struct Paged {
  pub total: u64,
  pub has_more: bool,
}

impl Paged {
  /// Construct with `has_more = false`. **Most callers should use
  /// [`Self::new_with_has_more`]** — forgetting `with_has_more(true)` is the
  /// most common reason "前端加载更多按钮永不出现"。
  #[must_use]
  pub fn new(total: u64) -> Self {
    Self { total, has_more: false }
  }

  /// Construct with explicit `total` + `has_more`. Preferred over
  /// `new(total).with_has_more(true)` to avoid the silent-default footgun.
  #[must_use]
  pub fn new_with_has_more(total: u64, has_more: bool) -> Self {
    Self { total, has_more }
  }

  #[must_use]
  pub fn with_has_more(mut self, has_more: bool) -> Self {
    self.has_more = has_more;
    self
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
#[cfg_attr(target_arch = "wasm32", derive(tsify::Tsify), tsify(into_wasm_abi, from_wasm_abi))]
pub struct PageResult<T> {
  pub page: Paged,
  pub result: Vec<T>,
}

impl<T> PageResult<T> {
  /// Construct with `has_more = false`. **Most callers should use
  /// [`Self::new_with_has_more`]** — forgetting `with_has_more(true)` is the
  /// most common reason "前端加载更多按钮永不出现"。
  #[must_use]
  pub fn new(total: u64, result: Vec<T>) -> Self {
    Self { page: Paged::new(total), result }
  }

  /// Construct with explicit `total` + `has_more`. Preferred over
  /// `new(...).with_has_more(true)` to avoid the silent-default footgun.
  #[must_use]
  pub fn new_with_has_more(total: u64, has_more: bool, result: Vec<T>) -> Self {
    Self { page: Paged::new_with_has_more(total, has_more), result }
  }

  #[must_use]
  pub fn with_has_more(mut self, has_more: bool) -> Self {
    self.page = self.page.with_has_more(has_more);
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn page_result_new_with_has_more_mirrors_paged() {
    let r = PageResult::new_with_has_more(100, true, vec![1, 2, 3]);
    assert_eq!(r.page.total, 100);
    assert!(r.page.has_more);
    assert_eq!(r.result, vec![1, 2, 3]);

    let r = PageResult::new(3, vec![1, 2, 3]);
    assert!(!r.page.has_more, "new() defaults has_more=false (documented footgun)");
  }
}
