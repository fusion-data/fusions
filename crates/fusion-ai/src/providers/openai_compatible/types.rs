//! openai_compatible 本地类型层（fusion-ai-de-rig.md §4.2）。
//!
//! fork 自 rig 同名类型（`OneOrMany` / `Message` / content 家族 / `ToolChoice` /
//! `Usage` 等），演进自主：这里是 provider 无关的内部消息模型，`completion/`
//! 与 `responses_api/` 的 wire 类型在其上映射转换。fork 基线 rig-core 0.39。

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::fmt;
use std::marker::PhantomData;
use std::str::FromStr;

// ================================================================
// OneOrMany
// ================================================================

/// 至少含一个元素的容器：`first` 恒存在，`rest` 可为空。
/// 不能用空向量构造，序列化为 JSON 序组（同 `Vec<T>`）。
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct OneOrMany<T> {
  first: T,
  rest: Vec<T>,
}

/// 用空向量构造 `OneOrMany` 的错误。
#[derive(Debug, thiserror::Error)]
#[error("Cannot create OneOrMany with an empty vector.")]
pub struct EmptyListError;

impl<T: Clone> OneOrMany<T> {
  pub fn first(&self) -> T {
    self.first.clone()
  }

  pub fn first_ref(&self) -> &T {
    &self.first
  }

  pub fn first_mut(&mut self) -> &mut T {
    &mut self.first
  }

  pub fn last(&self) -> T {
    self.rest.last().cloned().unwrap_or_else(|| self.first.clone())
  }

  pub fn last_ref(&self) -> &T {
    self.rest.last().unwrap_or(&self.first)
  }

  pub fn push(&mut self, item: T) {
    self.rest.push(item);
  }

  pub fn len(&self) -> usize {
    1 + self.rest.len()
  }

  /// 恒为 false：`OneOrMany` 无法构造为空。
  pub fn is_empty(&self) -> bool {
    false
  }

  pub fn one(item: T) -> Self {
    OneOrMany { first: item, rest: vec![] }
  }

  pub fn many<I>(items: I) -> Result<Self, EmptyListError>
  where
    I: IntoIterator<Item = T>,
  {
    let mut iter = items.into_iter();
    Ok(OneOrMany {
      first: match iter.next() {
        Some(item) => item,
        None => return Err(EmptyListError),
      },
      rest: iter.collect(),
    })
  }

  pub fn iter(&self) -> Iter<'_, T> {
    Iter { first: Some(&self.first), rest: self.rest.iter() }
  }
}

/// `OneOrMany::iter()` 的返回类型。
pub struct Iter<'a, T> {
  first: Option<&'a T>,
  rest: std::slice::Iter<'a, T>,
}

impl<'a, T> Iterator for Iter<'a, T> {
  type Item = &'a T;

  fn next(&mut self) -> Option<Self::Item> {
    if let Some(first) = self.first.take() {
      Some(first)
    } else {
      self.rest.next()
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    (0, Some(1 + self.rest.len()))
  }
}

/// `OneOrMany` 的拥有型迭代器。
pub struct IntoIter<T> {
  first: Option<T>,
  rest: std::vec::IntoIter<T>,
}

impl<T> IntoIterator for OneOrMany<T>
where
  T: Clone,
{
  type Item = T;
  type IntoIter = IntoIter<T>;

  fn into_iter(self) -> Self::IntoIter {
    IntoIter { first: Some(self.first), rest: self.rest.into_iter() }
  }
}

impl<T> Iterator for IntoIter<T>
where
  T: Clone,
{
  type Item = T;

  fn next(&mut self) -> Option<Self::Item> {
    match self.first.take() {
      Some(first) => Some(first),
      _ => self.rest.next(),
    }
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    (0, Some(1 + self.rest.len()))
  }
}

impl<T> Serialize for OneOrMany<T>
where
  T: Serialize + Clone,
{
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    let mut seq = serializer.serialize_seq(Some(self.len()))?;
    for e in self.iter() {
      seq.serialize_element(e)?;
    }
    seq.end()
  }
}

impl<'de, T> Deserialize<'de> for OneOrMany<T>
where
  T: Deserialize<'de> + Clone,
{
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    struct OneOrManyVisitor<T>(PhantomData<T>);

    impl<'de, T> Visitor<'de> for OneOrManyVisitor<T>
    where
      T: Deserialize<'de> + Clone,
    {
      type Value = OneOrMany<T>;

      fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a sequence of at least one element")
      }

      fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
      where
        A: SeqAccess<'de>,
      {
        let first = seq.next_element()?.ok_or_else(|| de::Error::invalid_length(0, &self))?;
        let mut rest = Vec::new();
        while let Some(value) = seq.next_element()? {
          rest.push(value);
        }
        Ok(OneOrMany { first, rest })
      }
    }

    deserializer.deserialize_any(OneOrManyVisitor(PhantomData))
  }
}

/// 允许字段从「单字符串」或「元素数组」反序列化为 `OneOrMany<T>`。
///
/// 用法：`#[serde(deserialize_with = "string_or_one_or_many")]`
pub fn string_or_one_or_many<'de, T, D>(deserializer: D) -> Result<OneOrMany<T>, D::Error>
where
  T: Deserialize<'de> + FromStr<Err = Infallible> + Clone,
  D: Deserializer<'de>,
{
  struct StringOrOneOrMany<T>(PhantomData<fn() -> T>);

  impl<'de, T> Visitor<'de> for StringOrOneOrMany<T>
  where
    T: Deserialize<'de> + FromStr<Err = Infallible> + Clone,
  {
    type Value = OneOrMany<T>;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
      formatter.write_str("a string or sequence")
    }

    fn visit_str<E>(self, value: &str) -> Result<OneOrMany<T>, E>
    where
      E: de::Error,
    {
      let item = FromStr::from_str(value).map_err(de::Error::custom)?;
      Ok(OneOrMany::one(item))
    }

    fn visit_seq<A>(self, seq: A) -> Result<OneOrMany<T>, A::Error>
    where
      A: SeqAccess<'de>,
    {
      Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
    }

    fn visit_map<M>(self, map: M) -> Result<OneOrMany<T>, M::Error>
    where
      M: MapAccess<'de>,
    {
      let item = Deserialize::deserialize(de::value::MapAccessDeserializer::new(map))?;
      Ok(OneOrMany::one(item))
    }
  }

  deserializer.deserialize_any(StringOrOneOrMany(PhantomData))
}

// ================================================================
// 消息模型
// ================================================================

/// provider 无关的消息模型：provider 方言（chat completions / responses）在其上映射。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
  System { content: String },

  User { content: OneOrMany<UserContent> },

  Assistant {
    /// provider 侧 assistant 消息 ID（Responses 回放需要）
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    content: OneOrMany<AssistantContent>,
  },
}

impl Message {
  pub fn system(text: impl Into<String>) -> Self {
    Self::System { content: text.into() }
  }

  pub fn user(text: impl Into<String>) -> Self {
    Self::User { content: OneOrMany::one(UserContent::Text(Text::new(text))) }
  }

  pub fn assistant(text: impl Into<String>) -> Self {
    Self::Assistant { id: None, content: OneOrMany::one(AssistantContent::Text(Text::new(text))) }
  }

  pub fn assistant_with_id(id: String, text: impl Into<String>) -> Self {
    Self::Assistant {
      id: Some(id),
      content: OneOrMany::one(AssistantContent::Text(Text::new(text))),
    }
  }
}

/// user 侧内容。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum UserContent {
  Text(Text),
  ToolResult(ToolResult),
  Image(Image),
  Audio(Audio),
  Document(Document),
}

impl UserContent {
  pub fn text(text: impl Into<String>) -> Self {
    Self::Text(Text::new(text))
  }

  pub fn image_url(url: impl Into<String>, media_type: Option<ImageMediaType>, detail: Option<ImageDetail>) -> Self {
    Self::Image(Image {
      data: DocumentSourceKind::Url(url.into()),
      media_type,
      detail,
      additional_params: None,
    })
  }

  pub fn audio(data: impl Into<String>, media_type: Option<AudioMediaType>) -> Self {
    Self::Audio(Audio { data: DocumentSourceKind::Base64(data.into()), media_type, additional_params: None })
  }
}

/// assistant 侧内容。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(untagged)]
pub enum AssistantContent {
  Text(Text),
  ToolCall(ToolCall),
  Reasoning(Reasoning),
  Image(Image),
}

impl AssistantContent {
  pub fn text(text: impl Into<String>) -> Self {
    Self::Text(Text::new(text))
  }
}

/// 文本内容。
#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Text {
  pub text: String,
  #[serde(flatten, skip_serializing_if = "Option::is_none")]
  pub additional_params: Option<serde_json::Value>,
}

impl Text {
  pub fn new(text: impl Into<String>) -> Self {
    Self { text: text.into(), additional_params: None }
  }
}

/// 图片内容。
#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Image {
  pub data: DocumentSourceKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub media_type: Option<ImageMediaType>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub detail: Option<ImageDetail>,
  #[serde(flatten, skip_serializing_if = "Option::is_none")]
  pub additional_params: Option<serde_json::Value>,
}

/// 音频内容。
#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Audio {
  pub data: DocumentSourceKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub media_type: Option<AudioMediaType>,
  #[serde(flatten, skip_serializing_if = "Option::is_none")]
  pub additional_params: Option<serde_json::Value>,
}

/// 文档内容。
#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Document {
  pub data: DocumentSourceKind,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub media_type: Option<DocumentMediaType>,
  #[serde(flatten, skip_serializing_if = "Option::is_none")]
  pub additional_params: Option<serde_json::Value>,
}

/// 内容来源：URL / base64 / 原始字节 / 纯字符串。
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(tag = "type", content = "value", rename_all = "camelCase")]
#[non_exhaustive]
pub enum DocumentSourceKind {
  Url(String),
  Base64(String),
  Raw(Vec<u8>),
  String(String),
  #[default]
  Unknown,
}

impl std::fmt::Display for DocumentSourceKind {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Url(string) | Self::Base64(string) | Self::String(string) => write!(f, "{string}"),
      Self::Raw(_) => write!(f, "<binary data>"),
      Self::Unknown => write!(f, "<unknown>"),
    }
  }
}

/// 工具调用结果。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolResult {
  pub id: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub call_id: Option<String>,
  pub content: OneOrMany<ToolResultContent>,
}

/// 工具调用结果内容。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolResultContent {
  Text(Text),
  Image(Image),
}

impl ToolResultContent {
  pub fn text(text: impl Into<String>) -> Self {
    Self::Text(Text::new(text))
  }
}

/// assistant 发起的工具调用。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolCall {
  pub id: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub call_id: Option<String>,
  pub function: ToolFunction,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub signature: Option<String>,
  #[serde(skip_serializing_if = "Option::is_none")]
  pub additional_params: Option<serde_json::Value>,
}

impl ToolCall {
  pub fn new(id: String, function: ToolFunction) -> Self {
    Self { id, call_id: None, function, signature: None, additional_params: None }
  }

  pub fn with_call_id(mut self, call_id: String) -> Self {
    self.call_id = Some(call_id);
    self
  }
}

/// 工具函数名与参数。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolFunction {
  pub name: String,
  pub arguments: serde_json::Value,
}

/// assistant 推理块。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Reasoning {
  pub id: Option<String>,
  pub content: Vec<ReasoningContent>,
}

impl Reasoning {
  pub fn new(input: &str) -> Self {
    Self { id: None, content: vec![ReasoningContent::Text { text: input.to_string(), signature: None }] }
  }

  pub fn with_id(mut self, id: String) -> Self {
    self.id = Some(id);
    self
  }
}

/// 推理块内容形态。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", content = "content", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReasoningContent {
  Text {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
  },
  Encrypted(String),
  Redacted { data: String },
  Summary(String),
}

impl ReasoningContent {
  /// 提取可读文本（Text / Encrypted / Redacted / Summary 一律降级为字符串）。
  pub fn text(&self) -> String {
    match self {
      Self::Text { text, .. } => text.clone(),
      Self::Encrypted(s) => s.clone(),
      Self::Redacted { data } => data.clone(),
      Self::Summary(s) => s.clone(),
    }
  }
}

// ================================================================
// 媒体类型
// ================================================================

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ImageMediaType {
  JPEG,
  PNG,
  GIF,
  WEBP,
  HEIC,
  HEIF,
  SVG,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DocumentMediaType {
  PDF,
  TXT,
  RTF,
  HTML,
  CSS,
  MARKDOWN,
  CSV,
  XML,
  Javascript,
  Python,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AudioMediaType {
  WAV,
  MP3,
  AIFF,
  AAC,
  OGG,
  FLAC,
  M4A,
  PCM16,
  PCM24,
}

impl ImageMediaType {
  pub fn to_mime_type(&self) -> &'static str {
    match self {
      Self::JPEG => "image/jpeg",
      Self::PNG => "image/png",
      Self::GIF => "image/gif",
      Self::WEBP => "image/webp",
      Self::HEIC => "image/heic",
      Self::HEIF => "image/heif",
      Self::SVG => "image/svg+xml",
    }
  }
}

impl DocumentMediaType {
  pub fn to_mime_type(&self) -> &'static str {
    match self {
      Self::PDF => "application/pdf",
      Self::TXT => "text/plain",
      Self::RTF => "application/rtf",
      Self::HTML => "text/html",
      Self::CSS => "text/css",
      Self::MARKDOWN => "text/markdown",
      Self::CSV => "text/csv",
      Self::XML => "application/xml",
      Self::Javascript => "application/javascript",
      Self::Python => "text/x-python",
    }
  }
}

/// 图片细节偏好（OpenAI 方言：low / high / auto）。
#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
  Low,
  High,
  #[default]
  Auto,
}

// ================================================================
// 工具与用量
// ================================================================

/// provider 无关的工具选择。
#[derive(Default, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
  #[default]
  Auto,
  None,
  Required,
  Specific { function_names: Vec<String> },
}

/// provider 无关的工具定义。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct ToolDefinition {
  pub name: String,
  pub parameters: serde_json::Value,
  pub description: String,
}

/// provider 无关的 token 用量。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Default)]
pub struct Usage {
  pub input_tokens: u64,
  pub output_tokens: u64,
  pub total_tokens: u64,
  pub cached_input_tokens: u64,
  pub cache_creation_input_tokens: u64,
}

// ================================================================
// 错误
// ================================================================

/// 消息方言转换错误。
#[derive(Debug, thiserror::Error)]
#[error("Message conversion error: {0}")]
pub struct MessageError(String);

impl MessageError {
  pub fn conversion(msg: impl Into<String>) -> Self {
    Self(msg.into())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn one_or_many_roundtrip() {
    let single = OneOrMany::one(1);
    assert_eq!(single.len(), 1);
    assert_eq!(single.first(), 1);

    let many = OneOrMany::many(vec![1, 2, 3]).unwrap();
    assert_eq!(many.len(), 3);
    assert_eq!(many.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3]);

    assert!(OneOrMany::<u32>::many(vec![]).is_err());
  }

  #[test]
  fn string_or_one_or_many_accepts_string_and_array() {
    #[derive(Deserialize)]
    struct Holder {
      #[serde(deserialize_with = "string_or_one_or_many")]
      content: OneOrMany<String>,
    }

    let from_str: Holder = serde_json::from_str(r#"{"content": "hello"}"#).unwrap();
    assert_eq!(from_str.content.first(), "hello");

    let from_arr: Holder = serde_json::from_str(r#"{"content": ["a", "b"]}"#).unwrap();
    assert_eq!(from_arr.content.len(), 2);
  }

  #[test]
  fn reasoning_content_text_extracts_all_variants() {
    assert_eq!(ReasoningContent::Text { text: "t".into(), signature: None }.text(), "t");
    assert_eq!(ReasoningContent::Encrypted("e".into()).text(), "e");
    assert_eq!(ReasoningContent::Summary("s".into()).text(), "s");
  }
}
