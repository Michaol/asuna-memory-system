mod graph_handlers;
mod handlers;
mod schema;

pub use schema::tool_definitions;

use crate::config::Config;
use crate::index::db::Db;
use serde_json::Value;
use std::rc::Rc;
use std::sync::Arc;

/// 工具调用处理器
pub struct ToolHandler {
    pub(super) config: Config,
    pub(super) db: Rc<Db>,
    /// MCP 是单线程服务器（`Rc<Db>`），嵌入器为轻量在线 API 配置句柄
    /// （`Arc<LazyEmbedder>`，与 backfill 线程共享同一实例）。
    pub(super) embedder: Option<Arc<crate::embedder::LazyEmbedder>>,
    pub(super) rebuild_progress: crate::index::rebuild::SharedProgress,
    pub(super) shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// C8: 启动回填与 rebuild_index 共用的执行锁——两者都会整段写 SQLite，
    /// 串行化杜绝并发写（rebuild 清空重填会吞掉回填刚写入的行）。
    pub(super) exec_lock: Arc<std::sync::Mutex<()>>,
    /// C8: 回填线程句柄——Drop 置位 shutdown 后 join，等待干净事务边界。
    backfill_handle: Option<std::thread::JoinHandle<()>>,
}

/// C8: 带 shutdown 轮询地获取执行锁——Drop 置位 shutdown 后即使锁被 rebuild
/// 长期持有也能及时退出（join 不会卡死）；poisoned 锁按 NEW-8 口径恢复。
fn acquire_exec_lock<'a>(
    lock: &'a std::sync::Mutex<()>,
    stop: &std::sync::atomic::AtomicBool,
) -> Option<std::sync::MutexGuard<'a, ()>> {
    loop {
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        match lock.try_lock() {
            Ok(g) => return Some(g),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(std::sync::TryLockError::Poisoned(p)) => {
                lock.clear_poison();
                return Some(p.into_inner());
            }
        }
    }
}

impl ToolHandler {
    pub fn new(config: Config, db: Rc<Db>) -> Self {
        // M13: 只构建一次 LazyEmbedder，backfill 线程共享同一 Arc，避免重复的
        // provider info 日志与多建一个 ureq Agent。
        let embedder = config.create_embedder().map(Arc::new);
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let exec_lock = Arc::new(std::sync::Mutex::new(()));

        let backfill_handle = embedder.clone().map(|embedder| {
            let db_path = config.profile_db_path();
            let profile_dir = config.profile_dir();
            let dimensions = config.embedding.dimensions;
            let stop = shutdown.clone();
            let lock = exec_lock.clone();
            // C8: 保留 JoinHandle——stdio EOF / Drop 置位 shutdown 并 join，
            // 等当前 chunk 事务边界干净退出，不再 mid-write detach 线程；
            // 未完成 chunk 下次启动继续补齐。
            std::thread::spawn(move || {
                let Some(_guard) = acquire_exec_lock(&lock, &stop) else {
                    return;
                };
                // STEP 6 跨进程成本守卫：另一进程正在回填本 profile（Contended）
                // 则跳过——持有者的 drain loop 会把本进程 pending atom 一并索引；
                // 锁文件打开/加锁失败（Degraded）则 warn 后不加守卫继续，绝不阻塞启动。
                let _backfill_guard =
                    match crate::index::db::try_acquire_backfill_lock(&profile_dir) {
                        crate::index::db::BackfillLockOutcome::Acquired(f) => Some(f),
                        crate::index::db::BackfillLockOutcome::Contended => {
                            tracing::info!(
                                "vec_bounded_memory backfill skipped: another process holds backfill.lock"
                            );
                            return;
                        }
                        crate::index::db::BackfillLockOutcome::Degraded => None,
                    };
                let mut db = match Db::open(&db_path) {
                    Ok(db) => db,
                    Err(e) => {
                        tracing::warn!("vec_bounded_memory backfill: 打开 DB 失败: {}", e);
                        return;
                    }
                };
                db.set_dimensions(dimensions);
                if let Err(e) = db.init_schema() {
                    tracing::warn!("vec_bounded_memory backfill: schema 初始化失败: {}", e);
                    return;
                }
                if let Err(e) = db.backfill_bounded_memory_vec_with_control(&embedder, Some(&stop))
                {
                    tracing::warn!("vec_bounded_memory backfill skipped: {}", e);
                }
            })
        });

        Self {
            config,
            db,
            embedder,
            rebuild_progress: crate::index::rebuild::new_shared_progress(),
            shutdown,
            exec_lock,
            backfill_handle,
        }
    }

    /// MCP 规范（tools / Error Handling，context7 核验 2026-07-28 schema）：
    /// 未知工具名属协议层错误（JSON-RPC -32602），区别于工具执行错误
    /// （content+isError）。server.rs 分发前用本函数判定；名单以
    /// tool_definitions 为单一真相源，避免 schema 与路由两处维护漂移。
    pub fn is_known_tool(name: &str) -> bool {
        schema::tool_definitions()
            .iter()
            .any(|t| t["name"].as_str() == Some(name))
    }

    /// C6: 重建在写事务内清空 turns/sessions 并按 JSONL 快照重填。preserve-fresh 已
    /// 保证窗口期提交者不被陈旧 parse 覆盖（§8.8，含 BEGIN-crosses-t0 的文件水位判据），
    /// 「窗口内写入必丢」不再成立；重建期间拒绝写类工具是为避免与全局清表+重填争用
    /// 写锁/FTS（读类工具不受影响），沿用 rebuild-vs-rebuild 的 already_running 姿势。
    fn reject_writes_during_rebuild(&self, name: &str, args: &Value) -> Result<(), String> {
        let is_write = match name {
            "save_session"
            | "memory_write"
            | "memory_update"
            | "memory_remove"
            | "graph_assert"
            | "graph_link_entity"
            | "graph_prune_dangling" => true,
            "user_profile" => args.get("action").and_then(Value::as_str) != Some("read"),
            _ => false,
        };
        if !is_write {
            return Ok(());
        }
        let p = crate::util::recover_poison(&self.rebuild_progress);
        if p.status == crate::index::rebuild::RebuildStatus::Running {
            return Err(format!(
                "重建进行中，写类工具 {name} 暂不可用：重建正在清空并按 JSONL 快照重填索引，\
                 此刻写入会与清填争用写锁。请用 rebuild_status 查询进度，完成后重试。"
            ));
        }
        Ok(())
    }

    /// 路由工具调用
    pub fn call(&self, name: &str, args: &Value) -> Result<Value, String> {
        self.reject_writes_during_rebuild(name, args)?;
        match name {
            "save_session" => self.save_session(args),
            "search_sessions" => self.search_sessions(args),
            "memory_write" => self.memory_write(args),
            "memory_update" => self.memory_update(args),
            "memory_remove" => self.memory_remove(args),
            "memory_read" => self.memory_read(args),
            "user_profile" => self.user_profile(args),
            "memory_provenance" => self.memory_provenance(args),
            "rebuild_index" => self.rebuild_index(),
            "rebuild_status" => self.rebuild_status(),
            "graph_assert" => self.graph_assert(args),
            "graph_neighbors" => self.graph_neighbors(args),
            "graph_path" => self.graph_path(args),
            "graph_link_entity" => self.graph_link_entity(args),
            "graph_prune_dangling" => self.graph_prune_dangling(args),
            // 防御兜底：server.rs 已在协议层拦截未知名（-32602）；直接调用
            // call 的路径（测试等）仍得到工具层错误而非 panic。
            _ => Err(format!("未知工具: {}", name)),
        }
    }
}

impl Drop for ToolHandler {
    fn drop(&mut self) {
        self.shutdown
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // C8: join 回填线程——shutdown 在 chunk 边界被检查，线程在干净事务
        // 边界退出后才释放，EOF/关机不再 mid-write detach。
        if let Some(handle) = self.backfill_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests;
