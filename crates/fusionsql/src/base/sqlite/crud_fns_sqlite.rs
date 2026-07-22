use sea_query::{Condition, Query, SelectStatement, SqliteQueryBuilder};
use sea_query_binder::SqlxBinder;
use sqlx::{FromRow, sqlite::SqliteRow};

use fusionsql_core::filter::{FilterGroups, apply_to_sea_query};
use fusionsql_core::page::{Page, PageResult};

use crate::{
  ModelContext, ModelManager, Result, SqlError,
  base::{DbBmc, compute_page, count},
  field::HasSeaFields,
  id::Id,
  store::Dbx,
};

pub async fn sqlite_find_first<C, MC, E, F>(mm: &ModelManager<C>, filter: F) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
  E: HasSeaFields,
  F: Into<FilterGroups>,
{
  let list = sqlite_find_many::<C, MC, E, F>(mm, filter, None).await?;
  Ok(list.into_iter().next())
}

pub async fn sqlite_find_many<C, MC, E, F>(mm: &ModelManager<C>, filter: F, page: Option<Page>) -> Result<Vec<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
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

  // page（ORDER BY 列名默认按实体列集合校验，显式 allowlist 优先）
  let page = compute_page(bmc_config, page, E::field_names())?;
  apply_to_sea_query(&page, &mut query);

  // -- Execute the query
  match mm.dbx() {
    Dbx::Sqlite(dbx) => {
      let (sql, values) = query.build_sqlx(SqliteQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let entities = dbx.fetch_all(sqlx_query).await?;
      Ok(entities)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need sqlite database")),
  }
}

pub async fn sqlite_find_many_on<C, MC, E, F>(mm: &ModelManager<C>, f: F) -> Result<Vec<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
  E: HasSeaFields,
  F: FnOnce(&mut SelectStatement) -> Result<()>,
{
  let bmc_config = MC::_bmc_config();

  // -- Build the query
  let mut query = Query::select();
  query.from(bmc_config.table_ref()).columns(E::sea_column_refs());

  // condition from filter and list options
  f(&mut query)?;

  // -- Execute the query
  match mm.dbx() {
    Dbx::Sqlite(dbx) => {
      let (sql, values) = query.build_sqlx(SqliteQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let entities = dbx.fetch_all(sqlx_query).await?;
      Ok(entities)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need sqlite database")),
  }
}

pub async fn sqlite_find_by_id<C, MC, E>(mm: &ModelManager<C>, id: Id) -> Result<E>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
  E: HasSeaFields,
{
  let res = sqlite_get_by_id::<C, MC, E>(mm, id.clone()).await?;
  let bmc_config = MC::_bmc_config();
  match res {
    Some(entity) => Ok(entity),
    None => Err(SqlError::EntityNotFound { schema: bmc_config.schema, entity: bmc_config.table, id }),
  }
}

pub async fn sqlite_get_filter<C, MC, F, E>(mm: &ModelManager<C>, filter: F) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  F: Into<FilterGroups>,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
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

  // -- Execute the query
  match mm.dbx() {
    Dbx::Sqlite(dbx) => {
      let (sql, values) = query.build_sqlx(SqliteQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let res = dbx.fetch_optional(sqlx_query).await?;
      Ok(res)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need sqlite database")),
  }
}

pub async fn sqlite_get_by_id<C, MC, E>(mm: &ModelManager<C>, id: Id) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
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

  // -- Execute the query
  match mm.dbx() {
    Dbx::Sqlite(dbx) => {
      let (sql, values) = query.build_sqlx(SqliteQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let res = dbx.fetch_optional(sqlx_query).await?;
      Ok(res)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need sqlite database")),
  }
}

pub async fn sqlite_find_unique<C, MC, E, F>(mm: &ModelManager<C>, filter: F) -> Result<Option<E>>
where
  C: ModelContext,
  MC: DbBmc,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
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

  // -- Execute the query
  match mm.dbx() {
    Dbx::Sqlite(dbx) => {
      let (sql, values) = query.build_sqlx(SqliteQueryBuilder);
      let sqlx_query = sqlx::query_as_with::<_, E, _>(&sql, values);
      let entity = dbx.fetch_optional(sqlx_query).await?;

      Ok(entity)
    }
    #[allow(unreachable_patterns)]
    _ => Err(SqlError::InvalidDatabase("Need sqlite database")),
  }
}

pub async fn sqlite_page<C, MC, E, F>(mm: &ModelManager<C>, filter: F, page: Page) -> Result<PageResult<E>>
where
  C: ModelContext,
  MC: DbBmc,
  F: Into<FilterGroups>,
  E: for<'r> FromRow<'r, SqliteRow> + Unpin + Send,
  E: HasSeaFields,
{
  let bmc_config = MC::_bmc_config();
  let filter: FilterGroups = mm.apply_filter_interceptor(bmc_config, filter.into())?;
  let total_size = count::<C, MC, _>(mm, filter.clone()).await?;
  let items = sqlite_find_many::<C, MC, E, _>(mm, filter, Some(page)).await?;

  Ok(PageResult::new(total_size, items))
}
