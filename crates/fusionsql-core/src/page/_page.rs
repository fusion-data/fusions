use serde::{Deserialize, Serialize};

pub use super::*;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
#[cfg_attr(target_arch = "wasm32", derive(tsify::Tsify), tsify(into_wasm_abi, from_wasm_abi))]
pub struct Page {
  /// 指定返回的页码
  pub page: Option<u64>,
  /// 指定返回的条数
  pub limit: Option<u64>,
  /// 指定返回的偏移量
  pub offset: Option<u64>,
  /// 指定返回的排序
  pub order_bys: Option<OrderBys>,
}

impl Page {
  pub fn new_with_limit(limit: u64) -> Self {
    Self { limit: Some(limit), ..Default::default() }
  }

  pub fn new_with_offset_limit(offset: u64, limit: u64) -> Self {
    Self { limit: Some(limit), offset: Some(offset), ..Default::default() }
  }

  /// 1-indexed page-based pagination — 最常见的前端 cursor 形态（page=1, limit=20）。
  /// 与 [`Self::new_with_offset_limit`] 互斥；同时设置时 [`Self::get_offset`] 优先用 `offset`。
  pub fn new_with_page(page: u64, limit: u64) -> Self {
    Self { page: Some(page), limit: Some(limit), ..Default::default() }
  }

  pub fn new_with_order_bys(order_bys: impl Into<OrderBys>) -> Self {
    Self { order_bys: Some(order_bys.into()), ..Default::default() }
  }

  /// 计算 OFFSET。`page` 为 1-indexed；`page=0` 用 `saturating_sub` 防 underflow
  /// （否则 release 下 wrap 成 `u64::MAX` → OFFSET 巨大值返空集，前端误判
  /// "无数据"）。
  pub fn get_offset(&self) -> Option<u64> {
    self.offset.or_else(|| {
      self.page.map(|page| {
        let limit = self.limit.unwrap_or(0);
        page.saturating_sub(1).saturating_mul(limit)
      })
    })
  }
}

impl From<OrderBys> for Page {
  fn from(val: OrderBys) -> Self {
    Self { order_bys: Some(val), ..Default::default() }
  }
}

impl From<OrderBys> for Option<Page> {
  fn from(val: OrderBys) -> Self {
    Some(Page { order_bys: Some(val), ..Default::default() })
  }
}

impl From<OrderBy> for Page {
  fn from(val: OrderBy) -> Self {
    Self { order_bys: Some(OrderBys::from(val)), ..Default::default() }
  }
}

impl From<OrderBy> for Option<Page> {
  fn from(val: OrderBy) -> Self {
    Some(Page { order_bys: Some(OrderBys::from(val)), ..Default::default() })
  }
}
