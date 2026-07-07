use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
  #[error("Field value is not a sea-query Value")]
  FieldValueNotSeaValue,

  #[error("Failed to convert field value into target type. field: '{field_name}'")]
  FieldValueIntoTypeError { field_name: String },
}
