mod config;
mod embedder;
mod fact;
mod growth;
mod index;
mod mcp;
mod util;

use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

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
    Doctor,
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
    },
    /// 从 JSONL 重建索引
    Rebuild,
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
    let db = Arc::new(index::db::Db::open(&db_path)?);
    db.init_schema()?;

    tracing::info!("数据库: {}", db_path.display());

    match cli.command {
        Some(Commands::Doctor) => cmd_doctor(&config, &db, &db_path)?,
        Some(Commands::ListProfiles) => cmd_list_profiles(&config),
        Some(Commands::ListSessions { last_days, limit }) => {
            cmd_list_sessions(&config, &db, last_days, limit)?
        }
        Some(Commands::Search { query, top_k, mode }) => cmd_search(&db, &query, top_k, &mode)?,
        Some(Commands::Rebuild) => cmd_rebuild(&config, &db)?,
        Some(Commands::Import { file }) => cmd_import(&config, &db, &file)?,
        Some(Commands::Export { session_id }) => cmd_export(&config, &db, &session_id)?,
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
) -> anyhow::Result<()> {
    println!("=== Asuna Memory Doctor ===");
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
    println!("模型目录: {:?}", config.discover_model_dir());
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

    // 对话统计
    let session_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))
        .unwrap_or(0);
    let turn_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0))
        .unwrap_or(0);
    println!("索引统计: {} 会话, {} 轮对话", session_count, turn_count);

    // 一致性检查
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
        let cutoff = util::time::now_unix_ms() - days * 86400000;
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
        "{:<38} {:<25} {:<6} {:<15} {}",
        "SESSION_ID", "TIME", "TURNS", "SOURCE", "TITLE"
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

fn cmd_search(db: &index::db::Db, query: &str, top_k: usize, mode: &str) -> anyhow::Result<()> {
    let search_mode = match mode {
        "semantic" => fact::search::SearchMode::Semantic,
        "keyword" => fact::search::SearchMode::Keyword,
        _ => fact::search::SearchMode::Hybrid,
    };

    let params = fact::search::SearchParams {
        query: query.to_string(),
        search_mode,
        top_k,
        after_ms: None,
        before_ms: None,
        role: None,
    };

    let results = fact::search::search_sessions(db, None, &params)?;

    println!("搜索: \"{}\" (mode={})\n", query, mode);
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

fn cmd_rebuild(config: &config::Config, db: &index::db::Db) -> anyhow::Result<()> {
    println!("从 JSONL 重建索引...");
    let stats = index::rebuild::rebuild_from_jsonl(&config.conversations_dir(), db)?;
    println!(
        "完成: {} 个会话, {} 轮对话",
        stats.sessions_processed, stats.turns_indexed
    );
    if !stats.errors.is_empty() {
        println!("错误:");
        for e in &stats.errors {
            println!("  - {}", e);
        }
    }
    Ok(())
}

fn cmd_import(config: &config::Config, db: &index::db::Db, file: &PathBuf) -> anyhow::Result<()> {
    let (header, turns) = fact::conversation::read_session(file)?;
    let conv_dir = config.conversations_dir();
    let store = fact::session_store::SessionStore::new(&conv_dir, db);
    let stats = store.save(&header, &turns)?;
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
