use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "asuna-memory",
    version = env!("CARGO_PKG_VERSION"),
    about = "AI Agent Memory System - MCP Server"
)]
pub(crate) struct Cli {
    /// 配置文件路径
    #[arg(long, default_value = "~/.asuna/config.json", global = true)]
    pub(crate) config: PathBuf,

    /// 指定 profile
    #[arg(long, global = true)]
    pub(crate) profile: Option<String>,

    #[command(subcommand)]
    pub(crate) command: Option<Commands>,
}

#[derive(clap::Subcommand)]
pub(crate) enum Commands {
    /// 启动 MCP stdio 服务器
    Serve,
    /// 诊断配置、索引与 DB/.md 数据一致性（--verbose 额外诊断，--fix / --split_entries 修复）
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
        /// 返回数量（省略时用 config.search.default_top_k）
        #[arg(long)]
        top_k: Option<usize>,
        /// 搜索模式（hybrid / semantic / keyword；兼容别名 vector=semantic、fts=keyword）
        /// 省略时用 config.search.search_mode
        #[arg(long)]
        mode: Option<String>,
        /// 按角色过滤（user / assistant / tool_call / system）
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
    /// 导出会话（仅输出 DB 中的 preview 摘要，非 JSONL 全文）
    Export {
        /// 会话 ID
        session_id: String,
    },
    /// 安全删除 turn（自动清理 FTS + 向量索引，无需外部 UDF）。
    /// 注意：JSONL 源文件不会同步修改——之后的 `rebuild`（完整重建）会以
    /// JSONL 为真相源恢复该 turn。如需永久删除，请同时从 JSONL 中移除。
    DeleteTurn {
        /// Turn ID
        id: i64,
    },
    /// 执行只读 SQL 查询（tokenize_zh UDF 在进程内可用）
    Sql {
        /// SQL 查询语句
        query: String,
    },
    /// 启动 HTTP REST Gateway（供 Hermes 等 Agent 框架集成）
    Gateway {
        /// 监听端口（0 = 自动分配）
        #[arg(long, default_value = "0")]
        port: u16,
    },
}

/// Optional turn-search filters for `cmd_search`.
pub(crate) struct SearchFilters {
    pub(crate) role: Option<String>,
    pub(crate) after: Option<String>,
    pub(crate) before: Option<String>,
    pub(crate) last_days: Option<i64>,
}
