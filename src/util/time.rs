use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

/// 一天的毫秒数
pub const MS_PER_DAY: i64 = 86_400_000;

/// ISO 8601 字符串转 Unix 毫秒时间戳
///
/// 带时区偏移的 RFC 3339 串按其偏移解析；无时区的 naive 串（含紧凑格式）
/// 一律按 **UTC** 解析。
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

/// Unix 毫秒时间戳转 ISO 8601 (RFC 3339，毫秒精度) 字符串。
///
/// 按 **运行时本地时区** 渲染并携带该时区偏移（如 +08 机器上
/// `1970-01-01T08:00:00.000+08:00`；UTC 机器上偏移为零，`use_z=true`
/// 渲染为 `Z`）。注意 `to_rfc3339_opts(.., true)` 并不会强制 UTC：
/// 它渲染本地墙钟 + 自带偏移，仅当偏移为零时才写成 `Z`。消费方
/// （`ts_to_unix_ms`、`compute_session_path`）按串内偏移重新解析，
/// 时间点不变；但 CLI 显示与会话归档日期桶跟随机器时区（历史行为）。
pub fn unix_ms_to_iso(ms: i64) -> String {
    // [M4-FIX] 对无效时间戳使用 epoch fallback，避免 panic
    let dt = Utc
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(|| Utc.timestamp_millis_opt(0).single().unwrap());
    dt.with_timezone(&chrono::Local)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 当前时间 Unix 毫秒
pub fn now_unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// 解析 after/before ISO 串与 last_days，归一为 `(after_ms, before_ms)` 时间窗。
///
/// 语义（原 http.rs `parse_time_window`，现为其 /recall、/search、MCP
/// `search_sessions` 与 CLI `search` 四路共用）：
/// - `after`/`before` 解析失败 → `Err`（绝不静默放宽窗口）；
/// - `last_days` clamp 到 `[0, 36_500]`（负值会把 after 推到未来从而过滤
///   掉全部数据，超大值会溢出 i64），且 **覆盖** `after`；
/// - `last_days` 为 `None` 时 after 取解析后的 `after`。
pub fn resolve_window(
    after: Option<&str>,
    before: Option<&str>,
    last_days: Option<i64>,
) -> anyhow::Result<(Option<i64>, Option<i64>)> {
    let after_ms = match after {
        Some(s) => Some(ts_to_unix_ms(s).map_err(|e| anyhow::anyhow!("invalid after: {}", e))?),
        None => None,
    };
    let before_ms = match before {
        Some(s) => Some(ts_to_unix_ms(s).map_err(|e| anyhow::anyhow!("invalid before: {}", e))?),
        None => None,
    };
    let effective_after = if let Some(days) = last_days {
        let days = days.clamp(0, 36_500);
        Some(now_unix_ms() - days * MS_PER_DAY)
    } else {
        after_ms
    };
    Ok((effective_after, before_ms))
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

    #[test]
    fn test_unix_ms_to_iso_roundtrips_via_rfc3339() {
        // 输出携带运行时本地时区的偏移（+08 机器为 +08:00，UTC 机器为 Z），
        // 与机器时区无关的契约是：串必须可被 RFC 3339 重新解析且回到同一
        // 时间点（compute_session_path / CLI 切片 / ts_to_unix_ms 消费方依赖）。
        for ms in [0i64, 1_767_225_600_000, now_unix_ms()] {
            let s = unix_ms_to_iso(ms);
            let back = DateTime::parse_from_rfc3339(&s)
                .unwrap_or_else(|e| panic!("unix_ms_to_iso({ms}) = {s:?} 非合法 RFC 3339: {e}"));
            assert_eq!(back.timestamp_millis(), ms, "roundtrip 改变了时间点: {s:?}");
        }
        // 无效时间戳走 epoch fallback，不 panic，且回落到 epoch
        let s = unix_ms_to_iso(i64::MAX);
        let back = DateTime::parse_from_rfc3339(&s).expect("epoch fallback 非合法 RFC 3339");
        assert_eq!(back.timestamp_millis(), 0);
    }

    #[test]
    fn test_resolve_window_no_filters() {
        let (after, before) = resolve_window(None, None, None).unwrap();
        assert_eq!(after, None);
        assert_eq!(before, None);
    }

    #[test]
    fn test_resolve_window_parses_utc_and_naive() {
        // 2026-01-01T00:00:00Z = 1_767_225_600_000 ms
        let (after, before) = resolve_window(
            Some("2026-01-01T00:00:00Z"),
            Some("2026-02-01T00:00:00Z"),
            None,
        )
        .unwrap();
        assert_eq!(after, Some(1_767_225_600_000));
        assert_eq!(before, Some(1_767_225_600_000 + 31 * MS_PER_DAY));
        // naive 串按 UTC 解析，与 Z 后缀版本一致
        let (naive, _) = resolve_window(Some("2026-01-01T00:00:00"), None, None).unwrap();
        assert_eq!(naive, Some(1_767_225_600_000));
    }

    #[test]
    fn test_resolve_window_last_days_clamps_and_overrides_after() {
        let now = now_unix_ms();

        // last_days 覆盖显式 after
        let (after, _) = resolve_window(Some("2020-01-01T00:00:00Z"), None, Some(7)).unwrap();
        let after = after.unwrap();
        assert!(
            (after - (now - 7 * MS_PER_DAY)).abs() < 60_000,
            "after should be now-7d, got {}",
            after
        );

        // 负值 clamp 到 0 → after ≈ now
        let (after, _) = resolve_window(None, None, Some(-5)).unwrap();
        let after = after.unwrap();
        assert!((now - after) < 60_000);

        // 超大值 clamp 到 36_500 天
        let (after, _) = resolve_window(None, None, Some(i64::MAX / 2)).unwrap();
        let after = after.unwrap();
        assert!(
            (after - (now - 36_500 * MS_PER_DAY)).abs() < 60_000,
            "after should be now-36500d, got {}",
            after
        );
    }

    #[test]
    fn test_resolve_window_invalid_strings_error() {
        let e = resolve_window(Some("not-a-date"), None, None).unwrap_err();
        assert!(e.to_string().contains("invalid after"), "got: {}", e);
        let e = resolve_window(None, Some("yesterday"), None).unwrap_err();
        assert!(e.to_string().contains("invalid before"), "got: {}", e);
        // last_days 提供时非法 after 仍须报错（解析先于覆盖）
        let e = resolve_window(Some("garbage"), None, Some(7)).unwrap_err();
        assert!(e.to_string().contains("invalid after"), "got: {}", e);
    }
}
