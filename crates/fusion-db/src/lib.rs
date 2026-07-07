use std::{marker::PhantomData, sync::Arc};

use ::config::{File, FileFormat};
use fusion_common::ctx::Ctx;
use fusion_core::{application::ApplicationBuilder, async_trait, configuration::ConfigRegistry, plugin::Plugin};
pub use fusionsql::{DbConfig, ModelContext};
pub type ModelManager = fusionsql::DefaultModelManager;

pub const DEFAULT_CONFIG_STR: &str = include_str!("../resources/default.toml");

pub struct DbPlugin;

#[async_trait]
impl Plugin for DbPlugin {
  async fn build(&self, app: &mut ApplicationBuilder) {
    // sqlx::any::install_default_drivers();
    app.prepend_config_source(File::from_str(DEFAULT_CONFIG_STR, FileFormat::Toml));
    let config: DbConfig = app
      .get_config_by_path("fusion.db")
      .expect("DbPlugin config load failed, please check the config file: `fusion.db`");
    let mm = fusionsql::DefaultModelManager::new(&config, Some(app.get_fusion_config().app().name()))
      .await
      .expect("ModelManager init failed")
      .with_ctx(Ctx::new_super_admin());
    app.add_component(mm);
  }
}

pub struct TypedDbPlugin<C> {
  ctx_factory: Arc<dyn Fn() -> C + Send + Sync>,
  _ctx: PhantomData<C>,
}

impl<C> TypedDbPlugin<C>
where
  C: ModelContext,
{
  pub fn new<F>(ctx_factory: F) -> Self
  where
    F: Fn() -> C + Send + Sync + 'static,
  {
    Self { ctx_factory: Arc::new(ctx_factory), _ctx: PhantomData }
  }
}

#[async_trait]
impl<C> Plugin for TypedDbPlugin<C>
where
  C: ModelContext,
{
  async fn build(&self, app: &mut ApplicationBuilder) {
    app.prepend_config_source(File::from_str(DEFAULT_CONFIG_STR, FileFormat::Toml));
    let config: DbConfig = app
      .get_config_by_path("fusion.db")
      .expect("TypedDbPlugin config load failed, please check the config file: `fusion.db`");
    let mm = fusionsql::ModelManager::<C>::new(&config, Some(app.get_fusion_config().app().name()))
      .await
      .expect("ModelManager init failed")
      .with_ctx((self.ctx_factory)());
    app.add_component(mm);
  }
}
