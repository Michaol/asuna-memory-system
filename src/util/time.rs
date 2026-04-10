use chrono::{DateTime, TimeZone, Utc, NaiveDateTime};

/// ISO 8601 字符串转 Unix 毫秒时间戳
pub fn ts_to_unix_ms(iso: &str) -> anyhow::Result<i64> {
    // 尝试带时区解析
    if let Ok(dt) = DateTime::parse_from_rfc3339(iso) {
        return Ok(dt.timestamp_millis());
    }
    // 尝试无时区格式 (假设 UTC)
    if let Ok(naive) = NaiveDateTime::parse_from_str(iso, "%Y-%m-%dT%H:%M:%S%.f") {
        return Ok(naive.and_utc().timestamp_millis());
    }
    // 尝试紧凑格式
    if let Ok(naive) = NaiveDateTime::parse_from_str(iso, "%Y%m%dT%H%M%S") {
        return Ok(naive.and_utc().timestamp_millis());
    }
    anyhow::bail!("无法解析时间戳: {}", iso)
}

/// Unix 毫秒时间戳转 ISO 8601 字符串 (+08:00)
pub fn unix_ms_to_iso(ms: i64) -> String {
    let dt = Utc.timestamp_millis_opt(ms).single().unwrap();
    let local = dt.with_timezone(&chrono::Local);
    local.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 当前时间 Unix 毫秒
pub fn now_unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ts_to_unix_ms_rfc3339() {
        let ts = ts_to_unix_ms("2026-04-10T10:02:05.123+08:00").unwrap();
        assert!(ts > 0);
    }

    #[test]
    fn test_roundtrip() {
        let original = "2026-04-10T10:02:05.123+08:00";
        let ms = ts_to_unix_ms(original).unwrap();
        let back = unix_ms_to_iso(ms);
        // 重新解析应得到相同时间点
        let ms2 = ts_to_unix_ms(&back).unwrap();
        assert_eq!(ms, ms2);
    }

    #[test]
    fn test_now_positive() {
        let now = now_unix_ms();
        assert!(now > 1_700_000_000_000); // 2023-11-14 之后
    }
}
