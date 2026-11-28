use super::RebuildStats;
use crate::index::conversation;
use crate::index::db::Db;
use crate::index::provenance;
use crate::util::time;
use std::collections::{HashMap, HashSet};
use std::path::Path;

pub(super) fn run_phase1_rebuild(
    conversations_dir: &Path,
    db: &Db,
    preview_length: usize,
    vectors_will_be_wiped: bool,
    reencode_atom_vectors: bool,
) -> anyhow::Result<RebuildStats> {
    tracing::info!(
        target: "asuna_memory::index::rebuild",
        "完整重建模式：执行 Phase 1（元数据+FTS）"
    );
    let mut stats = RebuildStats {
        sessions_processed: 0,
        turns_indexed: 0,
        vectors_indexed: 0,
        vectors_skipped: 0,
        errors: Vec::new(),
    };

    // W5.2: 全量 JSONL 读取+解析移到写事务外——原实现在 immediate_tx 内逐文件
    // read_session（读全文+反序列化），写锁跨全量语料 IO，并发 gateway/MCP 写入
    // 超 busy_timeout=5000ms 即 SQLITE_BUSY。先解析进内存，事务只做批量写入。
    // W27b: 每文件解析后立即算 end_ts/total_tokens 并投影为紧凑 turn（仅
    // seq/ts/role/preview/char_count），完整 content/metadata 逐文件丢弃，
    // 不再全语料常驻（解析失败逐文件记 errors，不阻断其余会话——与原事务内
    // 错误累积语义一致）。
    // preserve-fresh: t0 必须在 try_list_sessions **之前**取——窗口期提交者的
    // in-tx 戳恒 > t0（C1），freshness 判定以此为下界，封死陈旧 parse 覆盖新 DB。
    let t0 = time::now_unix_ms();
    let files = conversation::try_list_sessions(conversations_dir)?;
    let mut parsed: Vec<ParsedSessionEntry> = Vec::with_capacity(files.len());
    // 解析失败的文件：只要 header 仍可解析就能识别会话身份，从而把该会话的 DB 行
    // **原样保留**（scoped 清表），而不是让一个坏文件阻塞所有会话的索引恢复。
    let mut preserve: Vec<String> = Vec::new();
    let mut unidentified: Vec<String> = Vec::new();
    let preview_cap = preview_length.max(1);
    for file_path in files {
        // preserve-fresh 文件水位：read 之前 stat（先 stat 后 read 偏保守——若 read
        // 期间文件被触及，parse 内容比水位新，事务内 re-stat 必判不一致而保留 DB）。
        let snap_stat = file_stat(&file_path);
        match conversation::read_session(&file_path) {
            Ok((header, turns)) => {
                // header start_time 不可解析的会话稍后在 index_session_file 记 errors
                // 并跳过，此处 end_ts 回退值不会被使用；可解析时与入库口径完全一致。
                let start_ts = time::ts_to_unix_ms(&header.start_time).unwrap_or(0);
                let end_ts = conversation::session_end_ts(&turns, start_ts);
                let total_tokens = conversation::session_total_tokens(&turns);
                // preserve-fresh 快照水位：snap_count 按 seq 去重（与入库
                // insert_session_turns / write_turn_rows 同口径），snap_max_seq
                // 取全量最大 seq。事务内与 DB 实表水位比对判新鲜。
                let snap_max_seq = turns.iter().map(|t| i64::from(t.seq)).max().unwrap_or(0);
                let snap_count = {
                    let mut seen = HashSet::new();
                    turns.iter().filter(|t| seen.insert(t.seq)).count() as i64
                };
                let compact: Vec<CompactTurn> = turns
                    .iter()
                    .map(|t| CompactTurn {
                        seq: t.seq,
                        ts: t.ts.clone(),
                        role: t.role.clone(),
                        preview: t.content.chars().take(preview_cap).collect(),
                        char_count: t.content.chars().count() as i64,
                    })
                    .collect();
                parsed.push(ParsedSessionEntry {
                    file_path,
                    header,
                    end_ts,
                    total_tokens,
                    turns: compact,
                    snap_count,
                    snap_max_seq,
                    snap_stat,
                });
            }
            Err(e) => {
                match conversation::read_session_id_only(&file_path) {
                    Some(sid) => {
                        tracing::warn!(
                            target: "asuna_memory::index::rebuild",
                            "rebuild: {} 解析失败但会话 {} 身份可识别，保留其 DB 行与向量: {}",
                            file_path.display(),
                            sid,
                            e
                        );
                        if !preserve.contains(&sid) {
                            preserve.push(sid);
                        }
                    }
                    None => unidentified.push(format!("{}", file_path.display())),
                }
                stats
                    .errors
                    .push(format!("{}: 解析失败: {}", file_path.display(), e));
            }
        }
    }

    // C1/W1: Phase 1 的清表是全局的（DELETE FROM turns/sessions/vec_turns），而
    // parsed 只含**可读**文件。语料目录缺失/为空、或有文件解析失败时，无法判定
    // 哪些会话的行应当保留——此时若 DB 已有会话，全量删除会销毁仍然正确的行，
    // 并把 entities/relations.source_turn 置 NULL（图谱溯源不可恢复）。拒绝执行
    // 而非静默清库；空 DB（首次安装）不受影响。
    // cfg(test) 窗口钩子：让单元测试在 parse 完成之后、BEGIN IMMEDIATE 之前注入
    // “并发提交者”（第二条 Db 连接写同一文件库），测试 preserve-fresh。生产无操作。
    #[cfg(test)]
    window_hook::run();
    let tx = db.immediate_tx()?;
    // W7: 计数必须在写事务**内**取——事务外读会给并发写入者（CLI import / MCP
    // 独立连接）留下窗口：守卫看到 0 而跳过 bail，随后全局 DELETE 销毁刚提交的行。
    let existing_sessions: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    if existing_sessions > 0 {
        if parsed.is_empty() {
            anyhow::bail!(
                "拒绝重建：DB 已有 {existing_sessions} 个会话，但 conversations 目录 {} 缺失或无可读 JSONL。\
                 全量重建会清空索引且无法恢复。请确认目录/卷挂载与 --profile 是否正确后重跑。",
                conversations_dir.display()
            );
        }
        if !unidentified.is_empty() {
            anyhow::bail!(
                "拒绝重建：{} 个 JSONL 文件损坏且**连 header 都无法解析**，会话身份不可识别，\
                 无法把其仍正确的 DB 行排除在全量清表之外。请先修复或移出这些文件后重跑：{}",
                unidentified.len(),
                unidentified.join("; ")
            );
        }
    }

    // 同一 session_id 同时出现在损坏文件与可读文件时：**保留优先**——把该会话从
    // parsed 剔除。否则保留的旧 turns 未被删、新 turns 又插入，会产生重复 seq。
    if !preserve.is_empty() {
        let before = parsed.len();
        parsed.retain(|e| !preserve.contains(&e.header.session_id));
        if parsed.len() != before {
            tracing::warn!(
                target: "asuna_memory::index::rebuild",
                "rebuild: {} 个可读文件的 session_id 与损坏文件重合，保留 DB 现有副本并跳过重插",
                before - parsed.len()
            );
        }
    }

    // preserve-fresh（多写者窗口收敛）：parsed 是 t0 之后某刻 T_read 的陈旧快照；
    // 若并发写者在 (T_read, 本事务 BEGIN IMMEDIATE) 窗口内提交了更新的 DB 状态，
    // 按水位判定保留 DB 行而非用陈旧 parse 覆盖（scoped DELETE 跳过 preserved、
    // FTS 按 rowid 补 posting、remap 自映射 preserved turn id → provenance 不置 NULL）。
    preserve_fresh_sessions(db.conn(), conversations_dir, &parsed, &mut preserve, t0)?;
    parsed.retain(|e| !preserve.contains(&e.header.session_id));

    rebuild_metadata(conversations_dir, db, &parsed, &mut stats, &preserve)?;
    // W1.1: 完整重建且有 embedder 时同时清空旧标度 atom 向量——否则 v2.7.1 的
    // 固定 clamp(-1,1)*127 标度行永久留存（DDL 未变故启动变更检测不触发 DROP），
    // 与 absmax 查询向量比对 cosine 被抬高、检索阈值大量拒掉本应命中的 atom，
    // 违背 HISTORY 升级指引"rebuild --full 重编码现有向量"。Phase 2 后由
    // maybe_backfill_bounded_memory_vec 以当前标度重嵌入。
    if reencode_atom_vectors {
        db.conn().execute("DELETE FROM vec_bounded_memory", [])?;
    }
    tx.commit()?;
    if vectors_will_be_wiped {
        stats.errors.push(
            "无嵌入后端：向量索引已清空且本次无法重建；语义/混合搜索将降级为关键词。\
             请配置在线 embedding API 后重跑 rebuild（atom 向量保留旧标度，未清空）"
                .to_string(),
        );
    }
    Ok(stats)
}

/// Phase 1: 清理表、插入 sessions/turns、重建 FTS 索引
///
/// 调用方负责 BEGIN/COMMIT 事务包裹。W5.2: JSONL 已由调用方在事务外解析；
/// W27b: 解析结果投影为紧凑条目（路径, header, end_ts, total_tokens, 紧凑 turns），
/// 本函数只做 DB 写入；解析失败的文件已由调用方记入 stats.errors。
struct ParsedSessionEntry {
    file_path: std::path::PathBuf,
    header: conversation::SessionHeader,
    end_ts: Option<i64>,
    total_tokens: i64,
    turns: Vec<CompactTurn>,
    /// preserve-fresh 快照水位：文件去重后 turn 数（与入库去重同口径）与最大 seq。
    snap_count: i64,
    snap_max_seq: i64,
    /// preserve-fresh 文件水位：解析前 stat 的 (len, mtime_ns)，见 [`file_stat`]。
    snap_stat: Option<(u64, u128)>,
}

/// W27b: 紧凑 turn 投影——只保留入库所需字段，完整 content/metadata 逐文件丢弃。
/// ts 保留原串：坏 ts 的 warn+errors+回退 start_ts 报告仍在插入期逐条执行（口径不变）。
struct CompactTurn {
    seq: u32,
    ts: String,
    role: String,
    preview: String,
    char_count: i64,
}

struct MetadataRebuildCtx<'a> {
    conversations_dir: &'a Path,
}

/// preserve-fresh 文件水位：解析前 stat 的 `(len, mtime_ns)`。事务内 re-stat 与
/// 之不一致 ⇒ 文件在 parse 之后被触及；触及者必在本事务 `BEGIN IMMEDIATE` 之前已
/// COMMIT（WAL 单写者互斥，官方 wal.html / isolation.html：BEGIN IMMEDIATE 阻塞
/// 所有其他写者），故本事务看到的 DB 至少与 post-parse 文件一样新 → 保留 DB 而非
/// 用陈旧 parse 覆盖。这封死 BEGIN-crosses-t0 竞态：写者 BEGIN/戳早于 t0、rename/
/// COMMIT 晚于 parse 时，count/max_seq 可能全等且水位 < t0，唯文件水位能判新鲜。
/// stat 失败记 None；re-stat 任一侧缺失即判不一致（偏保守保留）。
fn file_stat(path: &Path) -> Option<(u64, u128)> {
    let md = std::fs::metadata(path).ok()?;
    let mtime_ns = md
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((md.len(), mtime_ns))
}

/// preserve-fresh：在写事务内按 DB 水位判定哪些会话须保留（不被陈旧 parse 覆盖），
/// 把其 session_id 追加进 `preserve`（调用方随后从 parsed 剔除）。判据：
/// - parsed 中的会话：`updated_at>=t0`（C1 in-tx 戳）OR `db_count>snap_count`
///   OR `db_max_seq>snap_max_seq` OR `created_at>=t0` OR 文件水位 re-stat 不一致
///   （parse 之后被触及，见 [`file_stat`]，封死 BEGIN-crosses-t0 竞态）。
///   C2: `db_count` 用 turns 实表 `COUNT(*)`，**不**用 `sessions.turn_count`——
///   Overwrite 存输入 len（含重复 seq），`write_turn_rows` 去重后实表行数更少，
///   `turn_count` 虚高会恒判 `db_count>snap_count` 而误保留。
/// - sid 不在 parsed（文件缺失/窗口内新建）：`created_at>=t0` OR `updated_at>=t0`
///   （C3：升级残留会话窗口内被覆盖时 created_at 旧但 updated_at 新）OR
///   `sessions.file_path` 此刻在磁盘存在（文件在 parse 之后才出现＝窗口内新建，
///   其 BEGIN/戳可能早于 t0）才保留，否则照旧清理（vanished file cleanup）。
fn preserve_fresh_sessions(
    conn: &rusqlite::Connection,
    conversations_dir: &Path,
    parsed: &[ParsedSessionEntry],
    preserve: &mut Vec<String>,
    t0: i64,
) -> anyhow::Result<()> {
    use rusqlite::OptionalExtension;
    // 文件水位 re-stat：任一仍在 parsed 的文件与解析前 stat 不一致 ⇒ 该会话 parse
    // 之后被触及 → 保留（DB 至少与 post-parse 文件一样新，见 file_stat）。
    let mut touched: HashSet<&str> = HashSet::new();
    for entry in parsed {
        if file_stat(&entry.file_path) != entry.snap_stat {
            touched.insert(entry.header.session_id.as_str());
        }
    }
    // (a) parsed 中的会话：DB 水位领先文件快照 → 保留。
    // 重复 session_id（多文件）取各快照 count/max_seq 的**最大值**比对：DB 若只是
    // 与胜出版本一致（而非窗口期新增），不得因败者文件快照更小而误判保留——否则
    // 会把胜出文件一并剔出 parsed，破坏重复替换（W4 保存点回归）。
    let mut snap_wm: HashMap<&str, (i64, i64)> = HashMap::new();
    for entry in parsed {
        let wm = snap_wm
            .entry(entry.header.session_id.as_str())
            .or_insert((0, 0));
        wm.0 = wm.0.max(entry.snap_count);
        wm.1 = wm.1.max(entry.snap_max_seq);
    }
    let mut wm_stmt = conn.prepare(
        "SELECT s.created_at, s.updated_at,
                (SELECT COUNT(*) FROM turns t WHERE t.session_id = s.session_id),
                (SELECT COALESCE(MAX(t.seq), 0) FROM turns t WHERE t.session_id = s.session_id)
           FROM sessions s WHERE s.session_id = ?1",
    )?;
    for (sid, (snap_count, snap_max_seq)) in &snap_wm {
        let wm = wm_stmt
            .query_row(rusqlite::params![sid], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })
            .optional()?;
        if let Some((created_at, updated_at, db_count, db_max_seq)) = wm {
            if (updated_at >= t0
                || db_count > *snap_count
                || db_max_seq > *snap_max_seq
                || created_at >= t0
                || touched.contains(*sid))
                && !preserve.iter().any(|s| s.as_str() == *sid)
            {
                preserve.push((*sid).to_string());
            }
        }
    }
    // (b) sid 不在 parsed（文件缺失/窗口内新建）：仅窗口内触及者保留。
    let parsed_sids: HashSet<&str> = parsed
        .iter()
        .map(|e| e.header.session_id.as_str())
        .collect();
    let mut sess_stmt =
        conn.prepare("SELECT session_id, created_at, updated_at, file_path FROM sessions")?;
    let rows = sess_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (sid, created_at, updated_at, file_path) = row?;
        // 文件此刻存在＝parse 之后才出现（窗口内新建，BEGIN/戳可能早于 t0）→ 保留。
        let file_appeared = file_path.is_some_and(|p| conversations_dir.join(p).exists());
        if !parsed_sids.contains(sid.as_str())
            && (created_at >= t0 || updated_at >= t0 || file_appeared)
            && !preserve.contains(&sid)
        {
            preserve.push(sid);
        }
    }
    Ok(())
}

fn prepare_rebuild_tables(conn: &rusqlite::Connection, preserve: &[String]) -> anyhow::Result<()> {
    // 0. C9: 在 DELETE 前快照旧 turn 身份 (id, session_id, seq) 与 sessions 水位供后续恢复。
    provenance::snapshot_all_turn_ids(conn)?;
    conn.execute_batch(
        "DROP TABLE IF EXISTS temp.tmp_session_ts;
         CREATE TEMP TABLE tmp_session_ts (
             session_id TEXT PRIMARY KEY, created_at INTEGER, updated_at INTEGER
         );
         INSERT INTO tmp_session_ts SELECT session_id, created_at, updated_at FROM sessions;",
    )?;
    // W27b: preserve 入 temp 表——大 preserve 集不再拼 IN (?1..?N) 撞 SQL 变量数上限。
    if !preserve.is_empty() {
        conn.execute_batch(
            "DROP TABLE IF EXISTS temp.tmp_preserve_sessions;
             CREATE TEMP TABLE tmp_preserve_sessions (session_id TEXT PRIMARY KEY);",
        )?;
        let mut ins = conn.prepare("INSERT INTO tmp_preserve_sessions (session_id) VALUES (?1)")?;
        for sid in preserve {
            ins.execute(rusqlite::params![sid])?;
        }
    }
    // 1. 先 DROP turns_ad 再 'delete-all' 清空 FTS，最后重建 turns_ad。
    // scoped 保留：文件损坏但会话身份可识别时，只清其余会话，保住该会话仍正确的
    // DB 行 / 向量 / 图谱溯源（remap 按 (session_id, seq) join 当前 turns，保留行
    // id 不变 → 自然映射到自身，无需改 remap）。
    conn.execute("DROP TRIGGER IF EXISTS turns_ad", [])?;
    if !preserve.is_empty() {
        conn.execute_batch(
            "DELETE FROM vec_turns WHERE rowid IN
             (SELECT id FROM tmp_turn_id_old
              WHERE session_id NOT IN (SELECT session_id FROM temp.tmp_preserve_sessions));
             DELETE FROM turns
              WHERE session_id NOT IN (SELECT session_id FROM temp.tmp_preserve_sessions);
             DELETE FROM sessions
              WHERE session_id NOT IN (SELECT session_id FROM temp.tmp_preserve_sessions);",
        )?;
    } else {
        conn.execute_batch(
            "DELETE FROM turns;
             DELETE FROM sessions;
             DELETE FROM vec_turns;",
        )?;
    }
    if let Err(e) = conn.execute("INSERT INTO turns_fts(turns_fts) VALUES('delete-all')", []) {
        tracing::warn!(
            target: "asuna_memory::index::rebuild",
            "turns_fts 'delete-all' 失败（{}），DROP/重建兜底",
            e
        );
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS turns_fts; {}",
            crate::index::schema::TURNS_FTS_DDL
        ))?;
    }
    conn.execute_batch(crate::index::schema::TURNS_AD_TRIGGER_SQL)?;
    // 'delete-all' 清掉了**全部** posting，而保留会话的 turn 不会被重插（turns_ai
    // 不触发）→ 必须按显式 rowid 补回它们的 posting，否则这些会话重建后搜不到。
    // contentless FTS5 支持带非 NULL rowid 的 INSERT（官方文档 §4.4.1）。
    if !preserve.is_empty() {
        conn.execute(
            "INSERT INTO turns_fts(rowid, preview)
             SELECT id, preview FROM turns
             WHERE session_id IN (SELECT session_id FROM temp.tmp_preserve_sessions)
               AND preview IS NOT NULL",
            [],
        )?;
        conn.execute_batch("DROP TABLE IF EXISTS temp.tmp_preserve_sessions;")?;
    }
    Ok(())
}

fn validate_duplicate_replacement(
    conversations_dir: &Path,
    file_path: &Path,
    header: &conversation::SessionHeader,
    stats: &mut RebuildStats,
) -> bool {
    if let Err(e) = time::ts_to_unix_ms(&header.start_time) {
        tracing::warn!(
            target: "asuna_memory::index::rebuild",
            "rebuild: session_id 重复（{}）且较新文件 start_time 不可解析（{}），保留先前文件 turns",
            header.session_id,
            e
        );
        stats.errors.push(format!(
            "{}: 时间解析失败（重复 session_id，保留先前文件）: {}",
            file_path.display(),
            e
        ));
        return false;
    }
    if resolve_rel_path(conversations_dir, file_path).is_none() {
        tracing::warn!(
            target: "asuna_memory::index::rebuild",
            "rebuild: session_id 重复（{}）且较新文件路径含非 UTF-8 字符，保留先前文件 turns",
            header.session_id
        );
        stats.errors.push(format!(
            "{}: 路径含非 UTF-8 字符（重复 session_id，保留先前文件），跳过本会话",
            file_path.display()
        ));
        return false;
    }
    true
}

fn index_or_replace_session(
    conn: &rusqlite::Connection,
    ctx: &MetadataRebuildCtx<'_>,
    entry: &ParsedSessionEntry,
    seen_sessions: &mut HashMap<String, usize>,
    stats: &mut RebuildStats,
) -> anyhow::Result<()> {
    let file_path = &entry.file_path;
    let header = &entry.header;
    let turns = &entry.turns;
    let prev_turns = seen_sessions.get(&header.session_id).copied();
    let prev_and_sp = match prev_turns {
        Some(old_turns) => {
            if !validate_duplicate_replacement(ctx.conversations_dir, file_path, header, stats) {
                return Ok(());
            }
            tracing::warn!(
                target: "asuna_memory::index::rebuild",
                "rebuild: session_id 重复（{}），按排序后见者胜出，在保存点内清理先前 turns",
                header.session_id
            );
            let sp = SavepointGuard::begin(conn)?;
            conn.execute(
                "DELETE FROM turns WHERE session_id = ?1",
                rusqlite::params![header.session_id],
            )?;
            Some((old_turns, sp))
        }
        None => None,
    };

    let inserted = index_session_file(conn, ctx.conversations_dir, entry, stats);
    match (prev_and_sp, inserted) {
        // W22: 替代文件本有 turns 却一行未落（语句预处理失败 / 全部 INSERT 失败）
        // 时，释放保存点会提交上面的 DELETE FROM turns，先前胜出者仍正确的行被
        // 销毁。回滚保存点保留旧行（turns 为空的合法 0 行会话仍走下面的正常臂）。
        (Some((_, sp)), Some(0)) if !turns.is_empty() => {
            drop(sp);
            tracing::warn!(
                target: "asuna_memory::index::rebuild",
                "rebuild: 重复 session_id {} 的较新文件写入 0 行，回滚替换以保留先前 turns",
                header.session_id
            );
        }
        (Some((old_turns, sp)), Some(new_turns)) => {
            sp.release()?;
            stats.turns_indexed = stats.turns_indexed.saturating_sub(old_turns);
            seen_sessions.insert(header.session_id.clone(), new_turns);
        }
        (Some((_, sp)), None) => drop(sp),
        (None, Some(new_turns)) => {
            stats.sessions_processed += 1;
            seen_sessions.insert(header.session_id.clone(), new_turns);
        }
        (None, None) => {}
    }
    Ok(())
}

/// Phase 1 核心：在调用方已持有的 `IMMEDIATE` 事务内清空旧索引、按已解析列表
/// 写入 sessions + turns、并把软引用 remap 到新 turns.id。
/// 传入预先读完的紧凑 `parsed`，本函数只做 DB 写入。
fn rebuild_metadata(
    conversations_dir: &Path,
    db: &Db,
    parsed: &[ParsedSessionEntry],
    stats: &mut RebuildStats,
    preserve: &[String],
) -> anyhow::Result<()> {
    let conn = db.conn();
    // W27b: RAII 兜底——失败/panic 路径上 temp 表不残留连接（成功路径末尾显式 DROP）。
    let _temp_guard = TempTableGuard(conn);
    prepare_rebuild_tables(conn, preserve)?;

    let ctx = MetadataRebuildCtx { conversations_dir };
    let mut seen_sessions: HashMap<String, usize> = HashMap::new();
    for entry in parsed {
        index_or_replace_session(conn, &ctx, entry, &mut seen_sessions, stats)?;
    }

    // C9: 全部重插完成后，把图谱/有界记忆中的旧 turn 软引用 remap 到新 id
    provenance::remap_all_turn_refs(conn)?;

    // W27b: 成功路径显式清理会话水位快照 temp 表。
    conn.execute_batch("DROP TABLE IF EXISTS temp.tmp_session_ts;")?;

    tracing::info!(
        target: "asuna_memory::index::rebuild",
        "Phase 1 完成: {} 个会话, {} 轮对话, {} 个错误",
        stats.sessions_processed,
        stats.turns_indexed,
        stats.errors.len(),
    );

    Ok(())
}

/// W27b: temp 表 RAII 清理守卫（错误/panic 路径兜底；IF EXISTS 使成功路径重复 DROP 无害）。
struct TempTableGuard<'a>(&'a rusqlite::Connection);

impl Drop for TempTableGuard<'_> {
    fn drop(&mut self) {
        let _ = self.0.execute_batch(
            "DROP TABLE IF EXISTS temp.tmp_session_ts;
             DROP TABLE IF EXISTS temp.tmp_preserve_sessions;",
        );
    }
}

/// RAII SQLite SAVEPOINT 守卫（`rusqlite::Connection::savepoint` 需要 `&mut Connection`，
/// 本守卫直接基于 `&Connection` 运行命名保存点；未显式 `release` 而离开作用域时自动
/// `ROLLBACK TO SAVEPOINT ...; RELEASE SAVEPOINT ...;` 弹出栈并恢复先前状态）。
struct SavepointGuard<'a> {
    conn: &'a rusqlite::Connection,
    released: bool,
}

impl<'a> SavepointGuard<'a> {
    fn begin(conn: &'a rusqlite::Connection) -> rusqlite::Result<Self> {
        conn.execute_batch("SAVEPOINT sp_session_replace;")?;
        Ok(Self {
            conn,
            released: false,
        })
    }

    fn release(mut self) -> rusqlite::Result<()> {
        self.released = true;
        self.conn
            .execute_batch("RELEASE SAVEPOINT sp_session_replace;")
    }
}

impl Drop for SavepointGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            let _ = self.conn.execute_batch(
                "ROLLBACK TO SAVEPOINT sp_session_replace; RELEASE SAVEPOINT sp_session_replace;",
            );
        }
    }
}

fn resolve_rel_path(conversations_dir: &Path, file_path: &Path) -> Option<String> {
    file_path
        .strip_prefix(conversations_dir)
        .unwrap_or(file_path)
        .to_str()
        .map(str::to_string)
}

/// 处理单个 JSONL 会话文件：插入 session 与其所有 turns；成功返回实际插入轮次，
/// 早期校验/session 插入失败返回 `None`（由调用方决定是否回滚保存点）。
fn index_session_file(
    conn: &rusqlite::Connection,
    conversations_dir: &Path,
    entry: &ParsedSessionEntry,
    stats: &mut RebuildStats,
) -> Option<usize> {
    let file_path = &entry.file_path;
    let header = &entry.header;
    let start_ts = match time::ts_to_unix_ms(&header.start_time) {
        Ok(ts) => ts,
        Err(e) => {
            stats
                .errors
                .push(format!("{}: 时间解析失败: {}", file_path.display(), e));
            return None;
        }
    };

    // D2-9 / M6: 与 SessionStore::save_overwrite 共用 session_end_ts 计算逻辑。
    // W27b: end_ts/total_tokens 已在解析期按同一 helper 算好（紧凑投影）。
    let end_ts = entry.end_ts;
    let total_tokens = entry.total_tokens;

    // D7-4: 非 UTF-8 路径不得 to_string_lossy 写库（U+FFFD 替换后 cleanup 按
    // file_path 定位旧文件必失败）——记 errors 并跳过本会话，可观测不静默。
    let file_rel_path = match resolve_rel_path(conversations_dir, file_path) {
        Some(s) => s,
        None => {
            stats.errors.push(format!(
                "{}: 路径含非 UTF-8 字符，无法可靠记录 file_path，跳过本会话",
                file_path.display()
            ));
            return None;
        }
    };

    let tags_json = if header.tags.is_empty() {
        None
    } else {
        serde_json::to_string(&header.tags).ok()
    };

    let now = time::now_unix_ms();

    // 插入 session
    // W18: created_at / updated_at 是"何时入库 / 何时被触及"的**水位**，不是内容
    // 时间戳。重建必须保留原值（从 tmp_session_ts 快照取），否则升级必跑的
    // `rebuild --full` 会把整个归档语料判为刚变化，误触发全量 L3/L4/L5 重生成。
    // 也不能改用 end_ts：它可能比原水位更早（静默取消已挂起的整合），且空 turns
    // 时为 NULL，违反 updated_at NOT NULL。仅新会话用 now。
    if let Err(e) = conn.execute(
        "INSERT INTO sessions
         (session_id, start_ts, end_ts, file_path, title, profile_id, source, agent_model,
          turn_count, total_tokens, tags, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 COALESCE((SELECT created_at FROM tmp_session_ts WHERE session_id = ?1), ?12),
                 COALESCE((SELECT updated_at FROM tmp_session_ts WHERE session_id = ?1), ?12))
         ON CONFLICT(session_id) DO UPDATE SET
           start_ts = excluded.start_ts, end_ts = excluded.end_ts,
           file_path = excluded.file_path, title = excluded.title,
           profile_id = excluded.profile_id, source = excluded.source,
           agent_model = excluded.agent_model, turn_count = excluded.turn_count,
           total_tokens = excluded.total_tokens, tags = excluded.tags,
           updated_at = excluded.updated_at",
        rusqlite::params![
            header.session_id,
            start_ts,
            end_ts,
            file_rel_path,
            header.title,
            header.profile_id,
            header.source,
            header.agent_model,
            entry.turns.len() as i64,
            total_tokens,
            tags_json,
            now,
        ],
    ) {
        stats
            .errors
            .push(format!("{}: session 插入失败: {}", file_path.display(), e));
        return None;
    }

    // 插入 turns（turns_ai 触发器同步写 FTS）
    let inserted = insert_session_turns(
        conn,
        file_path,
        &header.session_id,
        start_ts,
        &entry.turns,
        stats,
    );

    // D2-10: turn_count 记实际成功插入数（而非 JSONL 侧 turns.len()）——插入
    // 失败/重复 seq 去重后两者不等，doctor 与增量检测按 DB 计数比对。
    if let Err(e) = conn.execute(
        "UPDATE sessions SET turn_count = ?1 WHERE session_id = ?2",
        rusqlite::params![inserted as i64, header.session_id],
    ) {
        stats.errors.push(format!(
            "{}: turn_count 更新失败: {}",
            file_path.display(),
            e
        ));
    }

    Some(inserted)
}

/// 插入单个会话的所有 turns，失败仅记录不中断；返回实际成功插入数（D2-10）。
/// W27b: turns 为紧凑投影（preview/char_count 已在解析期按 preview_length 算好）。
fn insert_session_turns(
    conn: &rusqlite::Connection,
    file_path: &Path,
    session_id: &str,
    start_ts: i64,
    turns: &[CompactTurn],
    stats: &mut RebuildStats,
) -> usize {
    let mut stmt = match conn.prepare_cached(
        "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview, char_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    ) {
        Ok(s) => s,
        Err(e) => {
            stats.errors.push(format!(
                "{}: turns 语句预处理失败: {}",
                file_path.display(),
                e
            ));
            return 0;
        }
    };
    // D2-11: 同文件重复 seq 去重（保留先见者，与写路径 session_store 的升级迁移
    // 同口径）——否则重复行虚增 turn_count 与 FTS posting。
    let mut seen_seqs: HashSet<u32> = HashSet::new();
    let mut inserted = 0usize;
    for turn in turns {
        if !seen_seqs.insert(turn.seq) {
            tracing::warn!(
                target: "asuna_memory::index::rebuild",
                "rebuild: 文件 {} 中 seq {} 重复，保留先见者丢弃后见者",
                file_path.display(),
                turn.seq
            );
            continue;
        }
        // W2.6: 坏 ts 回退 start_ts 必须可观测（与 header start_time 失败记 errors
        // 同口径）——静默塌缩会污染时间窗过滤与 recency 排序。数据仍落库（M1 保数据）。
        let ts_ms = match time::ts_to_unix_ms(&turn.ts) {
            Ok(ts) => ts,
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::index::rebuild",
                    "rebuild: turn ts 不可解析（{}: seq={}），回退 start_ts: {}",
                    file_path.display(),
                    turn.seq,
                    e
                );
                stats.errors.push(format!(
                    "{}: turn {} 时间解析失败（已回退 start_ts）: {}",
                    file_path.display(),
                    turn.seq,
                    e
                ));
                start_ts
            }
        };

        if let Err(e) = stmt.execute(rusqlite::params![
            session_id,
            turn.seq as i64,
            ts_ms,
            turn.role,
            turn.preview,
            turn.char_count,
        ]) {
            stats.errors.push(format!(
                "{}: turn {} 插入失败: {}",
                file_path.display(),
                turn.seq,
                e
            ));
        } else {
            stats.turns_indexed += 1;
            inserted += 1;
        }
    }
    inserted
}

/// cfg(test) 窗口钩子：在 `run_phase1_rebuild` 的「parse 完成之后、BEGIN IMMEDIATE
/// 之前」窗口点运行一个一次性闭包，模拟并发写者在窗口内提交。闭包用第二条
/// `Db::open` 连接写同一文件库（SQLite 按连接加锁，是跨进程 WAL 的有效进程内代理）。
///
/// **线程局部**单槽：rebuild 全程同步运行在测试线程上，钩子由本线程 set、本线程
/// 的 rebuild 消费。线程局部隔离杜绝了「全局槽 + 互斥」方案下并行 rebuild 测试
/// 互相窃取钩子的竞态（run() 在每个 cfg(test) rebuild 都会触发），无需串行锁。
#[cfg(test)]
pub(super) mod window_hook {
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce()>;
    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// 安装窗口闭包（由本线程下一次 `run_phase1_rebuild` 消费）。
    pub(in crate::index::rebuild) fn set(f: impl FnOnce() + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }

    /// 窗口点运行并消费闭包（无闭包则无操作）。
    pub(super) fn run() {
        let f = HOOK.with(|h| h.borrow_mut().take());
        if let Some(f) = f {
            f();
        }
    }
}
