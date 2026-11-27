use crate::config::Config;
use crate::embedder::{api::ApiFormat, LazyEmbedder};
use crate::growth::bounded_memory::BoundedMemory;
use crate::index::db::Db;
use crate::index::rebuild;
use crate::util;
use std::path::Path;

pub(crate) fn cmd_doctor(
    config: &Config,
    db: &Db,
    db_path: &Path,
    verbose: bool,
    fix: bool,
    split_entries: bool,
) -> anyhow::Result<()> {
    // M8d: 诊断查询失败必须可感知——收集实际错误，输出含错误详情，
    // 结束时以非零退出码告知自动化调用方（对齐 rebuild W4 姿势）。
    let mut diag_errors: Vec<String> = Vec::new();
    doctor_print_header(config, db, db_path, &mut diag_errors)?;
    doctor_print_embedder(config);
    doctor_print_limits_and_profiles(config);
    let turn_count = doctor_print_index_stats(db, &mut diag_errors);
    doctor_print_graph_stats(config, db, &mut diag_errors);
    if verbose && config.graph.enabled {
        doctor_print_graph_coverage(db, turn_count, &mut diag_errors);
    }

    // 拆分多条目坏行（独立于 --fix：仅修复 DB 行数与 .md 条目数不一致，
    // 不触发 .md/SQLite 的无损合并）。
    if split_entries {
        doctor_split_entries(config, db)?;
    }

    // Bounded memory DB/.md 一致性检查
    doctor_reconcile_all(config, db, fix)?;

    doctor_print_consistency(config, db)?;

    if !diag_errors.is_empty() {
        anyhow::bail!(
            "doctor 诊断存在 {} 项失败（详见上方输出）",
            diag_errors.len()
        );
    }
    Ok(())
}

fn doctor_print_header(
    config: &Config,
    db: &Db,
    db_path: &Path,
    diag_errors: &mut Vec<String>,
) -> anyhow::Result<()> {
    println!("=== Asuna Memory Doctor ===");
    println!("版本: v{}", env!("CARGO_PKG_VERSION"));
    println!("数据目录: {}", config.data_dir.display());
    println!("Profile: {}", config.profile_id);
    println!("Profile 目录: {}", config.profile_dir().display());
    println!("数据库: {}", db_path.display());
    println!(
        "完整性检查: {}",
        if db.integrity_check()? {
            "OK"
        } else {
            "FAILED"
        }
    );
    let fk_status = db
        .conn()
        .query_row("PRAGMA foreign_keys", [], |r| r.get::<_, i64>(0));
    println!(
        "外键约束: {}",
        match fk_status {
            Ok(1) => "ON".to_string(),
            Ok(_) => "OFF (建议升级)".to_string(),
            Err(e) => {
                diag_errors.push(format!("外键约束查询: {}", e));
                format!("查询失败 ({})", e)
            }
        }
    );
    Ok(())
}

fn doctor_print_embedder(config: &Config) {
    let api_configured =
        !config.embedding.api_url.is_empty() && !config.embedding.api_model.is_empty();

    if api_configured {
        doctor_print_api_embedder(config);
    } else {
        println!("嵌入后端: 无（纯关键词模式）");
        println!("嵌入引擎状态: DISABLED");
        println!("  如需启用 1024 维语义/混合检索，请在 config.json 中设置 embedding.api_url + api_model");
        println!("  （并通过 AMS_EMBEDDING_API_KEY 环境变量或 config.json 提供 API Key）");
    }
}

fn doctor_print_api_embedder(config: &Config) {
    // M13: 打印规范化后的 format 标签（与实际解析一致），而非原始 config 串。
    let fmt = ApiFormat::from_str(&config.embedding.api_format).as_str();
    println!(
        "嵌入后端: API ({} / {}, format={}, dim={})",
        util::sanitize_url_for_log(&config.embedding.api_url),
        config.embedding.api_model,
        fmt,
        config.embedding.dimensions
    );
    let embedder = config.create_embedder();
    match embedder {
        Some(ref emb) => doctor_probe_api_embedder(emb),
        None => {
            println!("嵌入引擎状态: DISABLED (API 创建失败)");
            println!("  语义搜索不可用，将降级为纯关键词搜索");
        }
    }
}

fn doctor_probe_api_embedder(emb: &LazyEmbedder) {
    match emb.embed_query("test") {
        Ok(v) => println!("嵌入引擎状态: OK (API, 维度={})", v.len()),
        Err(e) => {
            println!("嵌入引擎状态: FAILED ({})", e);
            println!("  语义搜索不可用，将降级为纯关键词搜索");
        }
    }
}

fn doctor_print_limits_and_profiles(config: &Config) {
    println!("Memory 容量限制: {} chars", config.memory.memory_char_limit);
    println!("User 容量限制: {} chars", config.memory.user_char_limit);
    let profiles = config.list_profiles();
    println!(
        "可用 Profiles: {}",
        if profiles.is_empty() {
            "(none)".to_string()
        } else {
            profiles.join(", ")
        }
    );
}

/// doctor 计数查询不得把失败伪装成 0——0 是合法统计值，查询失败是故障，
/// 输出必须可区分（Informational 修复：unwrap_or(0) 误报）。
/// M8d: 实际错误同时记入 diag_errors，供退出码反映。
fn doctor_count(db: &Db, sql: &str, diag_errors: &mut Vec<String>) -> String {
    match db.conn().query_row(sql, [], |r| r.get::<_, i64>(0)) {
        Ok(n) => n.to_string(),
        Err(e) => {
            diag_errors.push(format!("计数查询 [{}]: {}", sql, e));
            format!("查询失败 ({})", e)
        }
    }
}

// 对话统计
fn doctor_print_index_stats(db: &Db, diag_errors: &mut Vec<String>) -> Option<i64> {
    let session_count = doctor_count(db, "SELECT COUNT(*) FROM sessions", diag_errors);
    // M8d: turn 计数失败不得经 .ok() 吞掉错误详情——打印实际错误并记录。
    let (turn_count, turn_display) =
        match db
            .conn()
            .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get::<_, i64>(0))
        {
            Ok(n) => (Some(n), n.to_string()),
            Err(e) => {
                diag_errors.push(format!("turn 计数查询: {}", e));
                (None, format!("查询失败 ({})", e))
            }
        };
    let vec_count = doctor_count(db, "SELECT COUNT(*) FROM vec_turns_rowids", diag_errors);
    // J13 兜底：只读孤儿检测——vec_turns 中 rowid 已不在 turns 的向量行。
    // save 路径的定点清理使其正常情况下恒为 0；非 0 说明历史上有异常删除
    // 路径，rebuild 可回收（doctor 只报告，不做删除）。
    let orphan_note = match db.conn().query_row(
        "SELECT COUNT(*) FROM vec_turns_rowids WHERE rowid NOT IN (SELECT id FROM turns)",
        [],
        |r| r.get::<_, i64>(0),
    ) {
        Ok(n) if n > 0 => format!("（含 {} 个孤儿向量，rebuild 可回收）", n),
        Ok(_) => String::new(),
        Err(e) => {
            diag_errors.push(format!("孤儿向量查询: {}", e));
            format!("（孤儿向量查询失败: {}）", e)
        }
    };
    println!(
        "索引统计: {} 会话, {} 轮对话, {} 个向量{}",
        session_count, turn_display, vec_count, orphan_note
    );
    turn_count
}

// 图谱统计
fn doctor_print_graph_stats(config: &Config, db: &Db, diag_errors: &mut Vec<String>) {
    let entity_count = doctor_count(db, "SELECT COUNT(*) FROM entities", diag_errors);
    let relation_count = doctor_count(db, "SELECT COUNT(*) FROM relations", diag_errors);
    let graph_status = if config.graph.enabled {
        format!(
            "ENABLED ({} entities, {} relations)",
            entity_count, relation_count
        )
    } else {
        "DISABLED (config.graph.enabled = false)".to_string()
    };
    println!("图谱: {}", graph_status);

    if config.graph_using_defaults {
        println!(
            "  ⚠ config.json 缺少 'graph' 配置段，使用默认值 (enabled={}, remind_on_save={})",
            config.graph.enabled, config.graph.remind_on_save
        );
        println!(
            "  如需自定义，在 config.json 中添加: \"graph\": {{ \"enabled\": true, \"remind_on_save\": true }}"
        );
    }
}

fn doctor_print_graph_coverage(db: &Db, turn_count: Option<i64>, diag_errors: &mut Vec<String>) {
    // 覆盖率：有多少 turn 至少被一条 relation 引用
    // W6: 排除悬空 source_turn（软引用按设计可悬空），使 covered+dangling 自洽、
    // 覆盖率不致 >100%。
    let covered = db.conn().query_row(
        "SELECT COUNT(DISTINCT source_turn) FROM relations
         WHERE source_turn IS NOT NULL
           AND EXISTS (SELECT 1 FROM turns t WHERE t.id = relations.source_turn)",
        [],
        |r| r.get::<_, i64>(0),
    );
    // 悬空引用：relations.source_turn 不存在于 turns 表
    let dangling = db.conn().query_row(
        "SELECT COUNT(DISTINCT r.source_turn) FROM relations r
         WHERE r.source_turn IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.id = r.source_turn)",
        [],
        |r| r.get::<_, i64>(0),
    );
    match (covered, turn_count) {
        (Ok(covered), Some(tc)) => {
            let coverage_pct = if tc > 0 {
                (covered as f64 / tc as f64 * 100.0).round() as i64
            } else {
                0
            };
            println!("图谱覆盖率: {}% ({}/{} turns)", coverage_pct, covered, tc);
        }
        (Err(e), _) => {
            diag_errors.push(format!("图谱覆盖率查询: {}", e));
            println!("图谱覆盖率: 查询失败 ({})", e);
        }
        _ => println!("图谱覆盖率: 查询失败（turn 总数未知）"),
    }
    match dangling {
        Ok(n) => println!("图谱悬空引用: {}", n),
        Err(e) => {
            diag_errors.push(format!("图谱悬空引用查询: {}", e));
            println!("图谱悬空引用: 查询失败 ({})", e);
        }
    }
}

fn doctor_split_entries(config: &Config, db: &Db) -> anyhow::Result<()> {
    for target in &["memory", "user"] {
        let bm = BoundedMemory::new(
            &config.memory_dir(),
            db,
            config.memory.memory_char_limit,
            config.memory.user_char_limit,
        )
        .with_security_scan(config.memory.security_scan);

        let report = bm.split_multi_entry_rows(target)?;
        if report.bad_rows == 0 {
            println!("bounded_memory[{}]: 无需拆分（无多条目坏行）", target);
        } else {
            println!(
                "bounded_memory[{}]: 拆分 {} 条多条目坏行 → 新增 {} 子条目, 跳过 {} 重复",
                target, report.bad_rows, report.sub_entries_created, report.duplicates_skipped
            );
        }
    }
    Ok(())
}

fn doctor_reconcile_all(config: &Config, db: &Db, fix: bool) -> anyhow::Result<()> {
    for target in &["memory", "user"] {
        doctor_reconcile_target(config, db, target, fix)?;
    }
    Ok(())
}

pub(crate) fn doctor_reconcile_target(
    config: &Config,
    db: &Db,
    target: &str,
    fix: bool,
) -> anyhow::Result<()> {
    let bm = BoundedMemory::new(
        &config.memory_dir(),
        db,
        config.memory.memory_char_limit,
        config.memory.user_char_limit,
    )
    .with_security_scan(config.memory.security_scan);

    let report = bm.reconcile_check(target)?;
    if report.only_in_md.is_empty() && report.only_in_db.is_empty() {
        println!(
            "bounded_memory[{}]: OK ({} entries)",
            target, report.db_entry_count
        );
    } else {
        println!(
            "WARNING bounded_memory[{}]: DIVERGED (.md={}, db={})",
            target, report.md_entry_count, report.db_entry_count
        );
        if !report.only_in_md.is_empty() {
            println!("  only in .md: {} entries", report.only_in_md.len());
        }
        if !report.only_in_db.is_empty() {
            println!("  only in SQLite: {} entries", report.only_in_db.len());
        }
        if fix {
            let count = bm.reconcile_fix(target)?;
            println!("  Fixed: merged .md and SQLite ({} entries total)", count);
        } else {
            println!("  Run doctor --fix to repair (lossless merge of .md and SQLite)");
        }
    }
    Ok(())
}

// 一致性检查
fn doctor_print_consistency(config: &Config, db: &Db) -> anyhow::Result<()> {
    let consistency = rebuild::check_consistency(&config.conversations_dir(), db)?;
    println!(
        "一致性: JSONL={} vs DB={} → {}",
        consistency.jsonl_count,
        consistency.db_session_count,
        if consistency.in_sync {
            "OK"
        } else {
            "不同步，建议运行 rebuild"
        }
    );
    Ok(())
}
