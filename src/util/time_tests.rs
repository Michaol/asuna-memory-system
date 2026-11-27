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

/// C10: i64 极值时间戳在本地时区偏移叠加下也不得 panic（epoch fallback 兜底），
/// 且输出必须仍是可解析的 RFC 3339 并回落 epoch 时间点。
#[test]
fn test_unix_ms_to_iso_extremes_no_panic() {
    for ms in [i64::MAX, i64::MIN] {
        let s = unix_ms_to_iso(ms);
        let back = DateTime::parse_from_rfc3339(&s)
            .unwrap_or_else(|e| panic!("unix_ms_to_iso({ms}) = {s:?} 非合法 RFC 3339: {e}"));
        assert_eq!(back.timestamp_millis(), 0, "极值应回落 epoch: {s:?}");
    }
}

/// M6a: 超长非法 ISO 输入的错误信息必须截断（J28 同尺标——不得把整个
/// payload 嵌进错误串）。
#[test]
fn test_ts_to_unix_ms_truncates_oversized_invalid_input() {
    let long = "x".repeat(500);
    let msg = ts_to_unix_ms(&long).unwrap_err().to_string();
    assert!(msg.contains("truncated, 500 chars total"), "got: {msg}");
    assert!(!msg.contains(&"x".repeat(300)), "错误串不得包含完整原文");
}

/// C10: last_days 极值（i64::MAX / i64::MIN）经 clamp + saturating 不得
/// 溢出或把窗口推到未来。
#[test]
fn test_resolve_window_last_days_extremes_saturate() {
    let now = now_unix_ms();
    for days in [i64::MAX, i64::MIN] {
        let (after, _) = resolve_window(None, None, Some(days)).unwrap();
        let after = after.unwrap();
        let expected = now - if days < 0 { 0 } else { 36_500 * MS_PER_DAY };
        assert!(
            (after - expected).abs() < 60_000,
            "days={days}: after={after}"
        );
    }
}

/// v4-M: 倒置窗口只 warn 不 Err——返回值语义不变（调用方拿到空结果集）。
#[test]
fn test_resolve_window_inverted_still_ok() {
    let (after, before) = resolve_window(
        Some("2026-02-01T00:00:00Z"),
        Some("2026-01-01T00:00:00Z"),
        None,
    )
    .unwrap();
    assert!(after.unwrap() > before.unwrap());
}
