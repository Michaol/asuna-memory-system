use super::args::SearchFilters;
use crate::config::Config;
use crate::fact::conversation;
use crate::fact::search::{search_sessions, SearchMode, SearchParams};
use crate::fact::session_store::SessionStore;
use crate::index::db::Db;
use crate::index::rebuild;
use crate::util::{time, validate_session_id, VALID_ROLES};
use std::path::Path;

pub(crate) fn cmd_list_profiles(config: &Config) {
    let profiles = config.list_profiles();
    if profiles.is_empty() {
        println!("没有找到任何 profile");
    } else {
        for p in &profiles {
            let marker = if *p == config.profile_id {
                " (active)"
            } else {
                ""
            };
            println!("  {}{}", p, marker);
        }
    }
}

pub(crate) fn cmd_list_sessions(
    _config: &Config,
    db: &Db,
    last_days: Option<i64>,
    limit: usize,
) -> anyhow::Result<()> {
    let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
    let (cutoff_opt, _) = time::resolve_window(None, None, last_days)?;
    let (query, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(cutoff) = cutoff_opt
    {
        (
            "SELECT session_id, start_ts, source, turn_count, title
             FROM sessions WHERE start_ts >= ?1
             ORDER BY start_ts DESC LIMIT ?2"
                .to_string(),
            vec![Box::new(cutoff), Box::new(limit_i64)],
        )
    } else {
        (
            "SELECT session_id, start_ts, source, turn_count, title
             FROM sessions ORDER BY start_ts DESC LIMIT ?1"
                .to_string(),
            vec![Box::new(limit_i64)],
        )
    };

    let mut stmt = db.conn().prepare(&query)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(Box::as_ref).collect();

    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        let ts_ms: i64 = row.get(1)?;
        Ok((
            row.get::<_, String>(0)?,
            time::unix_ms_to_iso(ts_ms),
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?.unwrap_or(0),
            row.get::<_, Option<String>>(4)?,
        ))
    })?;

    let mut count = 0;
    println!(
        "{:<38} {:<25} {:<6} {:<15} TITLE",
        "SESSION_ID", "TIME", "TURNS", "SOURCE"
    );
    println!("{}", "-".repeat(110));
    for row in rows {
        let (sid, ts, source, turns, title) = row?;
        let ts_short: String = ts.chars().take(25).collect();
        println!(
            "{:<38} {:<25} {:<6} {:<15} {}",
            sid,
            ts_short,
            turns,
            source.unwrap_or_else(|| "-".to_string()),
            title.unwrap_or_else(|| "-".to_string()),
        );
        count += 1;
    }
    println!("\n共 {} 个会话", count);
    Ok(())
}

pub(crate) fn cmd_search(
    config: &Config,
    db: &Db,
    query: &str,
    top_k: usize,
    mode: &str,
    filters: SearchFilters,
) -> anyhow::Result<()> {
    // W5: 空/纯空白 query 在入口拒绝（与 MCP search_sessions / HTTP /search 同口径）。
    if query.trim().is_empty() {
        anyhow::bail!("query 不能为空或纯空白");
    }
    // 白名单预校验：from_cli_str 对未知值静默回落 Hybrid（历史兼容），
    // CLI 入口改为报错；vector/fts 别名保留（见 --mode help）。
    if !matches!(mode, "hybrid" | "semantic" | "keyword" | "vector" | "fts") {
        anyhow::bail!(
            "invalid search mode '{}': must be one of 'hybrid', 'semantic', 'keyword' (CLI aliases: 'vector', 'fts')",
            mode
        );
    }
    let search_mode = SearchMode::from_cli_str(mode);
    if let Some(ref role) = filters.role {
        if !VALID_ROLES.contains(&role.as_str()) {
            anyhow::bail!("invalid role '{}': must be one of {:?}", role, VALID_ROLES);
        }
    }

    // 自动发现并创建嵌入器
    let embedder = config.create_embedder();

    // 时间过滤：--last-days 覆盖 --after；时间戳解析失败直接报错而非静默忽略
    // （与 HTTP /search·/recall、MCP search_sessions 共用 util::time::resolve_window）
    let (effective_after, before_ms) = time::resolve_window(
        filters.after.as_deref(),
        filters.before.as_deref(),
        filters.last_days,
    )?;

    let params = SearchParams {
        query: query.to_string(),
        search_mode,
        top_k,
        after_ms: effective_after,
        before_ms,
        role: filters.role,
    };

    let results = search_sessions(db, embedder.as_ref(), &params)?;

    println!("搜索: \"{}\" (mode={})", query, mode);
    println!();
    for (i, r) in results.iter().enumerate() {
        let ts = time::unix_ms_to_iso(r.timestamp_ms);
        let ts_short: String = ts.chars().take(19).collect();
        println!(
            "[{}] score={:.4} | {} | {} | {}",
            i + 1,
            r.score,
            ts_short,
            r.role,
            r.session_id,
        );
        println!("    {}\n", r.preview);
    }
    println!("共 {} 条结果", results.len());
    Ok(())
}

pub(crate) fn cmd_rebuild(config: &Config, db: &Db, full: bool) -> anyhow::Result<()> {
    println!(
        "从 JSONL 重建索引{}...",
        if full {
            "（完整模式）"
        } else {
            "（增量模式）"
        }
    );
    let embedder = config.create_embedder();
    let stats = rebuild::rebuild_from_jsonl_with_preview(
        &config.conversations_dir(),
        db,
        embedder.as_ref(),
        full,
        config.conversation.preview_length,
    )?;
    println!(
        "完成: {} 个会话, {} 轮对话, {} 个向量",
        stats.sessions_processed, stats.turns_indexed, stats.vectors_indexed
    );
    if !stats.errors.is_empty() {
        println!("错误:");
        for e in &stats.errors {
            println!("  - {}", e);
        }
        // W4: 破坏性/不完整结果必须以非零退出码告知自动化调用方——否则
        // `rebuild --full && start-gateway` 之类脚本会把"向量索引已清空且无法
        // 重建"或"N 个会话解析失败"当成功。
        anyhow::bail!(
            "rebuild 完成但有 {} 项错误（详见上方列表）",
            stats.errors.len()
        );
    }
    Ok(())
}

pub(crate) fn cmd_import(config: &Config, db: &Db, file: &Path) -> anyhow::Result<()> {
    let (header, turns) = conversation::read_session(file)?;
    // W5: 与 MCP save_session 对齐——空 turns 显式拒绝，不得静默导入空会话。
    if turns.is_empty() {
        anyhow::bail!("导入文件不含任何 turn（turns 为空）");
    }
    validate_session_id(&header.session_id).map_err(|e| anyhow::anyhow!("{e}"))?;
    // 产品决策（与 MCP save_session 对齐）：存储绑定活动 profile，跨 profile
    // 导入须显式 --profile 切换——Overwrite 路径会把 header.profile_id 持久化
    // 进 sessions，静默接受会污染 profile 归属。
    if header.profile_id != config.profile_id {
        anyhow::bail!(
            "profile mismatch: file belongs to '{}', current profile is '{}' (use --profile {} to import)",
            header.profile_id,
            config.profile_id,
            header.profile_id
        );
    }
    for (i, turn) in turns.iter().enumerate() {
        if !VALID_ROLES.contains(&turn.role.as_str()) {
            // M13d: 与 MCP save_session 同文案——列出允许角色，便于调用方自纠。
            anyhow::bail!(
                "turn[{}] role 非法: '{}' (允许: {:?})",
                i,
                turn.role,
                VALID_ROLES
            );
        }
        // W1.3: 与 MCP save_session 同口径——逐轮预校验 ts 格式，坏 ts 拒绝；
        // 下游 session_store 的 unwrap_or(start_ts) 会静默塌缩坏 ts 污染时间窗。
        time::ts_to_unix_ms(&turn.ts)
            .map_err(|e| anyhow::anyhow!("turn[{}] timestamp 无效: {}", i, e))?;
    }
    let conv_dir = config.conversations_dir();
    let store =
        SessionStore::new(&conv_dir, db).with_preview_length(config.conversation.preview_length);
    let embedder = config.create_embedder();
    let stats = store.save(&header, &turns, embedder.as_ref())?;
    // W5: 与 MCP save_session / HTTP capture 同口径——不安全内容软标记进
    // audit_log（security_scan_flag），导入不再绕过 L0 审计。
    for (i, t) in turns.iter().enumerate() {
        crate::growth::audit::flag_unsafe_turn(db, &header.session_id, i, &t.content);
    }
    println!("导入成功: {} ({} 轮)", stats.session_id, stats.turns_saved);
    if stats.vectors_skipped {
        println!("警告: 嵌入服务不可用，本次导入已跳过向量，可稍后运行 `rebuild` 补齐");
    }
    Ok(())
}

pub(crate) fn cmd_export(_config: &Config, db: &Db, session_id: &str) -> anyhow::Result<()> {
    use rusqlite::OptionalExtension;
    let (file_path, start_ts, source, turn_count): (String, i64, Option<String>, i64) = db
        .conn()
        .query_row(
            "SELECT file_path, start_ts, source, turn_count FROM sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            // turn_count 是 `INTEGER DEFAULT 0`（可为 NULL，非 NOT NULL），
            // 与 preview 同类隐患：按 i64 取列遇 NULL 会中途失败。
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                ))
            },
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("会话不存在: {}", session_id))?;

    println!("文件路径: {}", file_path);
    println!("时间: {}", time::unix_ms_to_iso(start_ts));
    println!("来源: {}", source.unwrap_or_else(|| "-".to_string()));
    println!("轮次: {}", turn_count);

    // 输出 turns
    let mut stmt = db.conn().prepare(
        "SELECT seq, timestamp_ms, role, preview FROM turns
         WHERE session_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt.query_map(rusqlite::params![session_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            // M8: preview 列可为 NULL（schema 的 turns_ai/ad/au 触发器专为此设守卫），
            // 按 String 取列会让 export 中途 InvalidColumnType 失败。
            row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        ))
    })?;

    println!("\n--- 对话内容 ---");
    for row in rows {
        let (seq, ts, role, preview) = row?;
        println!(
            "[{}] {} ({}): {}",
            seq,
            role,
            time::unix_ms_to_iso(ts),
            preview
        );
    }

    Ok(())
}

/// 安全删除 turn（在 Rust 进程内处理 FTS/vector 清理，无需外部 tokenize_zh UDF）
pub(crate) fn cmd_delete_turn(db: &Db, turn_id: i64) -> anyhow::Result<()> {
    use rusqlite::OptionalExtension;
    // W5: session_id 在事务内取自 turns 行，消除事务外 EXISTS 的 TOCTOU。
    // RAII IMMEDIATE 事务保护：`turns_ad` 触发器会自动删除 turns_fts 记录，
    // 禁止再手动 INSERT('delete') 触发双重删除破坏 FTS5 索引。
    let tx = db.immediate_tx()?;
    let session_id: Option<String> = tx
        .query_row(
            "SELECT session_id FROM turns WHERE id = ?1",
            rusqlite::params![turn_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(session_id) = session_id else {
        println!("turn {} not found", turn_id);
        return Ok(());
    };
    tx.execute(
        "DELETE FROM vec_turns WHERE rowid = ?1",
        rusqlite::params![turn_id],
    )?;
    let deleted = tx.execute(
        "DELETE FROM turns WHERE id = ?1",
        rusqlite::params![turn_id],
    )?;
    // W5: 按删除后实际行数重算 turn_count，避免 CLI 列表/详情轮次永久偏高。
    if deleted > 0 {
        tx.execute(
            "UPDATE sessions SET turn_count = (SELECT COUNT(*) FROM turns WHERE session_id = ?1)
             WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
    }
    tx.commit()?;

    println!("Deleted turn {} and its FTS/vector indexes", turn_id);
    println!("note: the JSONL source still contains this turn; a full `rebuild` will restore it");
    Ok(())
}
