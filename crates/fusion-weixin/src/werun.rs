//! 微信运动数据解密原语（g048-M1 · 打卡挑战步数形态——计划 §3.2）。
//!
//! **解密即弃纪律（privacy §3 D-6 承诺级）**：session_key 与解密中间数据
//! （明文 JSON / stepInfoList 全量 / watermark）仅本函数栈内瞬态存在——
//! 不落库 / 不入日志 / 不入错误面（错误消息只含类别，MUST NOT 携带解密
//! 片段）；产出仅两条展示快照值（当日步数 + 快照日期）。步数 MUST NOT
//! 参与完赛判定与排行榜排序（裁决 #3——仅展示快照，消费方按此约束）。
//!
//! 形态：AES-128-CBC（key = session_key base64 解码 16 字节）+ PKCS#7；
//! 水印 appid 校验（跨应用数据拒）——判例 = push.rs `decrypt_aes_message`
//! 同线（aes/cbc crate）。

use aes::Aes128;
use cbc::Decryptor;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use serde::Deserialize;

use crate::{SessionKey, WeixinLoginClient};

type Aes128CbcDecryptor = Decryptor<Aes128>;

/// 步数展示快照（仅两条值——当日步数 + 快照日期；其余解密面即弃）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeRunSnapshot {
  /// 当日步数（stepInfoList 命中指定日期的条目值）。
  pub step_count: i32,
  /// 快照日期（`yyyy-MM-dd`，Asia/Shanghai 业务日期——调用方锚定口径）。
  pub step_date: String,
}

/// werun 解密错误（类别化——MUST NOT 携带解密中间数据）。
#[derive(Debug, thiserror::Error)]
pub enum WeRunError {
  /// 输入形态非法（base64 / 长度 / padding——session 过期或篡改面统一归此类）。
  #[error("werun decrypt failed: malformed payload")]
  Malformed,
  /// 水印 appid 与配置不符（跨应用数据）。
  #[error("werun decrypt failed: watermark mismatch")]
  Watermark,
  /// 指定日期无步数条目（微信运动数据仅进入小程序时更新——当日可能缺失）。
  #[error("werun decrypt failed: no step entry for the date")]
  NoEntry,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WeRunPlain {
  step_info_list: Vec<StepEntry>,
  watermark: Watermark,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StepEntry {
  timestamp: i64,
  step: i64,
}

#[derive(Deserialize)]
struct Watermark {
  appid: String,
}

/// 解密微信运动数据并取指定业务日期的步数快照。
///
/// - `session_key`：当次会话密钥（jscode2session 携密原语产出——调用方即用即弃）；
/// - `encrypted_data` / `iv`：`wx.getWeRunData` 出参（base64）；
/// - `appid`：本小程序 appid（水印校验锚）；
/// - `today`：目标业务日期（`yyyy-MM-dd`，Asia/Shanghai 折算由调用方承载）。
pub fn decrypt_werun_snapshot(
  session_key: &SessionKey,
  encrypted_data: &str,
  iv: &str,
  appid: &str,
  today: &str,
) -> Result<WeRunSnapshot, WeRunError> {
  use base64::Engine as _;
  let engine = base64::engine::general_purpose::STANDARD;
  let key = engine.decode(session_key.expose_for_signing()).map_err(|_| WeRunError::Malformed)?;
  let iv = engine.decode(iv).map_err(|_| WeRunError::Malformed)?;
  let data = engine.decode(encrypted_data).map_err(|_| WeRunError::Malformed)?;
  let mut buf = data;
  if buf.len() % 16 != 0 || buf.is_empty() {
    return Err(WeRunError::Malformed);
  }
  let plain = Aes128CbcDecryptor::new_from_slices(&key, &iv)
    .map_err(|_| WeRunError::Malformed)?
    .decrypt_padded_mut::<Pkcs7>(&mut buf)
    .map_err(|_| WeRunError::Malformed)?;
  // 解析即弃：明文 JSON 仅本栈存在，错误不回传内容。
  let parsed: WeRunPlain = serde_json::from_slice(plain).map_err(|_| WeRunError::Malformed)?;
  if parsed.watermark.appid != appid {
    return Err(WeRunError::Watermark);
  }
  // 目标日期条目（timestamp 折算 Asia/Shanghai 业务日期比较）。
  for entry in parsed.step_info_list {
    let date = naive_date_asia_shanghai(entry.timestamp);
    if date == today {
      return Ok(WeRunSnapshot { step_count: entry.step.clamp(0, i32::MAX as i64) as i32, step_date: date });
    }
  }
  Err(WeRunError::NoEntry)
}

/// Unix 秒 → Asia/Shanghai 业务日期（`yyyy-MM-dd`；固定 +08:00 偏移——无夏令时）。
fn naive_date_asia_shanghai(ts: i64) -> String {
  let secs = ts + 8 * 3600;
  let days = secs.div_euclid(86_400);
  // 公历换算（1970-01-01 纪元——civil_from_days 简式，覆盖微信运动 30 天窗口
  // 与未来数十年量级）。
  let z = days + 719_468;
  let era = z.div_euclid(146_097);
  let doe = z.rem_euclid(146_097);
  let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
  let y = yoe + era * 400;
  let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
  let mp = (5 * doy + 2) / 153;
  let d = doy - (153 * mp + 2) / 5 + 1;
  let m = if mp < 10 { mp + 3 } else { mp - 9 };
  let y = if m <= 2 { y + 1 } else { y };
  format!("{y:04}-{m:02}-{d:02}")
}

impl WeixinLoginClient {
  /// 微信运动解密组合原语（g048 挑战步数形态——appid 不外泄，水印校验在
  /// crate 内闭合）：`wx.login` code → 携密会话 → AES-128-CBC 解密 → 指定业务
  /// 日期步数快照。**解密即弃**：session_key / 明文 / stepInfoList 仅本调用栈
  /// 存在（ZeroizeOnDrop + 局部作用域）；错误类别化不携内容。app 错误（换码
  /// 失败 / 未配置）与解密错误（Malformed）统一 Malformed——调用方退避
  /// `werun_unavailable`（session 过期 → 端上 checkSession 重登录重取链）。
  pub async fn decrypt_werun_steps(
    &self,
    code: &str,
    encrypted_data: &str,
    iv: &str,
    today: &str,
  ) -> Result<WeRunSnapshot, WeRunError> {
    let session = self.exchange_mp_with_session_key(code).await.map_err(|_| WeRunError::Malformed)?;
    let appid = self.mp_appid_or_empty();
    decrypt_werun_snapshot(&session.session_key, encrypted_data, iv, appid, today)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use base64::Engine as _;

  fn enc(key: &[u8], iv: &[u8], plain: &str) -> String {
    use aes::cipher::block_padding::Pkcs7;
    use aes::cipher::{BlockEncryptMut, KeyIvInit};
    use base64::Engine as _;
    type Enc = cbc::Encryptor<Aes128>;
    let mut buf = vec![0u8; plain.len() + 32];
    buf[..plain.len()].copy_from_slice(plain.as_bytes());
    let ct = Enc::new_from_slices(key, iv).unwrap().encrypt_padded_mut::<Pkcs7>(&mut buf, plain.len()).unwrap();
    base64::engine::general_purpose::STANDARD.encode(ct)
  }

  fn sk(key: &[u8]) -> SessionKey {
    use base64::Engine as _;
    SessionKey(base64::engine::general_purpose::STANDARD.encode(key))
  }

  /// 2026-10-19（Asia/Shanghai）当日 00:00 UTC+8 的 Unix 秒。
  const TS_TODAY: i64 = 1_792_358_400;

  fn plain_json(appid: &str) -> String {
    format!(
      r#"{{"stepInfoList":[{{"timestamp":{},"step":12345}},{{"timestamp":{},"step":999}}],"watermark":{{"timestamp":{},"appid":"{}"}}}}"#,
      TS_TODAY,
      TS_TODAY - 86_400,
      TS_TODAY,
      appid
    )
  }

  #[test]
  fn decrypt_today_entry() {
    let key = [7u8; 16];
    let iv = [8u8; 16];
    let data = enc(&key, &iv, &plain_json("wx-app"));
    let snap = decrypt_werun_snapshot(
      &sk(&key),
      &data,
      &base64::engine::general_purpose::STANDARD.encode(iv),
      "wx-app",
      "2026-10-19",
    )
    .unwrap();
    assert_eq!(snap.step_count, 12345);
    assert_eq!(snap.step_date, "2026-10-19");
  }

  #[test]
  fn watermark_mismatch_rejected() {
    let key = [7u8; 16];
    let iv = [8u8; 16];
    let data = enc(&key, &iv, &plain_json("wx-other"));
    let err = decrypt_werun_snapshot(
      &sk(&key),
      &data,
      &base64::engine::general_purpose::STANDARD.encode(iv),
      "wx-app",
      "2026-10-19",
    )
    .unwrap_err();
    assert!(matches!(err, WeRunError::Watermark));
  }

  #[test]
  fn no_entry_for_date() {
    let key = [7u8; 16];
    let iv = [8u8; 16];
    let data = enc(&key, &iv, &plain_json("wx-app"));
    let err = decrypt_werun_snapshot(
      &sk(&key),
      &data,
      &base64::engine::general_purpose::STANDARD.encode(iv),
      "wx-app",
      "2030-01-01",
    )
    .unwrap_err();
    assert!(matches!(err, WeRunError::NoEntry));
  }

  #[test]
  fn malformed_payload_is_category_only() {
    let err = decrypt_werun_snapshot(&sk(&[7u8; 16]), "not-base64!!", "aaaa", "wx-app", "2026-10-19").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("malformed"));
    assert!(!msg.contains("stepInfo"));
  }

  #[test]
  fn date_conversion_boundaries() {
    // 纪元日 + 已知日（2026-10-19 00:00 UTC = 前 8h 属 10-18 业务日）。
    assert_eq!(naive_date_asia_shanghai(0), "1970-01-01");
    assert_eq!(naive_date_asia_shanghai(TS_TODAY), "2026-10-19");
  }
}
