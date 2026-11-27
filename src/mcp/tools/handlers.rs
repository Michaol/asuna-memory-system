use super::ToolHandler;
use crate::fact::conversation::{SessionHeader, Turn};
use crate::fact::search::SearchMode;
use crate::fact::session_store::SessionStore;
use crate::growth::bounded_memory::BoundedMemory;
use serde_json::{json, Value};
use std::str::FromStr;

/// `search_sessions` 的 time_range 参数。用 serde 强类型对齐 HTTP /search
/// 的 after/before/last_days 契约：错类型报错，省略或 null 走默认（无过滤）。
#[derive(serde::Deserialize)]
struct SearchTimeRange {
    after: Option<String>,
    before: Option<String>,
    last_days: Option<i64>,
}

/// W4.2: memory_write/update/remove 的可选 session_id 与 save_session /
/// HTTP /capture 同口径校验——直入 source_session/audit_log，serde 仅保证
/// 类型，超长/控制字符/路径分隔符必须在入口拒绝。
fn validate_optional_session_id(session_id: Option<&str>) -> Result<(), String> {
    match session_id {
        Some(sid) => {
            crate::util::validate_session_id(sid).map_err(|e| format!("invalid session_id: {}", e))
        }
        None => Ok(()),
    }
}

fn mark_rebuild_failed(progress: &crate::index::rebuild::SharedProgress, msg: String) {
    let mut p = crate::util::recover_poison(progress);
    p.status = crate::index::rebuild::RebuildStatus::Failed;
    // N4: 追加而非覆盖——失败/panic 前累积的逐文件错误必须保留。
    p.errors.push(msg);
    p.finished_at = Some(crate::util::time::now_unix_ms());
}

struct RebuildJob {
    conversations_dir: std::path::PathBuf,
    db_path: std::path::PathBuf,
    dimensions: usize,
    embedder: Option<std::sync::Arc<crate::embedder::LazyEmbedder>>,
    preview_length: usize,
    progress: crate::index::rebuild::SharedProgress,
    /// C8: 与启动回填共用的执行锁（见 ToolHandler::exec_lock）。
    exec_lock: std::sync::Arc<std::sync::Mutex<()>>,
}

impl RebuildJob {
    fn run(self) {
        // C8: 先拿执行锁再动 DB——rebuild 清空 turns/sessions 并按快照重填，
        // 与回填并发写会互相吞行。poisoned 锁按 NEW-8 口径恢复。
        let _guard = crate::util::recover_poison(&self.exec_lock);
        // init 段一并纳入 catch_unwind：任何 panic 都复位进度，不留 Running。
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.init_and_rebuild()));
        handle_rebuild_outcome(&self.progress, result);
    }

    fn init_and_rebuild(&self) -> anyhow::Result<()> {
        let mut db = match crate::index::db::Db::open(&self.db_path) {
            Ok(db) => db,
            Err(e) => {
                mark_rebuild_failed(&self.progress, format!("db: {}", e));
                return Ok(());
            }
        };
        db.set_dimensions(self.dimensions);
        if let Err(e) = db.init_schema() {
            mark_rebuild_failed(&self.progress, format!("schema: {}", e));
            return Ok(());
        }
        crate::index::rebuild::rebuild_from_jsonl_with_progress_and_preview(
            &self.conversations_dir,
            &db,
            self.embedder.as_deref(),
            &self.progress,
            false,
            self.preview_length,
        )?;
        Ok(())
    }
}

fn handle_rebuild_outcome(
    progress: &crate::index::rebuild::SharedProgress,
    result: std::thread::Result<anyhow::Result<()>>,
) {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::error!("rebuild failed: {}", e);
            let still_running = crate::util::recover_poison(progress).status
                == crate::index::rebuild::RebuildStatus::Running;
            if still_running {
                mark_rebuild_failed(progress, format!("rebuild: {}", e));
            }
        }
        Err(panic_payload) => {
            let msg = panic_payload_message(panic_payload.as_ref());
            mark_rebuild_failed(progress, format!("panic: {}", msg));
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(ToString::to_string))
        .unwrap_or_else(|| "unknown panic".to_string())
}

fn parse_session_header(
    args: &Value,
    session_id: &str,
    turns_arr: &[Value],
    expected_profile: &str,
) -> Result<SessionHeader, String> {
    // NEW-1-mcp: 可选元数据字段同 N1/N5 严格化——显式错类型报错而非静默回落
    // （省略/null 仍走默认；schema.rs 声明 string/array）。
    let source: Option<String> =
        serde_json::from_value(args.get("source").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid source: {}", e))?;
    let title: Option<String> =
        serde_json::from_value(args.get("title").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid title: {}", e))?;
    let tags: Vec<String> = serde_json::from_value::<Option<Vec<String>>>(
        args.get("tags").cloned().unwrap_or(Value::Null),
    )
    .map_err(|e| format!("invalid tags: {}", e))?
    .unwrap_or_default();
    let profile: Option<String> =
        serde_json::from_value(args.get("profile").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid profile: {}", e))?;
    if profile.as_deref().is_some_and(|p| p != expected_profile) {
        return Err(
            "profile override not supported; start the server with --profile <id>".to_string(),
        );
    }
    let first_turn_ts = turns_arr
        .first()
        .and_then(|t| t["timestamp"].as_str())
        .unwrap_or("");
    Ok(SessionHeader {
        v: 1,
        header_type: "session_header".to_string(),
        session_id: session_id.to_string(),
        start_time: first_turn_ts.to_string(),
        profile_id: expected_profile.to_string(),
        source,
        agent_model: None,
        title,
        tags,
    })
}

fn parse_session_turn(i: usize, t: &Value) -> Result<Turn, String> {
    let ts = t["timestamp"]
        .as_str()
        .ok_or_else(|| format!("turn[{}] 缺少 timestamp", i))?
        .to_string();
    // W1.3: 逐轮预校验 ts 格式（与 HTTP /capture 的 400 同口径）
    crate::util::time::ts_to_unix_ms(&ts)
        .map_err(|e| format!("turn[{}] timestamp 无效: {}", i, e))?;
    let role = t["role"]
        .as_str()
        .ok_or_else(|| format!("turn[{}] 缺少 role", i))?;
    if !crate::util::VALID_ROLES.contains(&role) {
        return Err(format!(
            "turn[{}] role 非法: '{}' (允许: {:?})",
            i,
            role,
            crate::util::VALID_ROLES
        ));
    }
    let content = t["content"]
        .as_str()
        .ok_or_else(|| format!("turn[{}] 缺少 content", i))?
        .to_string();
    // N5: schema 声明 metadata 为 object。错类型在此拒绝，null 视同省略。
    let metadata = match t.get("metadata") {
        None | Some(Value::Null) => None,
        Some(v) if v.is_object() => Some(v.clone()),
        Some(_) => return Err(format!("turn[{}] metadata 必须为 object", i)),
    };
    Ok(Turn {
        ts,
        seq: (i + 1) as u32,
        role: role.to_string(),
        content,
        metadata,
    })
}

impl ToolHandler {
    pub(super) fn save_session(&self, args: &Value) -> Result<Value, String> {
        const MAX_MCP_TURNS_PER_BATCH: usize = 1000;
        let session_id = args["session_id"].as_str().ok_or("缺少 session_id")?;
        crate::util::validate_session_id(session_id).map_err(ToString::to_string)?;
        let turns_arr = args["turns"].as_array().ok_or("缺少 turns")?;

        if turns_arr.is_empty() {
            return Err("turns 数组不能为空".to_string());
        }
        if turns_arr.len() > MAX_MCP_TURNS_PER_BATCH {
            return Err(format!(
                "单批 turns 数量过多（最多 {} 条）",
                MAX_MCP_TURNS_PER_BATCH
            ));
        }

        let header = parse_session_header(args, session_id, turns_arr, &self.config.profile_id)?;
        let turns: Vec<Turn> = turns_arr
            .iter()
            .enumerate()
            .map(|(i, t)| parse_session_turn(i, t))
            .collect::<Result<_, _>>()?;

        let conv_dir = self.config.conversations_dir();
        let store = SessionStore::new(&conv_dir, &self.db)
            .with_preview_length(self.config.conversation.preview_length);
        let stats = store
            .save(&header, &turns, self.embedder.as_deref())
            .map_err(|e| format!("保存失败: {}", e))?;

        for (i, t) in turns.iter().enumerate() {
            crate::growth::audit::flag_unsafe_turn(&self.db, session_id, i, &t.content);
        }

        let mut response = json!({
            "status": "ok",
            "session_id": stats.session_id,
            "file_path": stats.file_path.to_string_lossy(),
            "turns_saved": stats.turns_saved
        });

        if stats.vectors_skipped {
            response["warning"] =
                json!("embeddings unavailable, vectors skipped — run rebuild later to backfill");
        }

        if self.config.graph.enabled && self.config.graph.remind_on_save {
            match self.compute_graph_pending(&stats.session_id) {
                Ok(Some(pending_value)) => {
                    response["graph_pending"] = pending_value;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!("graph_pending hint computation failed: {}", e);
                }
            }
        }

        Ok(response)
    }

    pub(super) fn search_sessions(&self, args: &Value) -> Result<Value, String> {
        // 与 HTTP /search 的 query 上限同口径（transport::http::search）。
        const MAX_MCP_QUERY_CHARS: usize = 10_000;
        let query = args["query"].as_str().ok_or("缺少 query")?;
        if query.trim().is_empty() {
            return Err("query 不能为空白".to_string());
        }
        if query.chars().count() > MAX_MCP_QUERY_CHARS {
            return Err(format!("query 过长（最多 {} 字符）", MAX_MCP_QUERY_CHARS));
        }
        // W3/N1: 显式给出的 top_k / time_range / search_mode / role 错类型必须
        // 报错（HTTP /search 由 serde 强类型拒收），省略或 null 仍走默认值。
        let top_k: Option<usize> =
            serde_json::from_value(args.get("top_k").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid top_k: {}", e))?;
        let top_k = top_k.unwrap_or(self.config.search.default_top_k).min(50);
        let search_mode: Option<String> =
            serde_json::from_value(args.get("search_mode").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid search_mode: {}", e))?;
        let search_mode = search_mode
            .as_deref()
            .unwrap_or(&self.config.search.search_mode);

        let mode = SearchMode::from_str(search_mode)
            .map_err(|_| format!("未知 search_mode: {}", search_mode))?;

        let time_range: Option<SearchTimeRange> =
            serde_json::from_value(args.get("time_range").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid time_range: {}", e))?;
        let (effective_after, before_ms) = crate::util::time::resolve_window(
            time_range.as_ref().and_then(|t| t.after.as_deref()),
            time_range.as_ref().and_then(|t| t.before.as_deref()),
            time_range.as_ref().and_then(|t| t.last_days),
        )
        .map_err(|e| e.to_string())?;

        let role: Option<String> =
            serde_json::from_value(args.get("role").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid role: {}", e))?;
        if let Some(ref r) = role {
            if !crate::util::VALID_ROLES.contains(&r.as_str()) {
                return Err(format!(
                    "invalid role '{}': must be one of {:?}",
                    r,
                    crate::util::VALID_ROLES
                ));
            }
        }

        let params = crate::fact::search::SearchParams {
            query: query.to_string(),
            search_mode: mode,
            top_k,
            after_ms: effective_after,
            before_ms,
            role,
        };

        let results =
            crate::fact::search::search_sessions(&self.db, self.embedder.as_deref(), &params)
                .map_err(|e| e.to_string())?;

        Ok(json!({
            "status": "ok",
            "count": results.len(),
            "results": results
        }))
    }

    pub(super) fn memory_write(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let content = args["content"].as_str().ok_or("缺少 content")?;
        // N5: confidence 错类型必须报错，不得静默落 "medium"；省略/null 仍默认。
        let confidence: Option<String> =
            serde_json::from_value(args.get("confidence").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid confidence: {}", e))?;
        let confidence = confidence.as_deref().unwrap_or("medium");
        let session_id: Option<String> =
            serde_json::from_value(args.get("session_id").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid session_id: {}", e))?;
        let session_id = session_id.as_deref();
        validate_optional_session_id(session_id)?;

        let bm = self.make_bounded_memory();
        bm.write(target, content, confidence, session_id)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok", "target": target}))
    }

    pub(super) fn memory_update(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
        let new_text = args["new_text"].as_str().ok_or("缺少 new_text")?;
        let session_id: Option<String> =
            serde_json::from_value(args.get("session_id").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid session_id: {}", e))?;
        let session_id = session_id.as_deref();
        validate_optional_session_id(session_id)?;

        let bm = self.make_bounded_memory();
        bm.update(target, old_text, new_text, session_id)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok"}))
    }

    pub(super) fn memory_remove(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
        let session_id: Option<String> =
            serde_json::from_value(args.get("session_id").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid session_id: {}", e))?;
        let session_id = session_id.as_deref();
        validate_optional_session_id(session_id)?;

        let bm = self.make_bounded_memory();
        bm.remove(target, old_text, session_id)
            .map_err(|e| e.to_string())?;

        Ok(json!({"status": "ok"}))
    }

    pub(super) fn memory_read(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;

        let bm = self.make_bounded_memory();
        let content = bm.read(target).map_err(|e| e.to_string())?;

        Ok(json!({"target": target, "content": content}))
    }

    pub(super) fn user_profile(&self, args: &Value) -> Result<Value, String> {
        let action = args["action"].as_str().ok_or("缺少 action")?;
        let bm = self.make_bounded_memory();

        match action {
            "read" => {
                let content = bm.read("user").map_err(|e| e.to_string())?;
                Ok(json!({"action": "read", "content": content}))
            }
            "write" => {
                let content = args["content"].as_str().ok_or("缺少 content")?;
                // N5 同族：confidence 错类型报错而非静默落 "medium"（省略/null 仍默认）。
                let confidence: Option<String> =
                    serde_json::from_value(args.get("confidence").cloned().unwrap_or(Value::Null))
                        .map_err(|e| format!("invalid confidence: {}", e))?;
                let confidence = confidence.as_deref().unwrap_or("medium");
                bm.write("user", content, confidence, None)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            "update" => {
                let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
                let new_text = args["new_text"].as_str().ok_or("缺少 new_text")?;
                bm.update("user", old_text, new_text, None)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            "remove" => {
                let old_text = args["old_text"].as_str().ok_or("缺少 old_text")?;
                bm.remove("user", old_text, None)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"status": "ok"}))
            }
            _ => Err(format!("未知 action: {}", action)),
        }
    }

    pub(super) fn memory_provenance(&self, args: &Value) -> Result<Value, String> {
        let target = args["target"].as_str().ok_or("缺少 target")?;
        let bm = self.make_bounded_memory();
        let report = bm.verify_provenance(target).map_err(|e| e.to_string())?;

        Ok(json!({
            "status": "ok",
            "report": report
        }))
    }

    fn make_bounded_memory(&self) -> BoundedMemory<'_> {
        BoundedMemory::new(
            &self.config.memory_dir(),
            &self.db,
            self.config.memory.memory_char_limit,
            self.config.memory.user_char_limit,
        )
        .with_security_scan(self.config.memory.security_scan)
    }

    pub(super) fn rebuild_index(&self) -> Result<Value, String> {
        self.start_rebuild(|body| std::thread::Builder::new().spawn(body).map(|_| ()))
    }

    /// W25b: Running 在 spawn 前置位作为并发占位（保住 already_running 语义），
    /// 所以线程创建失败必须显式复位，否则状态永久卡在 Running。
    /// `Builder::spawn` 把创建失败（线程资源耗尽等）返回为 io::Error 而非 panic；
    /// `spawn` 参数注入以便测试覆盖该失败路径。
    pub(super) fn start_rebuild(
        &self,
        spawn: impl FnOnce(Box<dyn FnOnce() + Send + 'static>) -> std::io::Result<()>,
    ) -> Result<Value, String> {
        let conversations_dir = self.config.conversations_dir();
        let db_path = self.config.profile_db_path();
        let dimensions = self.config.embedding.dimensions;
        let embedder = self.embedder.clone();
        let preview_length = self.config.conversation.preview_length;
        let progress = self.rebuild_progress.clone();

        {
            let mut p = crate::util::recover_poison(&self.rebuild_progress);
            if p.status == crate::index::rebuild::RebuildStatus::Running {
                return Ok(json!({
                    "status": "already_running",
                    "message": "Rebuild already in progress. Use rebuild_status to check."
                }));
            }
            *p = crate::index::rebuild::RebuildProgress::default();
            p.status = crate::index::rebuild::RebuildStatus::Running;
            p.started_at = crate::util::time::now_unix_ms();
        }

        let job = RebuildJob {
            conversations_dir,
            db_path,
            dimensions,
            embedder,
            preview_length,
            progress,
            exec_lock: self.exec_lock.clone(),
        };
        let body = move || job.run();

        if let Err(e) = spawn(Box::new(body)) {
            mark_rebuild_failed(&self.rebuild_progress, format!("spawn: {}", e));
            return Err(format!("rebuild 线程启动失败: {}", e));
        }

        Ok(json!({
            "status": "started",
            "message": "Rebuild started in background. Use rebuild_status to check progress."
        }))
    }

    pub(super) fn rebuild_status(&self) -> Result<Value, String> {
        let p = crate::util::recover_poison(&self.rebuild_progress);
        let elapsed = if let Some(f) = p.finished_at {
            f - p.started_at
        } else if p.started_at > 0 {
            crate::util::time::now_unix_ms() - p.started_at
        } else {
            0
        };
        Ok(json!({
            "status": p.status,
            "sessions_processed": p.sessions_processed,
            "turns_indexed": p.turns_indexed,
            "vectors_indexed": p.vectors_indexed,
            "errors": p.errors,
            // M4: 时钟回拨（now/finished_at 早于 started_at）不得产出负 elapsed。
            "elapsed_ms": elapsed.max(0),
        }))
    }
}
