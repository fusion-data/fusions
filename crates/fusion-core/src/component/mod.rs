#![doc = include_str!("../../DI.md")]
mod error;

#[cfg(feature = "with-macros")]
pub use fusion_core_macros::Component;
pub use inventory::submit;
use log::debug;
use std::{any::Any, collections::HashSet, ops::Deref, sync::Arc};

use crate::application::ApplicationBuilder;

pub use error::{ComponentError, ComponentResult};

/// Component's dyn trait reference
#[derive(Debug, Clone)]
pub struct DynComponentArc(Arc<dyn Any + Send + Sync>);

impl DynComponentArc {
  /// constructor
  pub fn new<T>(component: T) -> Self
  where
    T: Any + Send + Sync,
  {
    Self(Arc::new(component))
  }

  /// Downcast to the specified type
  pub fn downcast<T>(self) -> ComponentResult<ComponentArc<T>>
  where
    T: Any + Send + Sync,
  {
    match self.0.downcast::<T>() {
      Ok(item) => Ok(ComponentArc::new(item)),
      Err(_) => Err(ComponentError::ComponentTypeMismatch(std::any::type_name::<T>())),
    }
  }
}

/// A component reference of a specified type
#[derive(Debug, Clone)]
pub struct ComponentArc<T>(Arc<T>);

impl<T> ComponentArc<T> {
  fn new(target_ref: Arc<T>) -> Self {
    Self(target_ref)
  }
}

impl<T> Deref for ComponentArc<T> {
  type Target = T;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

pub trait Component: Clone + Sized + 'static {
  /// Construct the Component
  fn build(app: &ApplicationBuilder) -> crate::Result<Self>;
}

pub trait ComponentInstaller: Send + Sync + 'static {
  /// Get the dependencies of the Component
  fn dependencies(&self) -> Vec<&str>;

  /// Install the Component into the Application
  fn install_component(&self, app: &mut ApplicationBuilder) -> crate::Result<()>;
}

inventory::collect!(&'static dyn ComponentInstaller);

/// Register a [`ComponentInstaller`] with the inventory so
/// [`auto_inject_component`] discovers it. `$crate` keeps the expansion
/// resolvable no matter how the consumer names or re-exports this crate.
#[macro_export]
macro_rules! submit_component {
  ($ty:tt) => {
    $crate::component::submit! {
      &($ty) as &dyn $crate::component::ComponentInstaller
    }
  };
}

/// Find all ComponentInstaller and install them into the application
pub fn auto_inject_component(app_builder: &mut ApplicationBuilder) -> crate::Result<()> {
  let mut registrars: Vec<(&&dyn ComponentInstaller, Vec<&str>)> =
    inventory::iter::<&dyn ComponentInstaller>.into_iter().map(|cr| (cr, cr.dependencies())).collect();

  // 拓扑收敛：每轮至少安装一个组件即视为有进展；若某轮零安装而仍有
  // pending，说明存在循环依赖或缺失依赖，立即 panic（对齐 `build_plugins`）。
  while !registrars.is_empty() {
    let mut progress = false;
    let mut pending_registrars = vec![];
    for (registrar, deps) in registrars {
      let deps: Vec<&str> = deps.into_iter().filter(|d| !app_builder.components.contains_key(*d)).collect();
      if deps.is_empty() {
        registrar.install_component(app_builder)?;
        progress = true;
      } else {
        debug!("Dependency does not exist, waiting for the next round: [{:?}]", deps);
        pending_registrars.push((registrar, deps));
      }
    }
    registrars = pending_registrars;

    if !progress {
      let deps: HashSet<&str> = registrars.iter().flat_map(|(_, deps)| deps).copied().collect();
      panic!(
        "Component registration failed, please check the component dependency relationship. Cyclic or missing dependencies, unregistered Components: {:?}",
        deps
      );
    }
  }
  Ok(())
}
