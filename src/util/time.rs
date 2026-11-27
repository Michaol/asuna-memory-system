use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

/// 一天的毫秒数
pub const MS_PER_DAY: i64 = 86_400_000;

/// ISO 8601 字符串转 Unix 毫秒时间戳
///
/// 带时区偏移的 RFC 3339 串按其偏移解析；无时区的 naive 串（含紧凑格式）
/// 一律按 **UTC** 解析。
pub fn ts_to_unix_ms(iso: &str) -> anyhow::Result<i64> {
    let iso = iso.trim();
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
    anyhow::bail!("无法解析时间戳: {}", crate::util::truncate_for_log(iso))
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
    use chrono::Offset;
    // [M4-FIX / C10] 对无效时间戳及本地偏移叠加溢出 NaiveDateTime 的极值
    // 使用 epoch fallback，避免 chrono::DateTime::naive_local() panic。
    let dt = Utc
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or(chrono::DateTime::UNIX_EPOCH);
    let local = dt.with_timezone(&chrono::Local);
    let safe_local = if local
        .naive_utc()
        .checked_add_offset(local.offset().fix())
        .is_some()
    {
        local
    } else {
        chrono::DateTime::UNIX_EPOCH.with_timezone(&chrono::Local)
    };
    safe_local.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 当前时间 Unix 毫秒
pub fn now_unix_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// `last_days` 窗口上限（约 100 年），防溢出与无意义极大负时间戳。
const MAX_LAST_DAYS: i64 = 36_500;

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
        let days = days.clamp(0, MAX_LAST_DAYS);
        Some(now_unix_ms().saturating_sub(days.saturating_mul(MS_PER_DAY)))
    } else {
        after_ms
    };
    // v4-M: 倒置窗口（after > before）结果恒空——保留 Ok（语义不变）但 warn 可观测。
    if let (Some(a), Some(b)) = (effective_after, before_ms) {
        if a > b {
            tracing::warn!(
                "time window inverted: after ({a}) > before ({b}); the query will match nothing"
            );
        }
    }
    Ok((effective_after, before_ms))
}

#[cfg(test)]
#[path = "time_tests.rs"]
mod tests;
