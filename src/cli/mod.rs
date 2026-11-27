mod args;
mod data_ops;
mod doctor;
mod sql;

use args::{Cli, Commands, SearchFilters};
use clap::Parser;
use std::path::PathBuf;
use std::rc::Rc;

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();
}

fn load_configured_profile(cli: &Cli) -> anyhow::Result<crate::config::Config> {
    let config_path = crate::config::expand_tilde(&cli.config);
    let mut config = crate::config::Config::load(&config_path)?;
    if let Some(ref profile) = cli.profile {
        if let Err(msg) = crate::util::validate_profile_id(profile) {
            anyhow::bail!("invalid --profile '{}': {}", profile, msg);
        }
        config.profile_id = profile.clone();
    }
    Ok(config)
}

fn open_writable_profile_db(
    config: &crate::config::Config,
) -> anyhow::Result<(crate::index::db::Db, PathBuf)> {
    config.ensure_dirs()?;
    tracing::info!(target: "asuna_memory", "数据目录: {}", config.data_dir.display());
    tracing::info!(target: "asuna_memory", "Profile: {}", config.profile_id);

    let db_path = config.profile_db_path();
    let mut db = crate::index::db::Db::open(&db_path)?;
    db.set_dimensions(config.embedding.dimensions);
    db.init_schema()?;
    tracing::info!(target: "asuna_memory", "数据库: {}", db_path.display());
    Ok((db, db_path))
}

fn run_gateway_command(
    config: crate::config::Config,
    db: crate::index::db::Db,
    port: u16,
) -> anyhow::Result<()> {
    tracing::info!(target: "asuna_memory", "启动 HTTP Gateway...");
    let embedder = config.create_embedder();
    let llm = crate::memory::llm::LlmClient::from_config(&config.llm);
    if llm.is_some() {
        tracing::info!(target: "asuna_memory", "LLM 客户端已配置 ({})", config.llm.model);
    } else {
        tracing::info!(
            target: "asuna_memory",
            "LLM 客户端未配置 (管线将跳过 L1 提取)。设置 AMS_LLM_BASE_URL + AMS_LLM_API_KEY 启用。"
        );
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = rt.block_on(crate::transport::http::run_gateway(
        config, db, embedder, llm, port,
    ));
    // R2: 限定等待在途 spawn_blocking 工作（如启动回填分片正处网络调用）的时间，
    // 使 SIGTERM/Ctrl+C 在容器 grace 期内退出，而非 runtime drop 无限阻塞。
    rt.shutdown_timeout(std::time::Duration::from_secs(10));
    result
}

/// CLI entrypoint called by `src/main.rs`.
pub fn run() -> anyhow::Result<()> {
    init_tracing();
    dispatch(Cli::parse())
}

/// 解析后的实际分发（从 run 提取，便于单元测试直接构造 Cli 验证副作用）。
fn dispatch(cli: Cli) -> anyhow::Result<()> {
    let config = load_configured_profile(&cli)?;

    // M2: sql 子命令使用严格只读连接与 Authorizer，不创建缺省数据库或触碰 WAL/schema
    if let Some(Commands::Sql { query }) = cli.command {
        let db_path = config.profile_db_path();
        let ro_db = crate::index::db::Db::open_readonly(&db_path)?;
        return sql::cmd_sql(&ro_db, &query);
    }

    // W12: list-profiles 是纯只读 listing——在 open_writable_profile_db **之前**
    // 分发，空/不存在的数据目录上零目录/DB 创建副作用。
    if let Some(Commands::ListProfiles) = cli.command {
        data_ops::cmd_list_profiles(&config);
        return Ok(());
    }

    let (db, db_path) = open_writable_profile_db(&config)?;

    match cli.command {
        // M4: Gateway 独占单连接与按需创建的 Tokio 运行时；CLI / MCP 无运行时与双连接开销
        Some(Commands::Gateway { port }) => run_gateway_command(config, db, port),
        Some(Commands::Doctor {
            verbose,
            fix,
            split_entries,
        }) => doctor::cmd_doctor(&config, &db, &db_path, verbose, fix, split_entries),
        Some(Commands::ListProfiles) => {
            // `Commands::ListProfiles` returned above before opening a writable DB.
            Ok(())
        }
        Some(Commands::ListSessions { last_days, limit }) => {
            data_ops::cmd_list_sessions(&config, &db, last_days, limit)
        }
        Some(Commands::Search {
            query,
            top_k,
            mode,
            role,
            after,
            before,
            last_days,
        }) => {
            let filters = SearchFilters {
                role,
                after,
                before,
                last_days,
            };
            // M3: 省略 --top-k / --mode 时读 config.search.*，与 MCP search_sessions
            // 同口径（此前 CLI 硬编码 5 / keyword，同一配置两个入口默认值不同）。
            let top_k = top_k.unwrap_or(config.search.default_top_k);
            let mode = mode.unwrap_or_else(|| config.search.search_mode.clone());
            data_ops::cmd_search(&config, &db, &query, top_k, &mode, filters)
        }
        Some(Commands::Rebuild { full }) => data_ops::cmd_rebuild(&config, &db, full),
        Some(Commands::Import { file }) => data_ops::cmd_import(&config, &db, &file),
        Some(Commands::Export { session_id }) => data_ops::cmd_export(&config, &db, &session_id),
        Some(Commands::DeleteTurn { id }) => data_ops::cmd_delete_turn(&db, id),
        Some(Commands::Serve) | None => {
            tracing::info!(target: "asuna_memory", "启动 MCP stdio 服务器...");
            let server = crate::mcp::server::Server::new(config, Rc::new(db));
            server.run()
        }
        // `Commands::Sql` returned above before opening a writable DB.
        Some(Commands::Sql { .. }) => Ok(()),
    }
}

#[cfg(test)]
mod tests;
