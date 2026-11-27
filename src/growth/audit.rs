use crate::index::db::Db;
use crate::util::time;

/// 记录审计日志
pub fn log_action(
    db: &Db,
    action: &str,
    target: &str,
    detail: &str,
    session_id: Option<&str>,
) -> anyhow::Result<()> {
    let now = time::now_unix_ms();
    db.conn().execute(
        "INSERT INTO audit_log (timestamp_ms, action, target, detail, session_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![now, action, target, detail, session_id],
    )?;
    Ok(())
}

/// U10 软处理：使用预先计算好的安全扫描结果记 warn 与 `security_scan_flag` 审计行。
pub fn record_unsafe_turn(
    db: &Db,
    session_id: &str,
    index: usize,
    content: &str,
    scan: &crate::growth::security::ScanResult,
) -> bool {
    if scan.is_safe() {
        return false;
    }
    tracing::warn!(
        "turn[{}] flagged by security scan ({}): session {}",
        index,
        scan.reason(),
        session_id
    );
    let redacted = crate::growth::security::redact_credentials(content);
    let summary: String = redacted.chars().take(200).collect();
    let detail = format!("turn[{}] flagged: {} | {}", index, scan.reason(), summary);
    if let Err(e) = log_action(db, "security_scan_flag", "turn", &detail, Some(session_id)) {
        tracing::warn!("failed to audit security_scan_flag: {}", e);
    }
    true
}

/// U10 软处理：扫描原始对话 turn（gateway /capture 与 MCP save_session 共用）。
/// 命中不安全模式**不阻断**——原始对话保真，避免误伤合法的安全讨论——但会
/// warn + 写入 `security_scan_flag` 审计行，使投毒尝试在 audit_log 可见。
/// 审计失败仅 warn（与 duplicate_skip 同姿势）。返回是否被标记。
pub fn flag_unsafe_turn(db: &Db, session_id: &str, index: usize, content: &str) -> bool {
    let scan = crate::growth::security::scan_content(content);
    record_unsafe_turn(db, session_id, index, content, &scan)
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
