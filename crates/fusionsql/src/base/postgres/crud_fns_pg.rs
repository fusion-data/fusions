use sea_query::{Condition, Query, SelectStatement};
use sea_query_binder::SqlxBinder;
use sqlx::{FromRow, postgres::PgRow};

use fusionsql_core::filter::{FilterGroups, apply_to_sea_query};
use fusionsql_core::page::Page;

use crate::{
  ModelContext, ModelManager, Result, SqlError,
  base::{DbBmc, compute_page, count, fill_select_statement},
  field::HasSeaFields,
  id::Id,
  page::PageResult,
  store::Dbx,
};

pub async fn pg_find_first<C, MC, E, F>(mm: &ModelManager<C>, filter: F) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
  F: Into<FilterGroups>,
{
  let list = pg_find_many::<C, MC, E, F>(mm, filter, None).await?;
  Ok(list.into_iter().next())
}

pub async fn pg_find_many<C, MC, E, F>(mm: &ModelManager<C>, filter: F, page: Option<Page>) -> Result<Vec<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
  F: Into<FilterGroups>,
{
  let bmc_config = MC::_bmc_config();

  // -- Build the query
  let mut query = Query::select();
  query.from(bmc_config.table_ref()).columns(E::sea_column_refs());

  // condition from filter
  let filters: FilterGroups = mm.apply_filter_interceptor(bmc_config, filter.into())?;
  let cond: Condition = filters.try_into()?;
  query.cond_where(cond);
  fill_select_statement(bmc_config, &mut query);

  // page（ORDER BY 列名默认按实体列集合校验，显式 allowlist 优先）
  let page = compute_page(bmc_config, page, E::field_names())?;
  apply_to_sea_query(&page, &mut query);

  // -- Execute the query
  match mm.dbx() {
    Dbx::Postgres(dbx_postgres) => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let entities = dbx_postgres.fetch_all(sqlx_query).await?;
      Ok(entities)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need postgres database")),
  }
}

pub async fn pg_find_many_on<C, MC, E, F>(mm: &ModelManager<C>, f: F) -> Result<Vec<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
  F: FnOnce(&mut SelectStatement) -> Result<()>,
{
  let bmc_config = MC::_bmc_config();

  // -- Build the query
  let mut query = Query::select();
  query.from(bmc_config.table_ref()).columns(E::sea_column_refs());

  // condition from filter and list options
  f(&mut query)?;
  fill_select_statement(bmc_config, &mut query);

  // -- Execute the query
  match mm.dbx() {
    Dbx::Postgres(dbx_postgres) => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let entities = dbx_postgres.fetch_all(sqlx_query).await?;
      Ok(entities)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need postgres database")),
  }
}

pub async fn pg_find_by_id<C, MC, E>(mm: &ModelManager<C>, id: Id) -> Result<E>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
{
  let res = pg_get_by_id::<C, MC, E>(mm, id.clone()).await?;
  let bmc_config = MC::_bmc_config();
  match res {
    Some(entity) => Ok(entity),
    None => Err(SqlError::EntityNotFound { schema: bmc_config.schema, entity: bmc_config.table, id }),
  }
}

pub async fn pg_get_filter<C, MC, F, E>(mm: &ModelManager<C>, filter: F) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  F: Into<FilterGroups>,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
{
  let bmc_config = MC::_bmc_config();

  // -- Build the query
  let mut query = Query::select();
  query.from(bmc_config.table_ref()).columns(E::sea_column_refs());

  // condition from filter
  let filters: FilterGroups = mm.apply_filter_interceptor(bmc_config, filter.into())?;
  let cond: Condition = filters.try_into()?;
  query.cond_where(cond);
  fill_select_statement(bmc_config, &mut query);

  // -- Execute the query
  match mm.dbx() {
    Dbx::Postgres(dbx_postgres) => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let res = dbx_postgres.fetch_optional(sqlx_query).await?;
      Ok(res)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need postgres database")),
  }
}

pub async fn pg_get_by_id<C, MC, E>(mm: &ModelManager<C>, id: Id) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
{
  let bmc_config = MC::_bmc_config();

  // -- Build the query
  let mut query = Query::select();
  query.from(bmc_config.table_ref()).columns(E::sea_column_refs());

  // condition from filter
  let filters: FilterGroups = id.to_filter_node(bmc_config.column_id).into();
  let cond: Condition = filters.try_into()?;
  query.cond_where(cond);
  fill_select_statement(bmc_config, &mut query);

  // -- Execute the query
  match mm.dbx() {
    Dbx::Postgres(dbx_postgres) => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let res = dbx_postgres.fetch_optional(sqlx_query).await?;
      Ok(res)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need postgres database")),
  }
}

pub async fn pg_find_unique<C, MC, E, F>(mm: &ModelManager<C>, filter: F) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
  F: Into<FilterGroups>,
{
  let bmc_config = MC::_bmc_config();

  // -- Build the query
  let mut query = Query::select();
  query.from(bmc_config.table_ref()).columns(E::sea_column_refs());

  // condition from filter
  let filters: FilterGroups = mm.apply_filter_interceptor(bmc_config, filter.into())?;
  let cond: Condition = filters.try_into()?;
  query.cond_where(cond);
  fill_select_statement(bmc_config, &mut query);

  // -- Execute the query
  match mm.dbx() {
    Dbx::Postgres(dbx_postgres) => {
      let (sql, values) = query.build_sqlx(sea_query::PostgresQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let entity = dbx_postgres.fetch_optional(sqlx_query).await?;

      Ok(entity)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need postgres database")),
  }
}

pub async fn pg_page<C, MC, E, F>(mm: &ModelManager<C>, filter: F, page: Page) -> Result<PageResult<E>>
where
  C: ModelContext,
  MC: DbBmc,
  F: Into<FilterGroups>,
  E: for<'r> FromRow<'r, PgRow> + Unpin + Send,
  E: HasSeaFields,
{
  let bmc_config = MC::_bmc_config();
  let filter: FilterGroups = mm.apply_filter_interceptor(bmc_config, filter.into())?;
  let total = count::<C, MC, _>(mm, filter.clone()).await?;
  // 计算 has_more：当前页起始偏移 + 本页返回条数 < 总数 → 仍有后续数据。
  let offset = page.get_offset().unwrap_or(0);
  let result = pg_find_many::<C, MC, E, _>(mm, filter, Some(page)).await?;
  let has_more = offset.saturating_add(result.len() as u64) < total;

  Ok(PageResult::new(total, result).with_has_more(has_more))
}
