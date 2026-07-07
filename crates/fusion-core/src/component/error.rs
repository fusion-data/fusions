use thiserror::Error;

pub type ComponentResult<T> = core::result::Result<T, ComponentError>;

#[derive(Debug, Error)]
pub enum ComponentError {
  #[error("Component not found, name is {0}")]
  ComponentNotFound(String),

  #[error("Component type mismatch, type is {0}")]
  ComponentTypeMismatch(&'static str),

  #[error("Component already registered, name is {0}")]
  AlreadyRegistered(String),
}
