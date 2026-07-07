/// Convenience macro rules to generate default CRUD functions for a Bmc/Entity.
/// Note: If custom functionality is required, use the code below as foundational
///       code for the custom implementations.
#[macro_export]
macro_rules! generate_pg_bmc_common {
	(
		Bmc: $struct_name:ident,
		Entity: $entity:ty,
		$(ForCreate: $for_create:ty,)?
		$(ForUpdate: $for_update:ty,)?
		$(ForInsert: $for_insert:ty,)?
	) => {
		impl $struct_name {
			$(
					pub async fn create<C>(
						mm: &fusionsql::ModelManager<C>,
						entity_c: $for_create,
					) -> fusionsql::Result<i64>
					where
						C: fusionsql::ModelContext,
					{
						fusionsql::base::create::<C, Self, _>(mm, entity_c).await
					}

					pub async fn create_many<C>(
						mm: &fusionsql::ModelManager<C>,
						entity_c: Vec<$for_create>,
					) -> fusionsql::Result<Vec<i64>>
					where
						C: fusionsql::ModelContext,
					{
						fusionsql::base::create_many::<C, Self, _>(mm, entity_c).await
					}
			)?

			$(
					pub async fn insert<C>(
						mm: &fusionsql::ModelManager<C>,
						entity_i: $for_insert,
					) -> fusionsql::Result<()>
					where
						C: fusionsql::ModelContext,
					{
						fusionsql::base::insert::<C, Self, _>(mm, entity_i).await
					}

					pub async fn insert_many<C>(
						mm: &fusionsql::ModelManager<C>,
						entity_i: Vec<$for_insert>,
					) -> fusionsql::Result<u64>
					where
						C: fusionsql::ModelContext,
					{
						fusionsql::base::insert_many::<C, Self, _>(mm, entity_i).await
					}
			)?

			pub async fn find_by_id<C>(
				mm: &fusionsql::ModelManager<C>,
				id: impl Into<fusionsql::id::Id>,
			) -> fusionsql::Result<$entity>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::pg_find_by_id::<C, Self, _>(mm, id.into()).await
			}

			pub async fn get_by_id<C>(
				mm: &fusionsql::ModelManager<C>,
				id: impl Into<fusionsql::id::Id>,
			) -> fusionsql::Result<Option<$entity>>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::pg_get_by_id::<C, Self, _>(mm, id.into()).await
			}

			$(
				pub async fn update_by_id<C>(
					mm: &fusionsql::ModelManager<C>,
					id: impl Into<fusionsql::id::Id>,
					entity_u: $for_update,
				) -> fusionsql::Result<()>
				where
					C: fusionsql::ModelContext,
				{
					fusionsql::base::update_by_id::<C, Self, _>(mm, id.into(), entity_u).await
				}
			)?

			pub async fn delete_by_id<C>(
				mm: &fusionsql::ModelManager<C>,
				id: impl Into<fusionsql::id::Id>,
			) -> fusionsql::Result<()>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::delete_by_id::<C, Self>(mm, id.into()).await
			}

			pub async fn delete_by_ids<C, V, I>(
				mm: &fusionsql::ModelManager<C>,
				ids: I,
			) -> fusionsql::Result<u64>
			where
					C: fusionsql::ModelContext,
					V: Into<fusionsql::id::Id>,
					I: IntoIterator<Item = V>,
			{
				let ids = ids.into_iter().map(|v| v.into()).collect();
				fusionsql::base::delete_by_ids::<C, Self>(mm, ids).await
			}
		}
	};
}

#[macro_export]
macro_rules! generate_pg_bmc_filter {
	(
		Bmc: $struct_name:ident,
		Entity: $entity:ty,
		Filter: $filter:ty,
		$(ForUpdate: $update:ty,)?
	) => {
		impl $struct_name {
			pub async fn find_unique<C>(
				mm: &fusionsql::ModelManager<C>,
				filter: Vec<$filter>,
			) -> fusionsql::Result<Option<$entity>>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::pg_find_unique::<C, Self, _, _>(mm, filter).await
			}

			pub async fn find_many<C>(
				mm: &fusionsql::ModelManager<C>,
				filter: Vec<$filter>,
				page: Option<fusionsql::page::Page>,
			) -> fusionsql::Result<Vec<$entity>>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::pg_find_many::<C, Self, _, _>(mm, filter, page).await
			}

			pub async fn count<C>(
				mm: &fusionsql::ModelManager<C>,
				filter: Vec<$filter>,
			) -> fusionsql::Result<u64>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::count::<C, Self, _>(mm, filter).await
			}

			pub async fn page<C>(
				mm: &fusionsql::ModelManager<C>,
				filter: Vec<$filter>,
				page: fusionsql::page::Page,
			) -> fusionsql::Result<fusionsql::page::PageResult<$entity>>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::pg_page::<C, Self, _, _>(mm, filter, page).await
			}

			pub async fn delete<C>(
				mm: &fusionsql::ModelManager<C>,
				filter: Vec<$filter>,
			) -> fusionsql::Result<u64>
			where
				C: fusionsql::ModelContext,
			{
				fusionsql::base::delete::<C, Self, _>(mm, filter).await
			}

			$(
				pub async fn update<C>(
					mm: &fusionsql::ModelManager<C>,
					filter: Vec<$filter>,
					entity_u: $update,
				) -> fusionsql::Result<u64>
				where
					C: fusionsql::ModelContext,
				{
					fusionsql::base::update::<C, Self, _, _>(mm, filter, entity_u).await
				}
			)?
		}
	};
}

#[macro_export]
macro_rules! generate_pg_bmc_filter_x {
  (
		Bmc: $struct_name:ident,
		Entity: $entity:ty,
		Filter: $filter:ty,
		$(ForUpdate: $update:ty,)?
	) => {
    impl $struct_name {
      pub async fn get_filter<C>(
        mm: &fusionsql::ModelManager<C>,
        filter: Vec<$filter>,
      ) -> fusionsql::Result<Option<$filter>>
      where
        C: fusionsql::ModelContext,
      {
        fusionsql::base::pg_get_filter::<C, Self, _, _>(mm, filter).await
      }
    }
  };
}
