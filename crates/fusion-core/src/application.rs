use std::{
  any::Any,
  fmt::Display,
  future::Future,
  sync::{Arc, OnceLock},
};

use config::Config;
use dashmap::DashMap;
use fusion_common::ahash::HashSet;
use fusion_common::time::OffsetDateTime;
use log::{debug, error, info};
use mea::{
  mutex::Mutex,
  shutdown::{ShutdownRecv, ShutdownSend},
};
use serde::de::DeserializeOwned;

use crate::{
  Result,
  component::{ComponentArc, ComponentError, ComponentResult, DynComponentArc, auto_inject_component},
  configuration::{ConfigRegistry, Configurable, ConfigureResult, FusionConfigRegistry, FusionSetting},
  plugin::{Plugin, PluginRef},
};

type Registry<T> = DashMap<String, T>;
// `Box<Task<T>>` is stored in `ApplicationBuilder.shutdown_hooks`; the closure
// itself must be `Send` for the builder to be `Send + Sync` without a hand-rolled
// `unsafe impl`. (Future created by the closure is already `+ Send` below.)
type Task<T> = dyn FnOnce(Application) -> Box<dyn Future<Output = Result<T>> + Send> + Send;

pub(crate) struct ApplicationInner {
  config_registry: FusionConfigRegistry,
  components: Registry<DynComponentArc>,
  start_time: OffsetDateTime,
  pub(crate) shutdown: Mutex<Option<(ShutdownSend, ShutdownRecv)>>,
  /// Hooks registered via [`ApplicationBuilder::add_shutdown_hook`]; drained and
  /// executed (in registration order) by [`Application::await_shutdown`].
  shutdown_hooks: Mutex<Vec<Box<Task<String>>>>,
}

/// Application, clone is cheap.
#[derive(Clone)]
pub struct Application(pub(crate) Arc<ApplicationInner>);

impl Display for Application {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "Application({}|{})", self.fusion_setting().app().name(), self.0.start_time)
  }
}

/// 手写 Debug：让下游 `#[derive(Debug)]` 包装 `Application` 的常见写法可编译。
/// 只打印应用名 / 启动时间 / 组件数，不递归 dump 组件与配置内容。
impl std::fmt::Debug for Application {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Application")
      .field("name", &self.fusion_setting().app().name())
      .field("start_time", &self.0.start_time)
      .field("components", &self.0.components.len())
      .finish_non_exhaustive()
  }
}

impl Application {
  pub fn builder() -> ApplicationBuilder {
    ApplicationBuilder::default()
  }

  pub fn global() -> Application {
    GLOBAL_APPLICATION.get().expect("Application is not initialized").clone()
  }

  /// Returns the global application if initialized, or `None` otherwise.
  /// Use this in best-effort code paths where the application may not be set up
  /// (e.g. integration tests without a full DI container).
  pub fn try_global() -> Option<Application> {
    GLOBAL_APPLICATION.get().cloned()
  }

  pub fn set_global(application: Application) {
    match GLOBAL_APPLICATION.set(application) {
      Ok(_) => (),
      Err(old) => {
        panic!("Global application was already set to {}", old)
      }
    }
  }

  pub async fn shutdown() {
    let app = Self::global();
    if let Some((shutdown_tx, _)) = app.0.shutdown.lock().await.as_ref() {
      shutdown_tx.shutdown();
    }
  }

  pub async fn await_shutdown() -> bool {
    let app = Self::global();
    if let Some((shutdown_tx, shutdown_rx)) = app.0.shutdown.lock().await.take() {
      drop(shutdown_rx);
      shutdown_tx.await_shutdown().await;
      app.run_shutdown_hooks().await;
      true
    } else {
      false
    }
  }

  /// Drain and run the shutdown hooks registered on the builder, in
  /// registration order. A failing hook is logged and does not prevent the
  /// remaining hooks from running.
  async fn run_shutdown_hooks(&self) {
    let hooks = std::mem::take(&mut *self.0.shutdown_hooks.lock().await);
    for hook in hooks {
      match Box::into_pin(hook(self.clone())).await {
        Ok(name) => info!("shutdown hook completed: {name}"),
        Err(e) => error!("shutdown hook failed: {e}"),
      }
    }
  }

  pub async fn is_shutdown(&self) -> bool {
    self.0.shutdown.lock().await.is_none()
  }

  /// Returns a clone of the shutdown receiver, or `None` when the shutdown
  /// pair has already been taken (e.g. by [`Self::await_shutdown`]).
  pub async fn shutdown_recv(&self) -> Option<ShutdownRecv> {
    let maybe = self.0.shutdown.lock().await;
    maybe.as_ref().map(|tuple| tuple.1.clone())
  }

  pub fn config_registry(&self) -> &FusionConfigRegistry {
    &self.0.config_registry
  }

  // -- Component access -----------------------------------------------------
  // Two return shapes, each with a panicking / fallible pair:
  //   cloned `T`:            `component`     / `try_component`
  //   shared `ComponentArc`: `component_arc` / `try_component_arc`
  // Panicking variants are for startup wiring where a missing component is a
  // programming error; `try_*` variants are for optional lookups.

  /// Get a shared reference to the component of the specified type.
  /// Fallible counterpart of [`Self::component_arc`].
  #[inline]
  pub fn try_component_arc<T>(&self) -> ComponentResult<ComponentArc<T>>
  where
    T: Any + Send + Sync,
  {
    self.try_component_arc_by_name(std::any::type_name::<T>())
  }

  #[deprecated(since = "0.2.0", note = "renamed to `try_component_arc`")]
  #[inline]
  pub fn get_component_arc<T>(&self) -> ComponentResult<ComponentArc<T>>
  where
    T: Any + Send + Sync,
  {
    self.try_component_arc()
  }

  /// Get a shared reference to the component of the specified type.
  /// **Panics** when the component is missing or of a mismatched type — use
  /// [`Self::try_component_arc`] for fallible lookup.
  #[track_caller]
  pub fn component_arc<T>(&self) -> ComponentArc<T>
  where
    T: Any + Send + Sync,
  {
    match self.try_component_arc() {
      Ok(c) => c,
      Err(e) => panic!("{e}"),
    }
  }

  /// Get a shared reference to the component registered under `component_name`
  /// (the component's full type path). Fallible.
  pub fn try_component_arc_by_name<T>(&self, component_name: &str) -> ComponentResult<ComponentArc<T>>
  where
    T: Any + Send + Sync,
  {
    let pair = match self.0.components.get(component_name) {
      Some(pair) => pair,
      None => return Err(ComponentError::ComponentNotFound(component_name.to_string())),
    };
    let component_ref = pair.value().clone();
    component_ref.downcast::<T>()
  }

  #[deprecated(since = "0.2.0", note = "renamed to `try_component_arc_by_name`")]
  pub fn get_component_ref_by_name<T>(&self, component_name: &str) -> ComponentResult<ComponentArc<T>>
  where
    T: Any + Send + Sync,
  {
    self.try_component_arc_by_name(component_name)
  }

  /// Get a clone of the component of the specified type. **Panics** when the
  /// component is missing or of a mismatched type — use [`Self::try_component`]
  /// for fallible lookup.
  #[track_caller]
  pub fn component<T>(&self) -> T
  where
    T: Clone + Send + Sync + 'static,
  {
    match self.try_component() {
      Ok(c) => c,
      Err(e) => panic!("{e}"),
    }
  }

  /// Get a clone of the component of the specified type.
  /// Fallible counterpart of [`Self::component`].
  pub fn try_component<T>(&self) -> ComponentResult<T>
  where
    T: Clone + Send + Sync + 'static,
  {
    self.try_component_arc().map(|c| T::clone(&c))
  }

  #[deprecated(since = "0.2.0", note = "renamed to `try_component`")]
  pub fn get_component<T>(&self) -> ComponentResult<T>
  where
    T: Clone + Send + Sync + 'static,
  {
    self.try_component()
  }

  /// Get all built components. The return value is the full crate path of all components
  pub fn component_names(&self) -> Vec<String> {
    self.0.components.iter().map(|e| e.key().clone()).collect()
  }

  #[deprecated(since = "0.2.0", note = "renamed to `component_names`")]
  pub fn get_component_names(&self) -> Vec<String> {
    self.component_names()
  }

  /// Register a component as a long-lived singleton. **Panics** when `T` is
  /// already registered — use [`Self::try_add_component`] for fallible
  /// registration (e.g. dynamic plugin reload, idempotent test setup).
  #[track_caller]
  pub fn add_component<T>(&self, component: T)
  where
    T: Clone + Any + Send + Sync,
  {
    if let Err(e) = self.try_add_component(component) {
      panic!("Error adding component: {e}");
    }
  }

  /// Fallible counterpart of [`Self::add_component`]. Returns
  /// [`ComponentError`] when `T` is already registered, leaving the existing
  /// instance untouched.
  pub fn try_add_component<T>(&self, component: T) -> ComponentResult<()>
  where
    T: Clone + Any + Send + Sync,
  {
    let component_name = std::any::type_name::<T>();
    if self.0.components.contains_key(component_name) {
      return Err(ComponentError::AlreadyRegistered(component_name.to_string()));
    }
    self.0.components.insert(component_name.to_string(), DynComponentArc::new(component));
    debug!("added component: {}", component_name);
    Ok(())
  }

  pub fn fusion_setting(&self) -> Arc<FusionSetting> {
    self.0.config_registry.fusion_setting()
  }

  /// Get `::config::Config` Instance
  pub fn underlying_config(&self) -> Arc<Config> {
    self.0.config_registry.config()
  }

  pub fn start_time(&self) -> &OffsetDateTime {
    &self.0.start_time
  }
}

impl ConfigRegistry for Application {
  fn get_config<T>(&self) -> ConfigureResult<T>
  where
    T: DeserializeOwned + Configurable,
  {
    self.0.config_registry.get_config()
  }

  fn get_config_by_path<T>(&self, path: &str) -> ConfigureResult<T>
  where
    T: DeserializeOwned,
  {
    self.0.config_registry.get_config_by_path(path)
  }
}

static GLOBAL_APPLICATION: OnceLock<Application> = OnceLock::new();

#[derive(Default)]
pub struct ApplicationBuilder {
  config_registry: FusionConfigRegistry,

  /// Plugins
  pub(crate) plugin_registry: Registry<PluginRef>,

  /// Components
  pub(crate) components: Registry<DynComponentArc>,

  /// Tasks
  shutdown_hooks: Vec<Box<Task<String>>>,
}

// Note: `ApplicationBuilder` is automatically `Send + Sync` via its fields
// (`FusionConfigRegistry` / `Registry<...>` / `Vec<Box<Task<...>>>` are all
// already `Send + Sync`). The hand-rolled `unsafe impl` here previously masked
// any future non-`Send` field by overriding the auto-trait inference; removed
// so the compiler keeps enforcing thread-safety on every field addition.

impl ApplicationBuilder {
  pub fn get_fusion_config(&self) -> Arc<FusionSetting> {
    self.config_registry.fusion_setting()
  }

  pub fn with_config_registry(&mut self, config_registry: FusionConfigRegistry) -> &mut Self {
    self.config_registry = config_registry;
    self
  }

  /// Append a config source. **Panics** when the source cannot be merged — use
  /// [`Self::try_add_config_source`] for fallible registration.
  #[track_caller]
  pub fn add_config_source<T>(&mut self, source: T) -> &mut Self
  where
    T: config::Source + Send + Sync + 'static,
  {
    if let Err(e) = self.try_add_config_source(source) {
      panic!("Add config source failed: {e}");
    }
    self
  }

  /// Fallible counterpart of [`Self::add_config_source`].
  pub fn try_add_config_source<T>(&mut self, source: T) -> ConfigureResult<&mut Self>
  where
    T: config::Source + Send + Sync + 'static,
  {
    self.config_registry.append_config_source(source)?;
    Ok(self)
  }

  /// Prepend a config source so it only fills in missing keys (does not override user config).
  /// **Panics** when the source cannot be merged — use
  /// [`Self::try_prepend_config_source`] for fallible registration.
  #[track_caller]
  pub fn prepend_config_source<T>(&mut self, source: T) -> &mut Self
  where
    T: config::Source + Send + Sync + 'static,
  {
    if let Err(e) = self.try_prepend_config_source(source) {
      panic!("Prepend config source failed: {e}");
    }
    self
  }

  /// Fallible counterpart of [`Self::prepend_config_source`].
  pub fn try_prepend_config_source<T>(&mut self, source: T) -> ConfigureResult<&mut Self>
  where
    T: config::Source + Send + Sync + 'static,
  {
    self.config_registry.prepend_config_source(source)?;
    Ok(self)
  }

  /// add plugin
  pub fn add_plugin<T: Plugin>(&mut self, plugin: T) -> &mut Self {
    let plugin_name = plugin.name().to_string();
    debug!("added plugin: {plugin_name}");

    if plugin.immediately() {
      plugin.immediately_build(self);
      return self;
    }
    if self.plugin_registry.contains_key(plugin.name()) {
      panic!("Error adding plugin {plugin_name}: plugin was already added in application")
    }
    self.plugin_registry.insert(plugin_name, PluginRef::new(plugin));
    self
  }

  /// Returns `true` if the [`Plugin`] has already been added.
  #[inline]
  pub fn contains_plugin<T: Plugin>(&self) -> bool {
    self.plugin_registry.contains_key(std::any::type_name::<T>())
  }

  /// Add component to the registry. **Panics** when `T` is already registered;
  /// use [`Self::try_add_component`] for fallible registration.
  #[track_caller]
  pub fn add_component<T>(&mut self, component: T) -> &mut Self
  where
    T: Clone + Any + Send + Sync,
  {
    if let Err(e) = self.try_add_component(component) {
      panic!("Error adding component to builder: {e}");
    }
    self
  }

  /// Fallible counterpart of [`Self::add_component`]. Returns the original
  /// builder via `&mut Self` on success; returns [`ComponentError::AlreadyRegistered`]
  /// when `T` is already registered, leaving the existing instance untouched.
  pub fn try_add_component<T>(&mut self, component: T) -> ComponentResult<&mut Self>
  where
    T: Clone + Any + Send + Sync,
  {
    let component_name = std::any::type_name::<T>();
    if self.components.contains_key(component_name) {
      return Err(ComponentError::AlreadyRegistered(component_name.to_string()));
    }
    self.components.insert(component_name.to_string(), DynComponentArc::new(component));
    debug!("added component: {}", component_name);
    Ok(self)
  }

  /// Get a shared reference to the component of the specified type. Fallible.
  pub fn try_component_arc<T>(&self) -> ComponentResult<ComponentArc<T>>
  where
    T: Any + Send + Sync,
  {
    let component_name = std::any::type_name::<T>();
    let pair = match self.components.get(component_name) {
      Some(pair) => pair,
      None => return Err(ComponentError::ComponentNotFound(component_name.to_string())),
    };
    let component_ref = pair.value().clone();
    component_ref.downcast::<T>()
  }

  #[deprecated(since = "0.2.0", note = "renamed to `try_component_arc`")]
  pub fn get_component_ref<T>(&self) -> ComponentResult<ComponentArc<T>>
  where
    T: Any + Send + Sync,
  {
    self.try_component_arc()
  }

  /// Get a clone of the component of the specified type. **Panics** when the
  /// component is missing — use [`Self::try_component`] for fallible lookup.
  #[track_caller]
  pub fn component<T>(&self) -> T
  where
    T: Clone + Send + Sync + 'static,
  {
    match self.try_component() {
      Ok(c) => c,
      Err(e) => panic!("{e}"),
    }
  }

  /// Get a clone of the component of the specified type.
  /// Fallible counterpart of [`Self::component`].
  pub fn try_component<T>(&self) -> ComponentResult<T>
  where
    T: Clone + Send + Sync + 'static,
  {
    self.try_component_arc().map(|c| T::clone(&c))
  }

  #[deprecated(since = "0.2.0", note = "renamed to `try_component`")]
  pub fn get_component<T>(&self) -> ComponentResult<T>
  where
    T: Clone + Send + Sync + 'static,
  {
    self.try_component()
  }

  /// Add a shutdown hook. Hooks run in registration order inside
  /// [`Application::await_shutdown`], after the shutdown signal has been
  /// processed by all subsystems. If the process never calls
  /// `await_shutdown`, hooks do not run.
  pub fn add_shutdown_hook<T>(&mut self, hook: T) -> &mut Self
  where
    T: FnOnce(Application) -> Box<dyn Future<Output = Result<String>> + Send> + Send + 'static,
  {
    self.shutdown_hooks.push(Box::new(hook));
    self
  }

  /// The `run` method is suitable for applications that contain scheduling logic,
  /// such as web, job, and stream.
  ///
  pub async fn run(&mut self) -> Result<Application> {
    self.inner_run().await
  }

  async fn inner_run(&mut self) -> Result<Application> {
    let app = self.build().await?;

    // 4. schedule
    // self.schedule().await

    Ok(app)
  }

  /// Unlike the [`run`] method, the `build` method is suitable for applications that do not contain scheduling logic.
  /// This method returns the built Application, and developers can implement logic such as command lines and task scheduling by themselves.
  pub async fn build(&mut self) -> Result<Application> {
    // 0. load toml config
    // self.load_config_if_need()?;

    // 1. build plugin
    self.build_plugins().await?;

    // 2. service dependency inject
    auto_inject_component(self)?;

    // 3. build application
    let application = self.build_application();

    Application::set_global(application);
    Ok(Application::global())
  }

  /// Initialize tracing for Application
  async fn build_plugins(&mut self) -> Result<()> {
    let registry = std::mem::take(&mut self.plugin_registry);
    let mut to_register = registry.iter().map(|e| e.value().to_owned()).collect::<Vec<_>>();
    let mut registered: HashSet<String> = HashSet::default();

    while !to_register.is_empty() {
      let mut progress = false;
      let mut next_round = vec![];

      for plugin in to_register {
        let deps = plugin.dependencies();
        if deps.iter().all(|dep| registered.contains(*dep)) {
          plugin.build(self).await;
          registered.insert(plugin.name().to_string());
          info!("{} plugin registered", plugin.name());
          progress = true;
        } else {
          next_round.push(plugin);
        }
      }

      if !progress {
        panic!("Cyclic dependency detected or missing dependencies for some plugins");
      }

      to_register = next_round;
    }

    self.plugin_registry = registry;
    Ok(())
  }

  fn build_application(&mut self) -> Application {
    let components = std::mem::take(&mut self.components);
    let configuration_state = std::mem::take(&mut self.config_registry);
    let shutdown_hooks = std::mem::take(&mut self.shutdown_hooks);
    let init_time = configuration_state.fusion_setting().app().time_now();
    let shutdown = Mutex::new(Some(mea::shutdown::new_pair()));
    Application(Arc::new(ApplicationInner {
      config_registry: configuration_state,
      components,
      start_time: init_time,
      shutdown,
      shutdown_hooks: Mutex::new(shutdown_hooks),
    }))
  }
}

impl ConfigRegistry for ApplicationBuilder {
  fn get_config<T>(&self) -> ConfigureResult<T>
  where
    T: DeserializeOwned + Configurable,
  {
    self.config_registry.get_config::<T>()
  }

  fn get_config_by_path<T>(&self, path: &str) -> ConfigureResult<T>
  where
    T: DeserializeOwned,
  {
    self.config_registry.get_config_by_path::<T>(path)
  }
}
#[cfg(test)]
mod tests {
  use fusion_common::env::remove_envs;

  use super::*;

  #[tokio::test]
  async fn test_application_run() {
    // process-wide env 串行化：与 test_config_load / jose::helper 共享锁，
    // 避免 remove_envs / Application::builder() 内的 load_config 与并发 env 读写撞车。
    // 锁作用域限定到同步部分：`Application::builder()` 在 default 路径里同步调用
    // `FusionConfigRegistry::default() → load_config_with()` 读 env；`.run()` 返回的
    // Future 此时 env 快照已固定，可以放到 .await 之前 drop guard，避免 clippy::await_holding_lock。
    let mut builder = {
      let _guard = crate::configuration::test_env_lock().lock().unwrap();
      remove_envs(&[
        "FUSION__APP__NAME",
        "FUSION__WEB__SERVER_ADDR",
        "FUSION__SECURITY__TOKEN__SECRET_KEY",
        "FUSION__SECURITY__PWD__PWD_KEY",
      ])
      .unwrap();
      Application::builder()
    };
    builder.run().await.unwrap();
    let app = Application::global();
    assert_eq!(app.fusion_setting().app().name(), "fusion");
  }
}
