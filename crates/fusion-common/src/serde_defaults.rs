//! Serde 默认值 / 跳过序列化 helper。
//!
//! 这些函数用于 `#[serde(default = "...")]` 与 `#[serde(skip_serializing_if = "...")]`。
//! serde 按字段类型对 `default = "path"` 指向的函数做返回类型推断，因此
//! 数值默认值收敛成泛型 [`default_zero`] / [`default_one`]，无需为每种整数类型
//! 各写一份。

/// `#[serde(skip_serializing_if = "is_true")]` —— 值为 `true` 时跳过。
pub fn is_true(b: &bool) -> bool {
  *b
}

/// `#[serde(skip_serializing_if = "is_false")]` —— 值为 `false` 时跳过。
pub fn is_false(b: &bool) -> bool {
  !*b
}

/// `#[serde(default = "default_true")]` —— bool 字段默认 `true`。
pub fn default_true() -> bool {
  true
}

/// `#[serde(default = "default_false")]` —— bool 字段默认 `false`。
///
/// 等价于 `#[serde(default)]`，提供显式命名以便 attr 可读。
pub fn default_false() -> bool {
  false
}

/// `#[serde(default = "default_zero")]` —— 数值字段默认零值。
///
/// 适用于任意实现 [`Default`] 的类型（所有整数 / 浮点的 `Default` 即零值）。
pub fn default_zero<T: Default>() -> T {
  T::default()
}

/// `#[serde(default = "default_one")]` —— 数值字段默认 `1`。
///
/// 适用于任意可由 `u8` 转换得到的类型（所有整数类型均满足 `From<u8>`）。
pub fn default_one<T: From<u8>>() -> T {
  T::from(1)
}
