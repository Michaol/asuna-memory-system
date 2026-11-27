use super::progress::ProgressFn;
use super::RebuildStats;
use crate::index::db::Db;

pub(super) fn apply_phase2_vectors(
    db: &Db,
    emb: &crate::embedder::LazyEmbedder,
    on_progress: Option<&ProgressFn>,
    stats: &mut RebuildStats,
) {
    match rebuild_vectors(
        db,
        emb,
        on_progress,
        stats.sessions_processed,
        stats.turns_indexed,
    ) {
        Ok((indexed, skipped, failed, errs)) => {
            stats.vectors_indexed = indexed;
            stats.vectors_skipped = skipped;
            if failed > 0 {
                stats.errors.push(format!(
                    "向量阶段: {} 条 turn 未能索引，语义/混合搜索将不完整",
                    failed
                ));
                stats.errors.extend(errs);
            }
        }
        Err(e) => {
            tracing::error!(
                target: "asuna_memory::index::rebuild",
                "rebuild Phase 2 向量嵌入失败: {}",
                e
            );
            stats.errors.push(format!("向量嵌入阶段失败: {}", e));
        }
    }
}

struct VectorBatchContext<'a> {
    db: &'a Db,
    embedder: &'a crate::embedder::LazyEmbedder,
    embed_batch_size: usize,
    num_db_batches: usize,
    total: usize,
    /// D1-4: Phase 1 计数透传给进度回调（运行中可见）。
    sessions_processed: usize,
    turns_indexed: usize,
    on_progress: Option<&'a ProgressFn>,
}

pub(super) struct VectorBatchCounters {
    pub(super) vectors_indexed: usize,
    pub(super) failed_count: usize,
    pub(super) errors: Vec<String>,
}

/// D5-6/D7-1: 一个写事务覆盖多少个嵌入批——摊销 WAL fsync/提交开销，同时
/// 限制单事务写锁时长与断点续传粒度（崩溃最多重嵌入一个事务批）。
const TX_BATCH_EMBED_MULTIPLIER: usize = 10;

/// D5-4: keyset 分页取一批待嵌入 turn（SQL 反连接排除已索引，D5-3）。
/// 每页只物化一个事务批的预览，不再一次性拉全语料 (id, preview) + 全量
/// vec rowid HashSet。嵌入失败的行 id ≤ last_id，本次运行不重试，断点续传
/// 下次运行经反连接重新拾取。
fn load_pending_vector_page(
    conn: &rusqlite::Connection,
    after_id: i64,
    limit: usize,
) -> anyhow::Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.preview FROM turns t
         WHERE t.preview IS NOT NULL AND t.id > ?1
           AND NOT EXISTS (SELECT 1 FROM vec_turns_rowids v WHERE v.rowid = t.id)
         ORDER BY t.id LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![after_id, limit as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Phase 2: 向量嵌入（分批事务，支持断点续传）
///
/// 返回 (已索引数, 跳过数, 失败 turn 数, 错误样本)。失败仅记录不中断，由调用方
/// 汇总到 stats.errors，避免"全部插入失败却报告成功"的静默退化。
fn query_vector_backlog_counts(conn: &rusqlite::Connection) -> anyhow::Result<(i64, i64)> {
    let total_turns: i64 = conn.query_row(
        "SELECT COUNT(*) FROM turns WHERE preview IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let pending_total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM turns t
         WHERE t.preview IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM vec_turns_rowids v WHERE v.rowid = t.id)",
        [],
        |r| r.get(0),
    )?;
    Ok((total_turns, pending_total))
}

fn rebuild_vectors(
    db: &Db,
    embedder: &crate::embedder::LazyEmbedder,
    on_progress: Option<&ProgressFn>,
    sessions_processed: usize,
    turns_indexed: usize,
) -> anyhow::Result<(usize, usize, usize, Vec<String>)> {
    let conn = db.conn();
    let (total_turns, pending_total) = query_vector_backlog_counts(conn)?;
    let skipped = (total_turns - pending_total).max(0) as usize;
    if pending_total == 0 {
        tracing::info!(
            target: "asuna_memory::index::rebuild",
            "Phase 2: 向量索引已是最新 ({} 条), 跳过",
            skipped
        );
        return Ok((skipped, skipped, 0, Vec::new()));
    }
    run_pending_vector_rebuild(
        db,
        embedder,
        on_progress,
        VectorBacklog {
            sessions_processed,
            turns_indexed,
            total_turns: total_turns as usize,
            pending_total: pending_total as usize,
            skipped,
        },
    )
}

struct VectorBacklog {
    sessions_processed: usize,
    turns_indexed: usize,
    total_turns: usize,
    pending_total: usize,
    skipped: usize,
}

fn run_pending_vector_rebuild(
    db: &Db,
    embedder: &crate::embedder::LazyEmbedder,
    on_progress: Option<&ProgressFn>,
    backlog: VectorBacklog,
) -> anyhow::Result<(usize, usize, usize, Vec<String>)> {
    if backlog.skipped > 0 {
        tracing::info!(
            target: "asuna_memory::index::rebuild",
            "向量断点续传：跳过已索引的 {} 条",
            backlog.skipped
        );
    }

    let embed_batch_size = embedder.batch_size().max(1);
    // W9b: saturating_mul 防 usize 溢出回绕成极小事务批（.max(1) 兜底 0）。
    let tx_batch_size = embed_batch_size
        .saturating_mul(TX_BATCH_EMBED_MULTIPLIER)
        .max(1);
    let ctx = VectorBatchContext {
        db,
        embedder,
        embed_batch_size,
        num_db_batches: backlog.pending_total.div_ceil(tx_batch_size),
        total: backlog.total_turns,
        sessions_processed: backlog.sessions_processed,
        turns_indexed: backlog.turns_indexed,
        on_progress,
    };
    tracing::info!(
        target: "asuna_memory::index::rebuild",
        "Phase 2 开始: {} 条待嵌入 (跳过 {} 条已存在)",
        backlog.pending_total,
        backlog.skipped
    );
    let mut counters = VectorBatchCounters {
        vectors_indexed: backlog.skipped,
        failed_count: 0,
        errors: Vec::new(),
    };
    run_vector_batch_pages(db.conn(), &ctx, tx_batch_size, &mut counters)?;
    tracing::info!(
        target: "asuna_memory::index::rebuild",
        "Phase 2 完成: {} 条向量已索引 ({} 条跳过)",
        counters.vectors_indexed - backlog.skipped,
        backlog.skipped
    );
    Ok((
        counters.vectors_indexed,
        backlog.skipped,
        counters.failed_count,
        counters.errors,
    ))
}

fn run_vector_batch_pages(
    conn: &rusqlite::Connection,
    ctx: &VectorBatchContext<'_>,
    tx_batch_size: usize,
    counters: &mut VectorBatchCounters,
) -> anyhow::Result<()> {
    let mut last_id = 0i64;
    let mut tx_idx = 0usize;
    while let Some(page) =
        Some(load_pending_vector_page(conn, last_id, tx_batch_size)?).filter(|p| !p.is_empty())
    {
        last_id = page.last().map_or(last_id, |&(id, _)| id);
        process_vector_db_batch(ctx, tx_idx, &page, counters)?;
        tx_idx += 1;
    }
    Ok(())
}

/// 向量错误样本保留上限
const MAX_ERR_SAMPLES: usize = 5;

/// 处理单个事务批：先事务外嵌入+量化，再 IMMEDIATE 事务批量 INSERT
fn process_vector_db_batch(
    ctx: &VectorBatchContext<'_>,
    tx_idx: usize,
    db_chunk: &[(i64, String)],
    counters: &mut VectorBatchCounters,
) -> anyhow::Result<()> {
    // 1) 事务外：本事务批的全部嵌入 + int8 量化（纯内存产物 (turn_id, bytes)）
    let mut rows: Vec<(i64, Vec<u8>)> = Vec::with_capacity(db_chunk.len());
    for embed_chunk in db_chunk.chunks(ctx.embed_batch_size) {
        embed_quantize_chunk(
            ctx.embedder,
            embed_chunk,
            &mut rows,
            &mut counters.failed_count,
            &mut counters.errors,
        );
    }

    // 2) 短事务：只写不联网（RAII IMMEDIATE 事务）
    let tx = ctx.db.immediate_tx()?;
    insert_quantized_vector_rows(&tx, &rows, counters)?;
    tx.commit()?;

    tracing::info!(
        target: "asuna_memory::index::rebuild",
        "向量嵌入进度: {}/{} (事务批 {}/{})",
        counters.vectors_indexed,
        ctx.total,
        tx_idx + 1,
        ctx.num_db_batches
    );

    if let Some(cb) = ctx.on_progress {
        cb(
            ctx.sessions_processed,
            ctx.turns_indexed,
            counters.vectors_indexed,
            ctx.total,
        );
    }

    Ok(())
}

pub(super) fn insert_quantized_vector_rows(
    tx: &rusqlite::Transaction<'_>,
    rows: &[(i64, Vec<u8>)],
    counters: &mut VectorBatchCounters,
) -> anyhow::Result<()> {
    use std::collections::HashSet;
    // W26b: 批量预取存活 turn id 与已索引 rowid（每 chunk 2 条查询）取代
    // 逐行 live/check 探针（2*N+1）；分块复用 sql_util 防 SQL 变量数上限。
    let ids: Vec<i64> = rows.iter().map(|&(id, _)| id).collect();
    let mut live_ids: HashSet<i64> = HashSet::new();
    let mut indexed_ids: HashSet<i64> = HashSet::new();
    crate::index::sql_util::for_each_in_chunk(&ids, |chunk, placeholders| -> anyhow::Result<()> {
        let mut live_stmt = tx.prepare(&format!(
            "SELECT id FROM turns WHERE id IN ({placeholders})"
        ))?;
        let live_rows = live_stmt.query_map(rusqlite::params_from_iter(chunk), |r| r.get(0))?;
        for id in live_rows {
            live_ids.insert(id?);
        }
        let mut check_stmt = tx.prepare(&format!(
            "SELECT rowid FROM vec_turns_rowids WHERE rowid IN ({placeholders})"
        ))?;
        let indexed_rows = check_stmt.query_map(rusqlite::params_from_iter(chunk), |r| r.get(0))?;
        for id in indexed_rows {
            indexed_ids.insert(id?);
        }
        Ok(())
    })?;
    let mut insert_stmt =
        tx.prepare_cached("INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))")?;
    for (turn_id, embedding_bytes) in rows {
        if !live_ids.contains(turn_id) {
            tracing::debug!(
                target: "asuna_memory::index::rebuild",
                "turn_id={} 已不存在，跳过向量插入（防孤儿向量）",
                turn_id
            );
            continue;
        }
        if indexed_ids.contains(turn_id) {
            tracing::debug!(
                target: "asuna_memory::index::rebuild",
                "turn_id={} 已被并发写入索引，跳过",
                turn_id
            );
            counters.vectors_indexed += 1;
            continue;
        }
        match insert_stmt.execute(rusqlite::params![*turn_id, embedding_bytes]) {
            Ok(_) => {
                counters.vectors_indexed += 1;
                // 同批重复 id 的后续行走"已索引"分支（与旧逐行探针语义一致）。
                indexed_ids.insert(*turn_id);
            }
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::index::rebuild",
                    "向量插入失败 turn_id={}: {}",
                    turn_id,
                    e
                );
                counters.failed_count += 1;
                if counters.errors.len() < MAX_ERR_SAMPLES {
                    counters
                        .errors
                        .push(format!("向量插入失败 turn_id={}: {}", turn_id, e));
                }
            }
        }
    }
    Ok(())
}

/// 事务外嵌入一个批次并量化为 int8 字节（纯内存）；嵌入失败仅记录不中断
/// （语义与原事务内 embed_and_insert_chunk 一致：failed_count += 批大小，
/// 错误样本受 MAX_ERR_SAMPLES 上限，整批跳过）。
fn embed_quantize_chunk(
    embedder: &crate::embedder::LazyEmbedder,
    embed_chunk: &[(i64, String)],
    rows: &mut Vec<(i64, Vec<u8>)>,
    failed_count: &mut usize,
    errors: &mut Vec<String>,
) {
    let texts: Vec<&str> = embed_chunk.iter().map(|(_, p)| p.as_str()).collect();
    let embeddings = match embedder.embed_documents(&texts) {
        Ok(embs) => embs,
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::index::rebuild",
                "批量嵌入失败: {}",
                e
            );
            *failed_count += texts.len();
            if errors.len() < MAX_ERR_SAMPLES {
                errors.push(format!("批量嵌入失败 ({} 条): {}", texts.len(), e));
            }
            return;
        }
    };

    for ((turn_id, _), embedding) in embed_chunk.iter().zip(embeddings.iter()) {
        // W2.4: 批量路径复用 W13 单条守卫——全零/非有限向量量化为全零 int8，
        // cosine 距离 NaN/NULL 排最前吃 LIMIT k，截断语义搜索。跳过+warn+计 failed。
        // （非有限半部上游 l2_normalize 已拒，此处主要防 API 返回全零向量。）
        if crate::index::vector::is_invalid_vector(embedding) {
            tracing::warn!(
                target: "asuna_memory::index::rebuild",
                "turn_id={} 向量为零/非有限，跳过向量索引（rebuild 批量路径）",
                turn_id
            );
            *failed_count += 1;
            if errors.len() < MAX_ERR_SAMPLES {
                errors.push(format!("turn_id={} 向量为零/非有限，已跳过", turn_id));
            }
            continue;
        }
        rows.push((*turn_id, crate::embedder::quantize_to_int8(embedding)));
    }
}
