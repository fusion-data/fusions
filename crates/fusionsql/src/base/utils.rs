use fusionsql_core::page::Page;
use sea_query::{DeleteStatement, Expr, InsertStatement, IntoIden, SelectStatement, UpdateStatement, WithQuery};
#[cfg(any(feature = "with-postgres", feature = "with-sqlite"))]
use sea_query_binder::{SqlxBinder, SqlxValues};

use crate::{
  ModelContext, Result, SqlError,
  base::{BmcConfig, CommonIden, TimestampIden},
  field::{SeaField, SeaFields},
  store::dbx::DbxProvider,
};

pub fn build_sqlx_for_update(dbx_type: &DbxProvider, query: UpdateStatement) -> (String, SqlxValues) {
  match dbx_type {
    #[cfg(feature = "with-postgres")]
    DbxProvider::Postgres => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      (sql, values)
    }
    #[cfg(feature = "with-sqlite")]
    DbxProvider::Sqlite => {
      let (sql, values) = query.build_sqlx(sea_query::SqliteQueryBuilder);
      (sql, values)
    }
  }
}

pub fn build_sqlx_for_select(dbx_type: &DbxProvider, query: SelectStatement) -> (String, SqlxValues) {
  match dbx_type {
    #[cfg(feature = "with-postgres")]
    DbxProvider::Postgres => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      (sql, values)
    }
    #[cfg(feature = "with-sqlite")]
    DbxProvider::Sqlite => {
      let (sql, values) = query.build_sqlx(sea_query::SqliteQueryBuilder);
      (sql, values)
    }
  }
}

pub fn build_sqlx_for_query(dbx_type: &DbxProvider, query: WithQuery) -> (String, SqlxValues) {
  match dbx_type {
    #[cfg(feature = "with-postgres")]
    DbxProvider::Postgres => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      (sql, values)
    }
    #[cfg(feature = "with-sqlite")]
    DbxProvider::Sqlite => {
      let (sql, values) = query.build_sqlx(sea_query::SqliteQueryBuilder);
      (sql, values)
    }
  }
}

pub fn build_sqlx_for_delete(dbx_type: &DbxProvider, query: DeleteStatement) -> (String, SqlxValues) {
  match dbx_type {
    #[cfg(feature = "with-postgres")]
    DbxProvider::Postgres => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      (sql, values)
    }
    #[cfg(feature = "with-sqlite")]
    DbxProvider::Sqlite => {
      let (sql, values) = query.build_sqlx(sea_query::SqliteQueryBuilder);
      (sql, values)
    }
  }
}

pub fn build_sqlx_for_insert(dbx_type: &DbxProvider, query: InsertStatement) -> (String, SqlxValues) {
  match dbx_type {
    #[cfg(feature = "with-postgres")]
    DbxProvider::Postgres => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      (sql, values)
    }
    #[cfg(feature = "with-sqlite")]
    DbxProvider::Sqlite => {
      let (sql, values) = query.build_sqlx(sea_query::SqliteQueryBuilder);
      (sql, values)
    }
  }
}

/// This method must be called when a model controller intends to create its entity.
pub fn prep_fields_for_create<C>(bmc_config: &BmcConfig, mut fields: SeaFields, ctx: &C) -> SeaFields
where
  C: ModelContext,
{
  fill_creations(bmc_config, &mut fields, ctx);
  if bmc_config.id_generated_by_db {
    let id_iden = bmc_config.column_id.into_iden();
    fields = SeaFields::new(fields.into_iter().filter(|f| f.iden != id_iden).collect());
  }

  fields
}

/// This method must be called when a Model Controller plans to update its entity.
pub fn prep_fields_for_update<C>(bmc_config: &BmcConfig, mut fields: SeaFields, ctx: &C) -> SeaFields
where
  C: ModelContext,
{
  fill_modifications(bmc_config, &mut fields, ctx);
  fields
}

pub fn clear_id_from_fields(bmc_config: &BmcConfig, fields: SeaFields) -> SeaFields {
  let mut fields = fields.into_vec();
  fields.retain(|f| f.iden != bmc_config.column_id.into_iden());
  SeaFields::new(fields)
}

/// Update the creations info for create
/// (e.g., created_by, created_at, and updated_by, updated_at will be updated with the same values)
fn fill_creations<C>(bmc_config: &BmcConfig, fields: &mut SeaFields, ctx: &C)
where
  C: ModelContext,
{
  if bmc_config.has_owner_id {
    fields.push(SeaField::new(CommonIden::OwnerId.into_iden(), ctx.audit_user_id()));
  }
  if bmc_config.has_created_by && !fields.exists(TimestampIden::CreatedBy.into_iden()) {
    fields.push(SeaField::new(TimestampIden::CreatedBy, ctx.audit_user_id()));
  }
  if bmc_config.has_created_at && !fields.exists(TimestampIden::CreatedAt.into_iden()) {
    fields.push(SeaField::new(TimestampIden::CreatedAt, ctx.req_time()));
  }
  if bmc_config.has_updated_by && !fields.exists(TimestampIden::UpdatedBy.into_iden()) {
    fields.push(SeaField::new(TimestampIden::UpdatedBy, ctx.audit_user_id()));
  }
  if bmc_config.has_updated_at && !fields.exists(TimestampIden::UpdatedAt.into_iden()) {
    fields.push(SeaField::new(TimestampIden::UpdatedAt, ctx.req_time()));
  }
}

/// Update the modifications info only for update.
/// (.e.g., only updated_by, updated_at will be updated)
fn fill_modifications<C>(bmc_config: &BmcConfig, fields: &mut SeaFields, ctx: &C)
where
  C: ModelContext,
{
  if bmc_config.has_updated_by && !fields.exists(TimestampIden::UpdatedBy.into_iden()) {
    fields.push(SeaField::new(TimestampIden::UpdatedBy, ctx.audit_user_id()));
  }
  if bmc_config.has_updated_at && !fields.exists(TimestampIden::UpdatedAt.into_iden()) {
    fields.push(SeaField::new(TimestampIden::UpdatedAt, ctx.req_time()));
  }
}

/// 归一化分页参数（limit 上下限 / 默认排序），并校验客户端提交的 ORDER BY 列名。
///
/// `entity_columns` 为实体列集合（调用方传 `E::field_names()`），是无显式
/// allowlist 时的默认排序名单 —— 详见 [`validate_order_bys`]。
pub fn compute_page(
  bmc_config: &BmcConfig,
  page: Option<Page>,
  entity_columns: &'static [&'static str],
) -> Result<Page> {
  if let Some(mut page) = page {
    // Validate the limit.
    if let Some(limit) = page.limit {
      if limit > bmc_config.list_limit_max {
        return Err(SqlError::ListLimitOverMax { max: bmc_config.list_limit_max, actual: limit });
      } else if limit < 1 {
        return Err(SqlError::ListLimitUnderMin { min: 1, actual: limit });
      }
    } else {
      // Set the default limit if no limit
      page.limit = Some(bmc_config.list_limit_default);
    }
    if let Some(page) = page.page
      && page < 1
    {
      return Err(SqlError::ListPageUnderMin { min: 1, actual: page });
    }
    // 客户端提交的 ORDER BY 是不可信输入 —— 在回落到服务端默认排序
    // （`bmc_config.order_bys`，受信代码配置）之前校验。
    validate_order_bys(bmc_config, page.order_bys.as_ref(), entity_columns)?;
    if page.order_bys.is_none() || page.order_bys.iter().any(|o| o.is_empty()) {
      page.order_bys = bmc_config.order_bys.as_ref().map(Into::into);
    }
    Ok(page)
  } else {
    // When None, return default
    Ok(Page {
      limit: Some(bmc_config.list_limit_default),
      order_bys: bmc_config.order_bys.as_ref().map(Into::into),
      ..Default::default()
    })
  }
}

/// 校验客户端提交的分页 `ORDER BY` 列名（不可参数化的标识符，是注入 /
/// 排序侧信道 / schema 探测的防线）。
///
/// 名单判定（opt-out 安全默认）：
/// 1. BMC 显式配置了 [`BmcConfig::with_order_by_allowlist`] → 以显式名单为准
///    （用于比实体列集合更收紧的场景，或显式放开 join / 计算列）；
/// 2. 否则回落到实体列集合 `entity_columns`（`HasFields::field_names()`）——
///    默认只允许按实体自身的列排序，挡住按响应中不存在的敏感列排序的
///    ORDER BY oracle、按任意列名探测 schema、以及按无索引隐藏列的慢排序。
///
/// 服务端默认排序（`BmcConfig.order_bys`）是受信代码配置，不经过本校验
/// （合法场景可按 join / 计算列排序）。`OrderBy` 列名先剥离 `!` 降序前缀再
/// 比对；任一列不在名单内返回 [`SqlError::InvalidArgument`]。
fn validate_order_bys(
  bmc_config: &BmcConfig,
  order_bys: Option<&fusionsql_core::page::OrderBys>,
  entity_columns: &'static [&'static str],
) -> Result<()> {
  let Some(order_bys) = order_bys else {
    return Ok(());
  };
  let allowed = bmc_config.order_by_allowlist.unwrap_or(entity_columns);
  for order_by in order_bys {
    let (col, _desc) = order_by.parse();
    if !allowed.contains(&col) {
      return Err(SqlError::InvalidArgument {
        message: format!(
          "Illegal ORDER BY column '{}' for table '{}'. Allowed columns: {:?}",
          col, bmc_config.table, allowed
        ),
      });
    }
  }
  Ok(())
}

/// 检查 sql execute 语句后受影响的数量
pub fn check_number_of_affected(bmc_config: &BmcConfig, expect_n: usize, return_n: u64) -> Result<u64> {
  // -- Check result
  if return_n as usize != expect_n {
    Err(SqlError::EntityNotFound {
      schema: bmc_config.schema,
      entity: bmc_config.table,
      id: 0.into(), // Using 0 because multiple IDs could be not found, you may want to improve error handling here
    })
  } else {
    Ok(return_n)
  }
}

pub fn fill_update_statement(bmc_config: &BmcConfig, stmt: &mut UpdateStatement) {
  if bmc_config.use_logical_deletion {
    stmt.and_where(Expr::col(CommonIden::LogicalDeletion).is_null());
  }
}

pub fn fill_select_statement(bmc_config: &BmcConfig, stmt: &mut SelectStatement) {
  if bmc_config.use_logical_deletion {
    stmt.and_where(Expr::col(CommonIden::LogicalDeletion).is_null());
  }
}

pub fn fill_delete_statement(bmc_config: &BmcConfig, stmt: &mut DeleteStatement) {
  if bmc_config.use_logical_deletion {
    stmt.and_where(Expr::col(CommonIden::LogicalDeletion).is_null());
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use fusionsql_core::page::{OrderBy, OrderBys};

  const ENTITY_COLUMNS: &[&str] = &["id", "name", "status"];

  fn page_order_by(col: &str) -> Option<Page> {
    Some(Page { order_bys: Some(OrderBys::from(OrderBy::from(col.to_string()))), ..Default::default() })
  }

  #[test]
  fn order_by_defaults_to_entity_columns() {
    // opt-out 安全默认：无显式 allowlist 时按实体列集合校验
    let config = BmcConfig::new_table("user");
    assert!(compute_page(&config, page_order_by("name"), ENTITY_COLUMNS).is_ok());
    // `!` 降序前缀剥离后比对
    assert!(compute_page(&config, page_order_by("!status"), ENTITY_COLUMNS).is_ok());

    // 表里存在但实体未映射的隐藏列（如 password_hash）→ 拒绝（ORDER BY oracle 防线）
    let err = compute_page(&config, page_order_by("password_hash"), ENTITY_COLUMNS).unwrap_err();
    assert!(matches!(err, SqlError::InvalidArgument { .. }), "unexpected: {err:?}");
  }

  #[test]
  fn explicit_allowlist_overrides_entity_columns() {
    // 显式 allowlist 比实体列集合更收紧：name 在实体列内但不在名单内 → 拒绝
    let config = BmcConfig::new_table("user").with_order_by_allowlist(&["id"]);
    assert!(compute_page(&config, page_order_by("id"), ENTITY_COLUMNS).is_ok());
    let err = compute_page(&config, page_order_by("name"), ENTITY_COLUMNS).unwrap_err();
    assert!(matches!(err, SqlError::InvalidArgument { .. }), "unexpected: {err:?}");
  }

  #[test]
  fn server_default_order_bys_is_trusted_and_not_validated() {
    // 服务端默认排序是受信配置，可按实体列集合之外的列（join / 计算列）排序
    let config =
      BmcConfig::new_table("user").with_order_bys(Some(fusionsql_core::page::StaticOrderBys(&["!joined_col"])));

    // 客户端未提供 order_bys → 回落服务端默认，不校验
    let page = compute_page(&config, Some(Page::default()), ENTITY_COLUMNS).unwrap();
    assert!(page.order_bys.is_some());
    // page 参数为 None 的默认路径同理
    let page = compute_page(&config, None, ENTITY_COLUMNS).unwrap();
    assert!(page.order_bys.is_some());
  }

  #[test]
  fn client_empty_order_bys_falls_back_without_error() {
    let config = BmcConfig::new_table("user");
    let page = Some(Page { order_bys: Some(OrderBys::new(Vec::new())), ..Default::default() });
    assert!(compute_page(&config, page, ENTITY_COLUMNS).is_ok());
  }
}
