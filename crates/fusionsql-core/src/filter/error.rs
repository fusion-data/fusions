//! Main Crate Error

use serde_json::Value;
use thiserror::Error;

/// fusionsql Result
pub type Result<T> = core::result::Result<T, Error>;

/// fusionsql Error
#[derive(Debug, Error)]
pub enum Error {
  // region:    --- Json Errors
  #[error("JSON value is not of expected type: {0}")]
  JsonValNotOfType(&'static str),

  #[error("JSON array element has an unexpected type")]
  JsonValArrayWrongType { actual_value: Value },

  #[error("JSON array item is not of expected type '{expected_type}'")]
  JsonValArrayItemNotOfType { expected_type: &'static str, actual_value: Value },

  #[error("JSON operator '{operator}' is not supported")]
  JsonOpValNotSupported { operator: String, value: Value },
  // endregion: --- Json Errors
}
