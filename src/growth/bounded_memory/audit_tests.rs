use super::*;

/// F8: 凭据原文不得落进 audit_log——write/update/remove 三处 detail 都先
/// redact_credentials 再截断；.md/DB 内容面保持原样，只有审计面脱敏。
#[test]
fn test_write_update_remove_audit_redacts_credentials() {
    let dir = std::env::temp_dir().join(format!(
        "asuna_growth_audit_{}_{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    // 凭据内容会被安全扫描拒绝，这里走受信任内部调用路径（scan 关闭）。
    let bm = BoundedMemory::new(&dir, &db, 2200, 1375).with_security_scan(false);

    let latest_detail = |action: &str| -> String {
        db.conn()
            .query_row(
                "SELECT detail FROM audit_log WHERE action = ?1 ORDER BY id DESC LIMIT 1",
                rusqlite::params![action],
                |r| r.get(0),
            )
            .unwrap()
    };
    // 拼接构造，避免源码里出现完整凭据字面量（与 growth/audit.rs 测试同姿势）。
    let secret_a = format!("{}{}", "sk-proj-", "aaaaaaaaaaaaaaaaaaaaaaaa");
    let secret_b = format!("{}{}", "sk-proj-", "bbbbbbbbbbbbbbbbbbbbbbbb");

    bm.write("memory", &format!("api key {secret_a}"), "high", None)
        .unwrap();
    assert!(
        bm.read("memory").unwrap().contains(&secret_a),
        "内容面不脱敏，只有审计 detail 脱敏"
    );
    let write_detail = latest_detail("write");
    assert!(write_detail.contains("content_preview"), "{write_detail}");
    assert!(
        !write_detail.contains(&secret_a),
        "raw secret leaked: {write_detail}"
    );
    assert!(write_detail.contains("[REDACTED]"), "{write_detail}");

    bm.update("memory", &secret_a, &secret_b, None).unwrap();
    let update_detail = latest_detail("update");
    assert!(
        !update_detail.contains(&secret_a),
        "raw old secret leaked: {update_detail}"
    );
    assert!(
        !update_detail.contains(&secret_b),
        "raw new secret leaked: {update_detail}"
    );
    assert_eq!(
        update_detail.matches("[REDACTED]").count(),
        2,
        "old/new 双侧脱敏: {update_detail}"
    );

    bm.remove("memory", &secret_b, None).unwrap();
    let remove_detail = latest_detail("remove");
    assert!(remove_detail.contains("removed"), "{remove_detail}");
    assert!(
        !remove_detail.contains(&secret_b),
        "raw secret leaked: {remove_detail}"
    );
    assert!(remove_detail.contains("[REDACTED]"), "{remove_detail}");

    std::fs::remove_dir_all(&dir).unwrap();
}
