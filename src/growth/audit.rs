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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_action() {
        let db = crate::index::db::Db::open_memory().unwrap();
        db.init_schema().unwrap();

        log_action(&db, "write", "memory", "test detail", Some("session-1")).unwrap();

        let count: i64 = db.conn()
            .query_row("SELECT COUNT(*) FROM audit_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let (action, target): (String, String) = db.conn()
            .query_row("SELECT action, target FROM audit_log LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(action, "write");
        assert_eq!(target, "memory");
    }
}
