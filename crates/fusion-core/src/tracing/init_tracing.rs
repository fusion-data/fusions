use std::sync::Arc;

use fusion_common::time::{self, FixedOffset};
use init_tracing_opentelemetry::{Guard, TracingConfig};
use log::Level;
use tracing::{Subscriber, debug, info, subscriber::DefaultGuard};
use tracing_subscriber::{
  Registry,
  filter::EnvFilter,
  fmt::{
    self,
    format::{DefaultFields, FmtSpan, Format, Full, Pretty},
    time::FormatTime,
  },
  layer::SubscriberExt,
  registry::LookupSpan,
};

use crate::{
  CoreError, Result,
  configuration::{FusionSetting, LogLevel, LogSetting, LogWriterType},
};

// setup a temporary subscriber to log output during setup
pub(crate) fn init_tracing_guard() -> (DefaultGuard, Option<String>) {
  let c = LogSetting {
    with_target: true,
    log_level: LogLevel(Level::Trace),
    log_writers: vec![LogWriterType::Stdout],
    log_dir: std::option_env!("FUSION__LOG_DIR").unwrap_or_else(|| "./var/logs/").to_string(),
    ..Default::default()
  };
  let fixed_offset = time::local_offset();
  let (layer, original_rust_log) = build_loglevel_filter_layer(&c);
  let subscriber = tracing_subscriber::registry()
    .with(layer)
    .with(stdout_fmt_layer(*fixed_offset, &c))
    .with(file_fmt_layer(&temporary_app_name(), *fixed_offset, &c));

  (::tracing::subscriber::set_default(subscriber), original_rust_log)
}

fn temporary_app_name() -> String {
  std::env::var("FUSION__APP__NAME")
    .or_else(|_| std::env::var("FUSION_APP_NAME"))
    .unwrap_or_else(|_| "fusion".to_string())
}

pub fn init_subscribers(setting: &FusionSetting) -> Result<Option<Guard>> {
  let (_tmp_guard, _) = init_tracing_guard();

  // 桥接 `log` crate（log::info!/warn!/error! 等）到全局 tracing subscriber。
  // 没有这一步，所有走 log crate 的 macro 都会被 tracing-subscriber 丢弃 ——
  // 历史 codepath 与三方 crate 大量使用 log::*!，缺桥接导致错误信息静默。
  // `LogTracer::init()` 全局唯一，这里 `.ok()` 吞重复 init err（如测试套件）。
  let _ = tracing_log::LogTracer::init();

  let c = setting.log();
  info!("init logging & tracing");
  info!("Loaded the LogSetting is:\n{}", toml::to_string(c).unwrap());

  if c.otel().enable {
    unsafe {
      std::env::set_var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", &c.otel().exporter_otlp_endpoint);
      std::env::set_var("OTEL_TRACES_SAMPLER", &c.otel().traces_sample);
      std::env::set_var("OTEL_SERVICE_NAME", setting.app().name());
    }

    let guard = TracingConfig::default()
      .with_log_directives("info,tokio::task=trace,tokio::task::waker=warn")
      .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NONE)
      .init_subscriber_ext(|subscriber| {
        subscriber.with(file_fmt_layer(setting.app().name(), *setting.app().time_offset(), c))
      })
      .map_err(|e| CoreError::tracing(format!("Init tracing & otel failed, error: {}", e)))?;
    Ok(Some(guard))
  } else {
    let subscriber = transform_identity(setting, tracing_subscriber::registry());
    tracing::subscriber::set_global_default(subscriber)
      .map_err(|e| CoreError::tracing(format!("Set global default traceing subscriber failed, error: {}", e)))?;
    Ok(None)
  }
}

fn transform_identity(
  setting: &FusionSetting,
  subscriber: Registry,
) -> impl Subscriber + for<'a> LookupSpan<'a> + Send + Sync {
  let c = setting.log();
  subscriber
    .with(build_loglevel_filter_layer(c).0)
    .with(stdout_fmt_layer(*setting.app().time_offset(), c))
    .with(file_fmt_layer(setting.app().name(), *setting.app().time_offset(), c))
}

#[must_use]
pub fn build_loglevel_filter_layer(c: &LogSetting) -> (EnvFilter, Option<String>) {
  let rust_log = std::env::var("RUST_LOG").or_else(|_| std::env::var("OTEL_LOG_LEVEL"));
  let original_rust_log = rust_log.clone().ok();

  let value = [
    if c.log_targets.is_empty() { None } else { Some(c.log_targets.join(",")) },
    // 仅保留 original_rust_log 里的 target 指令（含 '='）；裸全局级别交给 conf log_level。
    // 见 rust_log_directives 注释：本函数被调用两次，首次临时 init 用硬编码 Trace
    // set_var 污染 RUST_LOG，二次读回裸 "TRACE" 若原样追加 → 末尾全局 TRACE 兜底 → 日志爆炸。
    rust_log_directives(original_rust_log.as_deref()).or_else(|| Some(c.log_level.to_string())),
  ]
  .into_iter()
  .flatten()
  .collect::<Vec<_>>()
  .join(",");

  let log_value = if value.ends_with(',') { &value[..value.len() - 1] } else { value.as_str() };

  debug!("ORIGINAL RUST_LOG: {:?}; NEW RUST_LOG: {}", original_rust_log, log_value);
  unsafe {
    std::env::set_var("RUST_LOG", log_value);
  }
  (EnvFilter::from_default_env(), original_rust_log)
}

/// 从原始 RUST_LOG 字符串提取 target 指令（含 '='，如 `foo=debug`），丢弃裸全局级别
/// （如 `TRACE` / `info`）。
///
/// 背景：`build_loglevel_filter_layer` 被调用两次——`init_tracing_guard`（临时 init，硬编码
/// `log_level=Trace`）经 set_var 把 `RUST_LOG` 写成裸 `"TRACE"`；`transform_identity`（正式
/// init）读回并经本函数追加到 `log_targets` 末尾。若原样追加裸级别，它会成为 EnvFilter
/// 全局兜底，把所有未显式列出的 target（rig / h2 / hyper / reqwest / sqlx …）抬到该级别——
/// rig 在 TRACE 会 `to_string_pretty` dump 完整 LLM prompt（日志爆炸 + 内容泄露），
/// fusion_core::configuration 在 TRACE 会 dump 整套环境变量（密钥泄露）。仅保留 target 指令
/// 即可根治：全局默认级别统一走 conf `log_level`（调用方 or_else），不再被污染的裸级别覆盖。
fn rust_log_directives(original: Option<&str>) -> Option<String> {
  let directives: Vec<&str> = original?.split(',').map(str::trim).filter(|d| d.contains('=')).collect();
  (!directives.is_empty()).then(|| directives.join(","))
}

pub fn stdout_fmt_layer<S>(
  fixed_offset: FixedOffset,
  c: &LogSetting,
) -> Option<fmt::Layer<S, Pretty, Format<Pretty, Chrono>>>
where
  S: Subscriber,
  for<'a> S: LookupSpan<'a>,
{
  if c.log_writers.iter().any(|lw| lw.is_stdout()) {
    let l = _fmt_layer(fixed_offset, c).pretty().with_ansi(true);
    Some(l)
  } else {
    None
  }
}

pub fn file_fmt_layer<S>(
  app_name: &str,
  fixed_offset: FixedOffset,
  c: &LogSetting,
) -> Option<
  fmt::Layer<
    S,
    fmt::format::JsonFields,
    Format<fmt::format::Json, Chrono>,
    tracing_appender::rolling::RollingFileAppender,
  >,
>
where
  S: Subscriber,
  for<'a> S: LookupSpan<'a>,
{
  use std::path::Path;
  if c.log_writers.iter().any(|lw| lw.is_file()) {
    //.with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
    let path = Path::new(&c.log_dir);
    let file_appender = tracing_appender::rolling::daily(path, format!("{}.log", app_name));
    let l = _fmt_layer(fixed_offset, c).json().with_writer(file_appender);
    Some(l)
  } else {
    None
  }
}

fn _fmt_layer<S>(fixed_offset: FixedOffset, c: &LogSetting) -> fmt::Layer<S, DefaultFields, Format<Full, Chrono>>
where
  S: Subscriber,
  for<'a> S: LookupSpan<'a>,
{
  let fmt_span = c.with_span_events.iter().fold(FmtSpan::NONE, |span, s| span | parse_to_fmt_span(s));
  let format = if c.time_format.is_empty() {
    Arc::new(ChronoFmtType::Rfc3339)
  } else {
    Arc::new(ChronoFmtType::Custom(c.time_format.clone()))
  };
  let fmt_time = Chrono { format, fixed_offset };
  fmt::layer::<S>()
    .with_file(c.with_file)
    .with_line_number(c.with_line_number)
    .with_thread_ids(c.with_thread_ids)
    .with_thread_names(c.with_thread_names)
    .with_target(c.with_target)
    .with_span_events(fmt_span)
    .with_timer(fmt_time)
}

fn parse_to_fmt_span(s: &str) -> FmtSpan {
  if "new".eq_ignore_ascii_case(s) {
    FmtSpan::NEW
  } else if "enter".eq_ignore_ascii_case(s) {
    FmtSpan::ENTER
  } else if "exit".eq_ignore_ascii_case(s) {
    FmtSpan::EXIT
  } else if "close".eq_ignore_ascii_case(s) {
    FmtSpan::CLOSE
  } else if "none".eq_ignore_ascii_case(s) {
    FmtSpan::NONE
  } else if "active".eq_ignore_ascii_case(s) {
    FmtSpan::ACTIVE
  } else if "full".eq_ignore_ascii_case(s) {
    FmtSpan::FULL
  } else {
    panic!("Invalid FmtSpan value: {}", s)
  }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Chrono {
  format: Arc<ChronoFmtType>,
  fixed_offset: FixedOffset,
}

impl FormatTime for Chrono {
  fn format_time(&self, w: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
    let t = time::now().with_timezone(&self.fixed_offset);
    match self.format.as_ref() {
      ChronoFmtType::Rfc3339 => w.write_str(&t.to_rfc3339()),
      ChronoFmtType::Custom(fmt) => w.write_str(&format!("{}", t.format(fmt))),
    }
  }
}

#[derive(Debug, Clone, Eq, PartialEq)]

enum ChronoFmtType {
  /// Format according to the RFC 3339 convention.
  Rfc3339,
  /// Format according to a custom format string.
  Custom(String),
}

#[cfg(test)]
mod tests {
  use super::rust_log_directives;

  #[test]
  fn rust_log_directives_keeps_target_directives() {
    // 含 '=' 的 target 指令保留（用户临时调试 override，如 RUST_LOG=foo=debug）
    assert_eq!(rust_log_directives(Some("foo=debug,bar=trace")), Some("foo=debug,bar=trace".to_string()));
  }

  #[test]
  fn rust_log_directives_drops_bare_global_level() {
    // 裸全局级别丢弃——这正是双重调用污染的来源（init_tracing_guard set RUST_LOG=TRACE）
    assert_eq!(rust_log_directives(Some("TRACE")), None);
    assert_eq!(rust_log_directives(Some("info")), None);
  }

  #[test]
  fn rust_log_directives_filters_mixed_to_targets_only() {
    // 混合输入：保留 target 指令，丢弃裸级别
    assert_eq!(rust_log_directives(Some("TRACE,foo=debug")), Some("foo=debug".to_string()));
    assert_eq!(rust_log_directives(Some("foo=debug,info")), Some("foo=debug".to_string()));
  }

  #[test]
  fn rust_log_directives_none_for_empty_or_absent() {
    assert_eq!(rust_log_directives(None), None);
    assert_eq!(rust_log_directives(Some("")), None);
    assert_eq!(rust_log_directives(Some("   ")), None);
  }
}
