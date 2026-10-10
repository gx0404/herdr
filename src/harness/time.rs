//! harness 适配器共用的时间换算：RFC3339 文本、文件修改时间与 JSON 里的 Unix 秒都换成
//! Unix 毫秒。换不了（格式不对、早于纪元、溢出）一律 `None`，不 panic。

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// RFC3339 时间文本（如 `2026-09-22T10:00:00.123Z`）→ Unix 毫秒。
pub fn rfc3339_ms(text: &str) -> Option<u64> {
    let stamp = ::time::OffsetDateTime::parse(
        text.trim(),
        &::time::format_description::well_known::Rfc3339,
    )
    .ok()?;
    u64::try_from(stamp.unix_timestamp_nanos() / 1_000_000).ok()
}

/// JSON 字符串字段里的 RFC3339 时间；字段缺失或不是字符串时为 `None`。
pub fn rfc3339_value_ms(value: Option<&Value>) -> Option<u64> {
    value?.as_str().and_then(rfc3339_ms)
}

/// 系统时刻 → Unix 毫秒；早于纪元时为 `None`。
pub fn system_time_ms(time: SystemTime) -> Option<u64> {
    let elapsed = time.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(elapsed.as_millis()).ok()
}

/// 文件的最后修改时刻（Unix 毫秒）。
pub fn modified_ms(meta: &std::fs::Metadata) -> Option<u64> {
    system_time_ms(meta.modified().ok()?)
}

/// JSON 里以 Unix 秒记的整数时间 → 毫秒。
pub fn unix_seconds_ms(value: Option<&Value>) -> Option<u64> {
    value?.as_u64()?.checked_mul(1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rfc3339_text_becomes_unix_millis() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:01.250Z"), Some(1_250));
        assert_eq!(
            rfc3339_ms(" 2026-09-22T10:00:00+02:00 "),
            Some(1_790_064_000_000)
        );
        assert_eq!(rfc3339_ms("yesterday"), None);
        assert_eq!(rfc3339_ms("1969-12-31T23:59:59Z"), None, "早于纪元");
        assert_eq!(
            rfc3339_value_ms(Some(&json!("1970-01-01T00:00:02Z"))),
            Some(2_000)
        );
        assert_eq!(rfc3339_value_ms(Some(&json!(2_000))), None);
        assert_eq!(rfc3339_value_ms(None), None);
    }

    #[test]
    fn seconds_and_system_times_become_millis() {
        assert_eq!(unix_seconds_ms(Some(&json!(3))), Some(3_000));
        assert_eq!(unix_seconds_ms(Some(&json!(-3))), None);
        assert_eq!(unix_seconds_ms(Some(&json!(u64::MAX))), None, "溢出");
        assert_eq!(unix_seconds_ms(Some(&json!("3"))), None);
        assert_eq!(
            system_time_ms(UNIX_EPOCH + std::time::Duration::from_millis(42)),
            Some(42)
        );
        assert_eq!(
            system_time_ms(UNIX_EPOCH - std::time::Duration::from_millis(1)),
            None
        );
    }
}
