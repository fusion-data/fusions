use rand::{RngExt, distr::Alphanumeric, rng};
use serde::{Deserializer, Serializer, de::Visitor};

// b64u 编解码的唯一实现位于 `crate::digest`；此处 re-export 以保持
// `fusion_common::string::b64u_*` 调用路径的向后兼容。
pub use crate::digest::{b64u_decode, b64u_decode_to_string, b64u_encode};

pub fn repeat_str(s: &str, n: usize) -> String {
  let mut v = String::with_capacity(s.len() * n);
  for _ in 0..n {
    v.push_str(s);
  }
  v
}

pub fn repeat_char(c: char, n: usize) -> String {
  let mut v = String::with_capacity(c.len_utf8() * n);
  for _ in 0..n {
    v.push(c);
  }
  v
}

pub fn random_string(n: usize) -> String {
  rng().sample_iter(Alphanumeric).take(n).map(char::from).collect()
}

pub fn deser_str_to_vec_u8<'de, D>(d: D) -> core::result::Result<Vec<u8>, D::Error>
where
  D: Deserializer<'de>,
{
  struct StrToVecU8;
  impl Visitor<'_> for StrToVecU8 {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
      formatter.write_str("expect 'str'.")
    }

    fn visit_str<E>(self, v: &str) -> core::result::Result<Self::Value, E>
    where
      E: serde::de::Error,
    {
      Ok(v.as_bytes().into())
    }
  }

  d.deserialize_str(StrToVecU8)
}

pub fn ser_vec_u8_to_str<S>(v: &[u8], s: S) -> core::result::Result<S::Ok, S::Error>
where
  S: Serializer,
{
  let string = std::str::from_utf8(v).map_err(serde::ser::Error::custom)?;
  s.serialize_str(string)
}
