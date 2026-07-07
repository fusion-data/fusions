use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// 能用包装结果，将可 Serialize 的类型包裹在 `data` 字段中
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
pub struct WrapperResult<T> {
  pub data: T,
}

impl<T> WrapperResult<T> {
  pub fn new(data: T) -> Self {
    Self { data }
  }
}

impl<T: Serialize> From<T> for WrapperResult<T> {
  fn from(data: T) -> Self {
    Self::new(data)
  }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
pub struct IdResult {
  pub id: serde_json::Value,
}

impl IdResult {
  /// 从可序列化的 id 构造。**Panics** when `serde_json::to_value(id)` 失败
  /// （含 `f64::NAN` 或自定义 impl Serialize 出错）。生产代码用奇怪类型时
  /// 优先用 [`Self::try_new`] 显式处理错误。
  ///
  /// # Panics
  /// 当 `id` 序列化失败时。
  pub fn new<T>(id: T) -> Self
  where
    T: Serialize,
  {
    Self::try_new(id).expect("IdResult::new: serde_json::to_value failed; use try_new for fallible API")
  }

  /// Fallible counterpart of [`Self::new`]: returns `Err` on serialization failure
  /// instead of panicking.
  pub fn try_new<T>(id: T) -> Result<Self, serde_json::Error>
  where
    T: Serialize,
  {
    Ok(Self { id: serde_json::to_value(id)? })
  }

  pub fn to<T>(&self) -> Result<T, serde_json::Error>
  where
    T: DeserializeOwned,
  {
    serde_json::from_value(self.id.clone())
  }

  #[cfg(feature = "with-uuid")]
  pub fn to_uuid(&self) -> Result<uuid::Uuid, serde_json::Error> {
    self.to::<uuid::Uuid>()
  }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
pub struct IdI64Result {
  pub id: i64,
}
impl IdI64Result {
  pub fn new(id: i64) -> Self {
    Self { id }
  }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
pub struct IdStringResult {
  pub id: String,
}
impl IdStringResult {
  pub fn new(id: String) -> Self {
    Self { id }
  }
}

#[cfg(feature = "with-uuid")]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
pub struct IdUuidResult {
  pub id: uuid::Uuid,
}

#[cfg(feature = "with-uuid")]
impl IdUuidResult {
  pub fn new(id: uuid::Uuid) -> Self {
    Self { id }
  }
}

#[cfg(feature = "with-uuid")]
impl From<uuid::Uuid> for IdUuidResult {
  fn from(id: uuid::Uuid) -> Self {
    Self::new(id)
  }
}

#[cfg(feature = "with-ulid")]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "with-openapi", derive(utoipa::ToSchema))]
pub struct IdUlidResult {
  pub id: ulid::Ulid,
}

#[cfg(feature = "with-ulid")]
impl IdUlidResult {
  pub fn new(id: ulid::Ulid) -> Self {
    Self { id }
  }
}
