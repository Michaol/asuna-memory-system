mod config;
mod embedder;
mod fact;
mod graph;
mod growth;
mod index;
mod mcp;
mod memory;
mod model_download;
mod short_term;
mod transport;
mod util;

use clap::Parser;
use std::path::{Path, PathBuf};
use std::rc::Rc;

#[derive(Parser)]
#[command(name = "asuna-memory", version = env!("CARGO_PKG_VERSION"), about = "AI Agent Memory System - MCP Server")]
struct Cli {
    /// 配置文件路径
    #[arg(long, default_value = "~/.asuna/config.json")]
    config: PathBuf,

    /// 指定 profile
    #[arg(long)]
    profile: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand)]
enum Commands {
    /// 启动 MCP stdio 服务器
    Serve,
    /// 测试配置
    Doctor {
        /// 显示图谱覆盖率和悬空引用等额外诊断
        #[arg(long)]
        verbose: bool,
        /// 自动修复 DB/.md 不一致（无损合并 .md 与 SQLite；含多条目行拆分）
        #[arg(long)]
        fix: bool,
        /// 仅拆分含多个 § 分隔条目的 DB 行（修复 .md vs DB 行数不一致，不触发 .md/SQLite 合并）
        #[arg(long)]
        split_entries: bool,
    },
    /// 列出所有 profile
    ListProfiles,
    /// 列出所有会话
    ListSessions {
        /// 最近 N 天
        #[arg(long)]
        last_days: Option<i64>,
        /// 限制数量
        #[arg(long, default_value = "20")]
        limit: usize,
    },
    /// CLI 搜索
    Search {
        /// 搜索查询
        query: String,
        /// 返回数量
        #[arg(long, default_value = "5")]
        top_k: usize,
        /// 搜索模式
        #[arg(long, default_value = "keyword")]
        mode: String,
        /// 按角色过滤（user / assistant）
        #[arg(long)]
        role: Option<String>,
        /// 起始时间过滤（RFC3339，如 2026-01-01T00:00:00Z）
        #[arg(long)]
        after: Option<String>,
        /// 结束时间过滤（RFC3339）
        #[arg(long)]
        before: Option<String>,
        /// 仅搜索最近 N 天（覆盖 --after）
        #[arg(long)]
        last_days: Option<i64>,
    },
    /// 从 JSONL 重建索引
    Rebuild {
        /// Force full rebuild, ignore existing data (default: incremental mode)
        #[arg(long)]
        full: bool,
    },
    /// 导入 JSONL 对话文件
    Import {
        /// 文件路径
        file: PathBuf,
    },
    /// 导出会话
    Export {
        /// 会话 ID
        session_id: String,
    },
    /// 安全删除 turn（自动清理 FTS + 向量索引，无需外部 UDF）
    DeleteTurn {
        /// Turn ID
        id: i64,
    },
    /// 执行只读 SQL 查询（tokenize_zh UDF 在进程内可用）
    Sql {
        /// SQL 查询语句
        query: String,
    },
    /// 下载嵌入模型（从 GitHub Release Assets）
    ModelDownload,
    /// 启动 HTTP REST Gateway（供 Hermes 等 Agent 框架集成）
    Gateway {
        /// 监听端口（0 = 自动分配）
        #[arg(long, default_value = "0")]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    // 自动发现 ORT 动态库 — 必须在任何 ort 调用之前执行
    embedder::init_ort_library_path();

    let cli = Cli::parse();

    // 加载配置
    let config_path = config::expand_tilde(&cli.config);
    let mut config = config::Config::load(&config_path)?;

    // 覆盖 profile
    if let Some(profile) = cli.profile {
        config.profile_id = profile;
    }

    config.ensure_dirs()?;

    tracing::info!("数据目录: {}", config.data_dir.display());
    tracing::info!("Profile: {}", config.profile_id);

    // 打开数据库（按 profile 隔离）
    let db_path = config.profile_db_path();
    let mut db = index::db::Db::open(&db_path)?;
    db.set_dimensions(config.embedding.dimensions);
    db.init_schema()?;
    let db = Rc::new(db);

    tracing::info!("数据库: {}", db_path.display());

    match cli.command {
        Some(Commands::Doctor { verbose, fix, split_entries }) => cmd_doctor(&config, &db, &db_path, verbose, fix, split_entries)?,
        Some(Commands::ListProfiles) => cmd_list_profiles(&config),
        Some(Commands::ListSessions { last_days, limit }) => {
            cmd_list_sessions(&config, &db, last_days, limit)?
        }
        Some(Commands::Search { query, top_k, mode, role, after, before, last_days }) => {
            let filters = SearchFilters { role, after, before, last_days };
            cmd_search(&config, &db, &query, top_k, &mode, filters)?
        }
        Some(Commands::Rebuild { full }) => cmd_rebuild(&config, &db, full)?,
        Some(Commands::Import { file }) => cmd_import(&config, &db, &file)?,
        Some(Commands::Export { session_id }) => cmd_export(&config, &db, &session_id)?,
        Some(Commands::DeleteTurn { id }) => cmd_delete_turn(&db, id)?,
        Some(Commands::Sql { query }) => cmd_sql(&db, &query)?,
        Some(Commands::ModelDownload) => cmd_model_download(&config)?,
        Some(Commands::Gateway { port }) => {
            tracing::info!("启动 HTTP Gateway...");
            let embedder = config.create_embedder();
            let llm = memory::llm::LlmClient::from_config(&config.llm);
            if llm.is_some() {
                tracing::info!("LLM 客户端已配置 ({})", config.llm.model);
            } else {
                tracing::info!("LLM 客户端未配置 (管线将跳过 L1 提取)。设置 AMS_LLM_BASE_URL + AMS_LLM_API_KEY 启用。");
            }
            // Open a new database connection for the gateway (HTTP needs Send+Sync)
            let mut db_gateway = index::db::Db::open(&db_path)?;
            db_gateway.set_dimensions(config.embedding.dimensions);
            db_gateway.init_schema()?;
            transport::http::run_gateway(config, db_gateway, embedder, llm, port).await?;
        }
        Some(Commands::Serve) | None => {
            tracing::info!("启动 MCP stdio 服务器...");
            let server = mcp::server::Server::new(config, db);
            server.run()?;
        }
    }

    Ok(())
}

fn cmd_doctor(
    config: &config::Config,
    db: &index::db::Db,
    db_path: &std::path::Path,
    verbose: bool,
    fix: bool,
    split_entries: bool,
) -> anyhow::Result<()> {
    doctor_print_header(config, db, db_path)?;
    doctor_print_embedder(config);
    doctor_print_limits_and_profiles(config);
    let turn_count = doctor_print_index_stats(db);
    doctor_print_graph_stats(config, db);
    if verbose && config.graph.enabled {
        doctor_print_graph_coverage(db, turn_count);
    }

    // 拆分多条目坏行（独立于 --fix：仅修复 DB 行数与 .md 条目数不一致，
    // 不触发 .md/SQLite 的无损合并）。
    if split_entries {
        doctor_split_entries(config, db)?;
    }

    // Bounded memory DB/.md 一致性检查
    doctor_reconcile_all(config, db, fix)?;

    doctor_print_consistency(config, db)?;

    Ok(())
}

fn doctor_print_header(
    config: &config::Config,
    db: &index::db::Db,
    db_path: &std::path::Path,
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
    let fk_status: i64 = db
        .conn()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap_or(0);
    println!(
        "外键约束: {}",
        if fk_status == 1 { "ON" } else { "OFF (建议升级)" }
    );
    Ok(())
}

fn doctor_print_embedder(config: &config::Config) {
    let model_dir = config.discover_model_dir();
    let api_configured = !config.embedding.api_url.is_empty() && !config.embedding.api_model.is_empty();

    if api_configured {
        doctor_print_api_embedder(config, model_dir.as_deref());
    } else if let Some(ref path) = model_dir {
        doctor_print_local_embedder(path);
    } else {
        println!("嵌入后端: 无");
        println!("嵌入引擎状态: DISABLED");
        println!("  运行 'asuna-memory model-download' 下载嵌入模型 (~300MB)");
        println!("  或在 config.json 中设置 embedding.api_url + api_model 使用第三方 API");
    }
}

fn doctor_print_api_embedder(config: &config::Config, model_dir: Option<&Path>) {
    let fmt = if config.embedding.api_format.is_empty() { "openai" } else { &config.embedding.api_format };
    println!("嵌入后端: API ({} / {}, format={})", config.embedding.api_url, config.embedding.api_model, fmt);
    let embedder = config.create_embedder();
    match embedder {
        Some(ref emb) => doctor_probe_api_embedder(emb, model_dir),
        None => {
            println!("嵌入引擎状态: DISABLED (API 创建失败)");
            if model_dir.is_some() {
                println!("  提示: 本地有 ONNX 模型可用，清除 api_url/api_model 可回退到本地");
            }
        }
    }
}

fn doctor_probe_api_embedder(emb: &embedder::LazyEmbedder, model_dir: Option<&Path>) {
    match emb.embed_query("test") {
        Ok(v) => println!("嵌入引擎状态: OK (API, 维度={})", v.len()),
        Err(e) => {
            println!("嵌入引擎状态: FAILED ({})", e);
            println!("  语义搜索不可用，将降级为关键词搜索");
            if model_dir.is_some() {
                println!("  提示: 本地有 ONNX 模型可用，检查 API 配置或清除 api_url/api_model 回退到本地");
            }
        }
    }
}

fn doctor_print_local_embedder(path: &Path) {
    println!("嵌入后端: 本地 ONNX");
    println!("模型目录: {:?}", path);
    let embedder = embedder::LazyEmbedder::new(path);
    match embedder.embed_query("test") {
        Ok(v) => println!("嵌入引擎状态: OK (维度={})", v.len()),
        Err(e) => {
            println!("嵌入引擎状态: FAILED ({})", e);
            println!("  语义搜索不可用，将降级为关键词搜索");
            if e.to_string().contains("ONNX Runtime") {
                println!("  修复: 设置 LD_LIBRARY_PATH 指向 libonnxruntime.so 所在目录");
                println!("  或设置 ORT_DYLIB_PATH 环境变量指向完整的 .so 文件路径");
            }
            println!("  或在 config.json 中设置 embedding.api_url + api_model 使用第三方 API");
        }
    }
}

fn doctor_print_limits_and_profiles(config: &config::Config) {
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

// 对话统计
fn doctor_print_index_stats(db: &index::db::Db) -> i64 {
    let session_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);
    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap_or(0);
    let vec_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM vec_turns_rowids", [], |r| r.get(0))
        .unwrap_or(0);
    println!(
        "索引统计: {} 会话, {} 轮对话, {} 个向量",
        session_count, turn_count, vec_count
    );
    turn_count
}

// 图谱统计
fn doctor_print_graph_stats(config: &config::Config, db: &index::db::Db) {
    let entity_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM entities", [], |r| r.get(0))
        .unwrap_or(0);
    let relation_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM relations", [], |r| r.get(0))
        .unwrap_or(0);
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

fn doctor_print_graph_coverage(db: &index::db::Db, turn_count: i64) {
    // 覆盖率：有多少 turn 至少被一条 relation 引用
    let covered: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(DISTINCT source_turn) FROM relations
             WHERE source_turn IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let coverage_pct = if turn_count > 0 {
        (covered as f64 / turn_count as f64 * 100.0).round() as i64
    } else {
        0
    };
    // 悬空引用：relations.source_turn 不存在于 turns 表
    let dangling: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(DISTINCT r.source_turn) FROM relations r
             WHERE r.source_turn IS NOT NULL
               AND NOT EXISTS (SELECT 1 FROM turns t WHERE t.id = r.source_turn)",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    println!(
        "图谱覆盖率: {}% ({}/{} turns)",
        coverage_pct, covered, turn_count
    );
    println!("图谱悬空引用: {}", dangling);
}

fn doctor_split_entries(config: &config::Config, db: &index::db::Db) -> anyhow::Result<()> {
    for target in &["memory", "user"] {
        let bm = growth::bounded_memory::BoundedMemory::new(
            &config.memory_dir(),
            db,
            config.memory.memory_char_limit,
            config.memory.user_char_limit,
        )
        .with_security_scan(false);

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

fn doctor_reconcile_all(
    config: &config::Config,
    db: &index::db::Db,
    fix: bool,
) -> anyhow::Result<()> {
    for target in &["memory", "user"] {
        doctor_reconcile_target(config, db, target, fix)?;
    }
    Ok(())
}

fn doctor_reconcile_target(
    config: &config::Config,
    db: &index::db::Db,
    target: &str,
    fix: bool,
) -> anyhow::Result<()> {
    let bm = growth::bounded_memory::BoundedMemory::new(
        &config.memory_dir(),
        db,
        config.memory.memory_char_limit,
        config.memory.user_char_limit,
    )
    .with_security_scan(false);

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
fn doctor_print_consistency(config: &config::Config, db: &index::db::Db) -> anyhow::Result<()> {
    let consistency = index::rebuild::check_consistency(&config.conversations_dir(), db)?;
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

fn cmd_list_profiles(config: &config::Config) {
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

fn cmd_list_sessions(
    _config: &config::Config,
    db: &index::db::Db,
    last_days: Option<i64>,
    limit: usize,
) -> anyhow::Result<()> {
    let (query, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(days) = last_days {
        let cutoff = util::time::now_unix_ms() - days * util::time::MS_PER_DAY;
        (
            "SELECT session_id, start_ts, source, turn_count, title
             FROM sessions WHERE start_ts >= ?1
             ORDER BY start_ts DESC LIMIT ?2"
                .to_string(),
            vec![Box::new(cutoff), Box::new(limit as i64)],
        )
    } else {
        (
            "SELECT session_id, start_ts, source, turn_count, title
             FROM sessions ORDER BY start_ts DESC LIMIT ?1"
                .to_string(),
            vec![Box::new(limit as i64)],
        )
    };

    let mut stmt = db.conn().prepare(&query)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        let ts_ms: i64 = row.get(1)?;
        Ok((
            row.get::<_, String>(0)?,
            util::time::unix_ms_to_iso(ts_ms),
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
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
        println!(
            "{:<38} {:<25} {:<6} {:<15} {}",
            sid,
            &ts[..ts.len().min(25)],
            turns,
            source.unwrap_or_else(|| "-".to_string()),
            title.unwrap_or_else(|| "-".to_string()),
        );
        count += 1;
    }
    println!("\n共 {} 个会话", count);
    Ok(())
}

/// Optional turn-search filters for `cmd_search`.
struct SearchFilters {
    role: Option<String>,
    after: Option<String>,
    before: Option<String>,
    last_days: Option<i64>,
}

fn cmd_search(
    config: &config::Config,
    db: &index::db::Db,
    query: &str,
    top_k: usize,
    mode: &str,
    filters: SearchFilters,
) -> anyhow::Result<()> {
    let search_mode = match mode {
        "semantic" | "vector" => fact::search::SearchMode::Semantic,
        "keyword" | "fts" => fact::search::SearchMode::Keyword,
        _ => fact::search::SearchMode::Hybrid,
    };

    // 自动发现并创建嵌入器
    let embedder = config.create_embedder();

    // 时间过滤：--last-days 覆盖 --after；时间戳解析失败直接报错而非静默忽略
    let after_ms = match filters.after.as_deref() {
        Some(s) => Some(util::time::ts_to_unix_ms(s)?),
        None => None,
    };
    let before_ms = match filters.before.as_deref() {
        Some(s) => Some(util::time::ts_to_unix_ms(s)?),
        None => None,
    };
    let effective_after = if let Some(days) = filters.last_days {
        let days = days.clamp(0, 36_500);
        Some(util::time::now_unix_ms() - days * util::time::MS_PER_DAY)
    } else {
        after_ms
    };

    let params = fact::search::SearchParams {
        query: query.to_string(),
        search_mode,
        top_k,
        after_ms: effective_after,
        before_ms,
        role: filters.role,
    };

    let results = fact::search::search_sessions(db, embedder.as_ref(), &params)?;

    println!("搜索: \"{}\" (mode={})", query, mode);
    println!();
    for (i, r) in results.iter().enumerate() {
        let ts = util::time::unix_ms_to_iso(r.timestamp_ms);
        println!(
            "[{}] score={:.4} | {} | {} | {}",
            i + 1,
            r.score,
            &ts[..19],
            r.role,
            r.session_id,
        );
        println!("    {}\n", r.preview);
    }
    println!("共 {} 条结果", results.len());
    Ok(())
}

fn cmd_rebuild(config: &config::Config, db: &index::db::Db, full: bool) -> anyhow::Result<()> {
    println!("从 JSONL 重建索引{}...", if full { "（完整模式）" } else { "（增量模式）" });
    let embedder = config.create_embedder();
    let stats =
        index::rebuild::rebuild_from_jsonl(&config.conversations_dir(), db, embedder.as_ref(), full)?;
    println!(
        "完成: {} 个会话, {} 轮对话, {} 个向量",
        stats.sessions_processed, stats.turns_indexed, stats.vectors_indexed
    );
    if !stats.errors.is_empty() {
        println!("错误:");
        for e in &stats.errors {
            println!("  - {}", e);
        }
    }
    Ok(())
}

fn cmd_import(config: &config::Config, db: &index::db::Db, file: &Path) -> anyhow::Result<()> {
    let (header, turns) = fact::conversation::read_session(file)?;
    let conv_dir = config.conversations_dir();
    let store = fact::session_store::SessionStore::new(&conv_dir, db);
    let embedder = config.create_embedder();
    let stats = store.save(&header, &turns, embedder.as_ref())?;
    println!("导入成功: {} ({} 轮)", stats.session_id, stats.turns_saved);
    Ok(())
}

fn cmd_export(
    _config: &config::Config,
    db: &index::db::Db,
    session_id: &str,
) -> anyhow::Result<()> {
    let file_path: String = db
        .conn()
        .query_row(
            "SELECT file_path FROM sessions WHERE session_id = ?1",
            rusqlite::params![session_id],
            |r| r.get(0),
        )
        .map_err(|_| anyhow::anyhow!("会话不存在: {}", session_id))?;

    // file_path 是相对于 data_dir 的，但这里我们直接输出文件内容
    println!("文件路径: {}", file_path);

    // 从 sessions 表读取元数据
    let (start_ts, source, turn_count): (i64, Option<String>, i64) = db.conn().query_row(
        "SELECT start_ts, source, turn_count FROM sessions WHERE session_id = ?1",
        rusqlite::params![session_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    println!("时间: {}", util::time::unix_ms_to_iso(start_ts));
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
            row.get::<_, String>(3)?,
        ))
    })?;

    println!("\n--- 对话内容 ---");
    for row in rows {
        let (seq, ts, role, preview) = row?;
        println!(
            "[{}] {} ({}): {}",
            seq,
            role,
            util::time::unix_ms_to_iso(ts),
            preview
        );
    }

    Ok(())
}

/// 安全删除 turn（在 Rust 进程内处理 FTS/vector 清理，无需外部 tokenize_zh UDF）
fn cmd_delete_turn(db: &index::db::Db, turn_id: i64) -> anyhow::Result<()> {
    use rusqlite::OptionalExtension;
    let conn = db.conn();

    let preview: Option<String> = conn
        .query_row(
            "SELECT preview FROM turns WHERE id = ?1",
            rusqlite::params![turn_id],
            |r| r.get(0),
        )
        .optional()?;

    let preview = match preview {
        Some(p) => p,
        None => {
            println!("turn {} not found", turn_id);
            return Ok(());
        }
    };

    // 事务保护：三步操作原子执行，任一步失败则全部回滚（1.3 fix）
    conn.execute_batch("BEGIN IMMEDIATE")?;

    // 1. 手动删除 FTS 条目（contentless FTS 的 delete 命令）
    // jieba tokenizer 在 FTS5 引擎内自动分词，直接传原始 preview
    if let Err(e) = conn.execute(
        "INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', ?1, ?2)",
        rusqlite::params![turn_id, preview],
    ) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e.into());
    }

    // 2. 删除向量索引
    if let Err(e) = conn.execute(
        "DELETE FROM vec_turns WHERE rowid = ?1",
        rusqlite::params![turn_id],
    ) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e.into());
    }

    // 3. 删除 turn（触发器会触发但 tokenize_zh 在进程内可用）
    if let Err(e) = conn.execute(
        "DELETE FROM turns WHERE id = ?1",
        rusqlite::params![turn_id],
    ) {
        let _ = conn.execute_batch("ROLLBACK");
        return Err(e.into());
    }

    conn.execute_batch("COMMIT")?;
    println!("Deleted turn {} and its FTS/vector indexes", turn_id);
    Ok(())
}

/// 只读 SQL 查询（拒绝写操作，tokenize_zh UDF 在进程内可用）
fn cmd_sql(db: &index::db::Db, query: &str) -> anyhow::Result<()> {
    // 首 token 白名单：只放行明确的只读语句，避免 denylist 漏掉
    // REPLACE / 可写 PRAGMA / VACUUM / REINDEX 等写操作。
    let q_upper = query.trim().to_uppercase();
    let first_token = q_upper.split(|c: char| !c.is_alphanumeric()).next().unwrap_or("");
    if !matches!(first_token, "SELECT" | "PRAGMA" | "EXPLAIN" | "WITH") {
        anyhow::bail!("safety: sql subcommand only allows read queries (SELECT/PRAGMA/EXPLAIN/WITH)");
    }

    // 引擎级只读强制：SQLite 在 query_only=ON 下拒绝一切写操作（REPLACE、可写 PRAGMA、
    // VACUUM、写触发器、CTE 内写等），不受语句拼写花样影响。
    db.conn().execute_batch("PRAGMA query_only = ON;")?;

    let result = (|| -> anyhow::Result<()> {
        let mut stmt = db.conn().prepare(query)?;
        let cols: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let col_count = cols.len();
        println!("{}", cols.join("\t"));
        println!("{}", "-".repeat(col_count * 20));

        let rows = stmt.query_map([], |row| {
            let mut vals = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let v: rusqlite::Result<String> = row.get(i);
                vals.push(v.unwrap_or_else(|_| "NULL".to_string()));
            }
            Ok(vals)
        })?;

        let mut count = 0;
        for row in rows {
            let vals = row?;
            println!("{}", vals.join("\t"));
            count += 1;
        }
        println!("\n{} rows", count);
        Ok(())
    })();

    // 恢复连接状态（CLI 进程随后退出，但保持连接干净）
    let _ = db.conn().execute_batch("PRAGMA query_only = OFF;");
    result
}

fn cmd_model_download(config: &config::Config) -> anyhow::Result<()> {
    let dest = config.model_dir();

    if model_download::model_check(&dest) {
        println!("模型已存在: {}", dest.display());
        return Ok(());
    }

    println!(
        "下载 EmbeddingGemma 模型 ({} 个文件，~300MB)...",
        model_download::MODEL_FILES.len()
    );
    println!("来源: GitHub Release v{}", env!("CARGO_PKG_VERSION"));
    println!("目标: {}", dest.display());
    println!();

    model_download::download_model(&dest, Some(|p: f64| {
        let filled = (p * 20.0) as usize;
        let bar: String = "=".repeat(filled)
            + &" ".repeat(20_usize.saturating_sub(filled));
        print!("\r总进度: [{bar}] {:.0}%", p * 100.0);
        std::io::Write::flush(&mut std::io::stdout()).ok();
    }))?;

    println!("\n下载完成！运行 'asuna-memory doctor' 验证嵌入引擎。");
    Ok(())
}
