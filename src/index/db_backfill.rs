use super::db::Db;

/// 401/403 are permanent auth rejections: no retry can succeed, and walking
/// the whole pending list would stall shutdown for minutes.
pub fn is_permanent_backfill_error(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<crate::embedder::EmbedApiError>()
            .is_some_and(|err| matches!(err.status, Some(401 | 403)))
            || cause
                .downcast_ref::<ureq::Error>()
                .is_some_and(|ue| matches!(ue, ureq::Error::StatusCode(401 | 403)))
    })
}

/// Abort the backfill after this many consecutive chunk failures (W28b).
pub const MAX_CONSECUTIVE_BACKFILL_FAILURES: usize = 3;

/// Outcome of one backfill chunk (W28b). `Permanent` means every remaining
/// chunk would fail identically, so the whole run aborts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackfillChunkOutcome {
    Done,
    Failed,
    Permanent,
}

fn should_continue_backfill(
    outcome: BackfillChunkOutcome,
    consecutive_failures: &mut usize,
) -> bool {
    match outcome {
        BackfillChunkOutcome::Done => {
            *consecutive_failures = 0;
            true
        }
        BackfillChunkOutcome::Permanent => {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "vec_bounded_memory backfill aborted: permanent embedding error (auth/permission)"
            );
            false
        }
        BackfillChunkOutcome::Failed => {
            *consecutive_failures += 1;
            if *consecutive_failures >= MAX_CONSECUTIVE_BACKFILL_FAILURES {
                tracing::warn!(
                    target: "asuna_memory::index::db",
                    "vec_bounded_memory backfill aborted after {} consecutive chunk failures",
                    *consecutive_failures
                );
                return false;
            }
            true
        }
    }
}

/// Single backfill control loop shared by startup, MCP and rebuild callers.
pub fn run_backfill_chunks_with_control<T>(
    pending: &[T],
    batch_size: usize,
    shutdown: Option<&std::sync::atomic::AtomicBool>,
    mut process_chunk: impl FnMut(&[T]) -> anyhow::Result<BackfillChunkOutcome>,
) -> anyhow::Result<bool> {
    let mut consecutive_failures = 0usize;
    for chunk in pending.chunks(batch_size.max(1)) {
        if shutdown.is_some_and(|s| s.load(std::sync::atomic::Ordering::SeqCst)) {
            tracing::info!(
                target: "asuna_memory::index::db",
                "vec_bounded_memory backfill stopped: shutting down"
            );
            return Ok(false);
        }
        if !should_continue_backfill(process_chunk(chunk)?, &mut consecutive_failures) {
            return Ok(false);
        }
    }
    Ok(true)
}

enum BackfillRowOutcome {
    Inserted,
    Skipped,
    InvalidVec,
    InsertFailed,
}

impl Db {
    /// Rows the vec backfill considers (bounded_memory atoms without a vector
    /// index). Extracted from [`Self::maybe_backfill_bounded_memory_vec`] as
    /// the DB-visible candidate set so the supersede exclusion is testable
    /// without a live embedder.
    ///
    /// C14-c: rows superseded by a newer entry must NEVER be re-indexed —
    /// supersede de-indexes them on purpose, and a candidate query that
    /// ignores the chain resurrects every superseded vector at startup,
    /// defeating the whole invalidation chain (idx_bounded_memory_supersedes
    /// keeps the NOT EXISTS probe cheap).
    ///
    /// D5-3: 已索引排除下推到 SQL 反连接（NOT EXISTS vec_bounded_memory），
    /// 不再把全部 vec id 拉进内存 HashSet 做 Rust 侧过滤。
    pub(super) fn bounded_memory_backfill_candidates(&self) -> anyhow::Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT bm.id, bm.content FROM bounded_memory bm
             WHERE COALESCE(bm.memory_type, 'manual') = 'atom'
               AND bm.content IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM vec_bounded_memory v WHERE v.id = bm.id
               )",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn insert_backfill_vec_row(
        live_stmt: &mut rusqlite::CachedStatement<'_>,
        already_stmt: &mut rusqlite::CachedStatement<'_>,
        ins_stmt: &mut rusqlite::CachedStatement<'_>,
        id: i64,
        embedding: &[f32],
    ) -> anyhow::Result<BackfillRowOutcome> {
        let live_atom: bool = live_stmt.query_row(rusqlite::params![id], |r| r.get(0))?;
        if !live_atom {
            return Ok(BackfillRowOutcome::Skipped);
        }
        let already_indexed: i64 = already_stmt.query_row(rusqlite::params![id], |r| r.get(0))?;
        if already_indexed > 0 {
            return Ok(BackfillRowOutcome::Skipped);
        }
        // W2.4: 批量路径复用 W13 单条守卫——全零/非有限向量量化为全零 int8，
        // cosine 距离 NaN/NULL 排最前吃 LIMIT k，截断语义搜索。跳过+warn。
        if crate::index::vector::is_invalid_vector(embedding) {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "vec_bounded_memory backfill: id={} 向量为零/非有限，跳过向量索引",
                id
            );
            return Ok(BackfillRowOutcome::InvalidVec);
        }
        let bytes = crate::embedder::quantize_to_int8(embedding);
        if let Err(e) = ins_stmt.execute(rusqlite::params![id, bytes]) {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "vec_bounded_memory backfill: insert id={} failed: {}",
                id,
                e
            );
            return Ok(BackfillRowOutcome::InsertFailed);
        }
        Ok(BackfillRowOutcome::Inserted)
    }

    /// Insert one chunk of backfill embeddings inside an IMMEDIATE transaction,
    /// re-checking liveness (not deleted / not superseded) and duplicate state.
    pub fn insert_backfill_vec_chunk(
        &self,
        chunk: &[(i64, String)],
        embeddings: &[Vec<f32>],
    ) -> anyhow::Result<()> {
        // W8: zip 会静默截断到较短一侧——长度不等必须在开事务前拒绝，
        // 否则部分 atom 静默丢失向量索引且上层视同成功。
        anyhow::ensure!(
            chunk.len() == embeddings.len(),
            "vec_bounded_memory backfill: chunk/embeddings 长度不一致 ({} vs {})",
            chunk.len(),
            embeddings.len()
        );
        let tx = self.immediate_tx()?;
        // W28f: 循环外 prepare_cached，避免逐行重复编译（回填可达十万行）。
        // W12: propagate SQLite query errors instead of masking them with unwrap_or.
        let mut live_stmt = tx.prepare_cached(
            "SELECT EXISTS(
                SELECT 1 FROM bounded_memory bm
                WHERE bm.id = ?1
                  AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
            )",
        )?;
        let mut already_stmt =
            tx.prepare_cached("SELECT COUNT(*) FROM vec_bounded_memory WHERE id = ?1")?;
        let mut ins_stmt = tx.prepare_cached(
            "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
        )?;
        let mut insert_failed = 0usize;
        // N3: 零/非有限向量跳过原仅逐行 warn，批量路径无汇总计数——与 rebuild
        // failed_count 对齐可观测性，累加 skipped 供上层日志（不改 Result<()>
        // 签名，transport/http/backfill.rs 共用本函数）。
        let mut skipped_invalid = 0usize;
        for ((id, _), embedding) in chunk.iter().zip(embeddings.iter()) {
            match Self::insert_backfill_vec_row(
                &mut live_stmt,
                &mut already_stmt,
                &mut ins_stmt,
                *id,
                embedding,
            )? {
                BackfillRowOutcome::Inserted | BackfillRowOutcome::Skipped => {}
                BackfillRowOutcome::InvalidVec => skipped_invalid += 1,
                BackfillRowOutcome::InsertFailed => insert_failed += 1,
            }
        }
        drop(live_stmt);
        drop(already_stmt);
        drop(ins_stmt);
        tx.commit()?;
        // N3: 汇总本 chunk 因零/非有限被跳过的 atom 数，供上层日志可感知
        // （逐行 warn 之上补批量计数，对齐 rebuild failed_count 姿势）。
        if skipped_invalid > 0 {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "vec_bounded_memory backfill: {} atom 向量因零/非有限被跳过（本次不索引，下次启动回填重试）",
                skipped_invalid
            );
        }
        // D3-4: 逐行插入失败必须让上层可感知（原 warn-only 仍返 Ok，启动回填/
        // rebuild 视同成功）。成功行已提交，失败行留待下次启动重试。
        if insert_failed > 0 {
            anyhow::bail!(
                "vec_bounded_memory backfill: {} atom vector insert(s) failed",
                insert_failed
            );
        }
        Ok(())
    }

    /// Purge stale vectors of superseded atoms and return pending `(id, content)`
    /// pairs that lack vector embeddings in `vec_bounded_memory`.
    pub fn prepare_bounded_memory_vec_backfill(&self) -> anyhow::Result<Vec<(i64, String)>> {
        // D3-4: purge 失败上抛（原 warn 后继续）——失败意味着被取代原子的陈旧
        // 向量仍参与检索，静默继续会让调用方误以为索引干净。
        let purged = self.conn.execute(
            "DELETE FROM vec_bounded_memory WHERE id IN (
                 SELECT bm.id FROM bounded_memory bm
                 WHERE EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
             )",
            [],
        )?;
        if purged > 0 {
            tracing::info!(
                target: "asuna_memory::index::db",
                "vec_bounded_memory: purged {} stale vector(s) of superseded atoms \
                 (resurrected by a pre-C14-c backfill)",
                purged
            );
        }

        // D5-3: 候选查询已含 vec 反连接（见 bounded_memory_backfill_candidates），
        // 不再拉全量 vec id HashSet 做 Rust 侧过滤。
        self.bounded_memory_backfill_candidates()
    }

    /// Backfill vec_bounded_memory for atoms that have no vector index.
    ///
    /// This handles the case where vec_bounded_memory was wiped (e.g., after a
    /// float32→int8 schema migration) but bounded_memory entries still exist.
    /// Without this, semantic search on bounded memory silently degrades to FTS.
    pub fn maybe_backfill_bounded_memory_vec(
        &self,
        embedder: &crate::embedder::LazyEmbedder,
    ) -> anyhow::Result<()> {
        self.backfill_bounded_memory_vec_with_control(embedder, None)
    }

    fn run_backfill_vec_chunk(
        &self,
        embedder: &crate::embedder::LazyEmbedder,
        chunk: &[(i64, String)],
    ) -> anyhow::Result<BackfillChunkOutcome> {
        let texts: Vec<&str> = chunk.iter().map(|(_, c)| c.as_str()).collect();
        match embedder.embed_documents(&texts) {
            Ok(embeddings) => {
                self.insert_backfill_vec_chunk(chunk, &embeddings)?;
                Ok(BackfillChunkOutcome::Done)
            }
            Err(e) => {
                tracing::warn!(
                    target: "asuna_memory::index::db",
                    "vec_bounded_memory backfill: batch embed failed: {}",
                    e
                );
                if is_permanent_backfill_error(&e) {
                    Ok(BackfillChunkOutcome::Permanent)
                } else {
                    Ok(BackfillChunkOutcome::Failed)
                }
            }
        }
    }

    /// W11: Backfill with optional shutdown cancellation, permanent-error abort (401/403),
    /// and 3-failure circuit breaker (shared policy with HTTP gateway backfill).
    pub fn backfill_bounded_memory_vec_with_control(
        &self,
        embedder: &crate::embedder::LazyEmbedder,
        shutdown: Option<&std::sync::atomic::AtomicBool>,
    ) -> anyhow::Result<()> {
        let pending = self.prepare_bounded_memory_vec_backfill()?;

        if pending.is_empty() {
            return Ok(());
        }

        tracing::info!(
            target: "asuna_memory::index::db",
            "Backfilling vec_bounded_memory: {} atoms need embeddings...",
            pending.len()
        );

        let batch_size = embedder.batch_size().max(1);
        if !run_backfill_chunks_with_control(&pending, batch_size, shutdown, |chunk| {
            self.run_backfill_vec_chunk(embedder, chunk)
        })? {
            return Ok(());
        }

        let final_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM vec_bounded_memory", [], |r| r.get(0))
            .unwrap_or(0);

        tracing::info!(
            target: "asuna_memory::index::db",
            "vec_bounded_memory backfill complete: {} vectors total",
            final_count
        );
        Ok(())
    }

    /// N1: 活 atom 中仍缺向量索引的条数。存活/已索引判定与
    /// [`Self::bounded_memory_backfill_candidates`] 完全一致（atom 类型 + 非空
    /// content + 未被取代 + 未索引），但只 COUNT 不物化 content（对齐 D5-3 SQL
    /// 侧计数）；修改判定须同步两处。
    ///
    /// rebuild 在 atom 重嵌入后调用本方法校验缺口：maybe_backfill 的 batch embed
    /// 失败 warn+continue 仍返 Ok，Phase 1 已清空旧向量，API 故障会静默留缺口，
    /// 须由调用方计入 stats.errors（下次启动 maybe_backfill 自动续嵌）。
    pub fn count_atoms_missing_vec(&self) -> anyhow::Result<usize> {
        let missing: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM bounded_memory bm
             WHERE COALESCE(bm.memory_type, 'manual') = 'atom'
               AND bm.content IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM vec_bounded_memory v WHERE v.id = bm.id
               )",
            [],
            |r| r.get(0),
        )?;
        Ok(missing.max(0) as usize)
    }
}
