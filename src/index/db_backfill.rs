use super::db::Db;
use std::collections::HashSet;
use std::path::Path;

/// Outcome of the non-blocking cross-process backfill cost-guard lock attempt
/// (`<profile>/backfill.lock`).
///
/// Pure COST guard: correctness is already guaranteed by the in-tx live+indexed
/// recheck in [`Db::insert_backfill_vec_chunk`]; this only stops N processes on
/// the same profile from paying the same embedding API cost concurrently.
pub enum BackfillLockOutcome {
    /// Lock acquired — hold the `File` for the whole run; it auto-releases on drop.
    Acquired(std::fs::File),
    /// Another process holds the lock: skip this run. The holder's drain loop
    /// keeps re-indexing while each pass makes progress (it terminates on a pass
    /// that clears none), so this process's pending atoms are indexed by the
    /// holder in the common case; any residual (e.g. unindexable atoms, or a pass
    /// that clears none under sustained inflow) is picked up at a later startup.
    /// Pure cost/latency guard — correctness is the in-tx live+indexed recheck.
    Contended,
    /// Lock file could not be opened/locked (exotic FS): warned; run UNGUARDED.
    /// Never blocks startup; the in-tx recheck still prevents duplicate vectors.
    Degraded,
}

/// Try to acquire the profile-wide backfill cost guard without blocking.
///
/// Uses `std::fs::File::try_lock` (stable 1.89): Unix `flock(LOCK_EX|LOCK_NB)`,
/// Windows `LockFileEx(EXCLUSIVE|FAIL_IMMEDIATELY)`. Open `.read(true).write(true)
/// .create(true)` (plain create, NOT create_new; Windows requires read/write access
/// to lock). `backfill.lock` matches NEITHER the tmp-sweep `.{name}.{pid}.{seq}.tmp`
/// shape NOR offload `.offload_*.tmp`, so sweepers never delete it; it stays empty
/// and permanent (a harmless orphan). The OS releases the lock on close/process
/// death — no stale cleanup. A fresh handle is opened per attempt; never reuse a
/// locked handle (re-locking is unspecified / may deadlock).
pub fn try_acquire_backfill_lock(profile_dir: &Path) -> BackfillLockOutcome {
    if let Err(e) = std::fs::create_dir_all(profile_dir) {
        tracing::warn!(
            target: "asuna_memory::index::db",
            "backfill.lock 目录创建失败 {}（{}）：本次回填不加跨进程成本守卫",
            profile_dir.display(),
            e
        );
        return BackfillLockOutcome::Degraded;
    }
    let lock_path = profile_dir.join("backfill.lock");
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "backfill.lock 打开失败 {}（{}）：本次回填不加跨进程成本守卫",
                lock_path.display(),
                e
            );
            return BackfillLockOutcome::Degraded;
        }
    };
    match file.try_lock() {
        Ok(()) => BackfillLockOutcome::Acquired(file),
        Err(std::fs::TryLockError::WouldBlock) => BackfillLockOutcome::Contended,
        Err(std::fs::TryLockError::Error(e)) => {
            tracing::warn!(
                target: "asuna_memory::index::db",
                "backfill.lock 加锁失败 {}（{}）：本次回填不加跨进程成本守卫",
                lock_path.display(),
                e
            );
            BackfillLockOutcome::Degraded
        }
    }
}

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

/// D7: shared drain loop for the startup (HTTP gateway) and Db/MCP/rebuild backfill
/// callers. Recompute candidates until a pass clears ZERO of the candidates it
/// processed, then stop. Under sustained concurrent inflow it keeps making progress
/// (newly committed atoms get indexed this run, honoring the for_ai §8.8 drain
/// promise) until shutdown; it never spins on unindexable atoms because a pass that
/// clears none breaks (the zero/invalid-vector termination property).
///
/// `fetch` returns the current candidate set: `Ok(None)` stops gracefully (the HTTP
/// path warns and skips on a fetch error), `Err` aborts (the Db path propagates).
/// `run_pass` processes one snapshot in chunks, returning `Ok(false)` to stop
/// (shutdown / permanent error / failure breaker) or `Ok(true)` when the pass ran to
/// completion. `shutdown` is polled between passes. Returns `true` iff at least one
/// non-empty pass ran (the Db caller gates its "complete" log on this to keep the
/// zero-candidate startup silent, matching the pre-drain early-return behavior).
pub fn drain_backfill_until_no_progress(
    mut fetch: impl FnMut() -> anyhow::Result<Option<Vec<(i64, String)>>>,
    mut run_pass: impl FnMut(&[(i64, String)]) -> anyhow::Result<bool>,
    shutdown: Option<&std::sync::atomic::AtomicBool>,
) -> anyhow::Result<bool> {
    let Some(mut pending) = fetch()? else {
        return Ok(false);
    };
    let mut did_work = false;
    loop {
        if pending.is_empty() {
            return Ok(did_work);
        }
        did_work = true;
        let processed: HashSet<i64> = pending.iter().map(|(id, _)| *id).collect();
        tracing::info!(
            target: "asuna_memory::index::db",
            "Backfilling vec_bounded_memory: {} atoms need embeddings...",
            pending.len()
        );
        if !run_pass(&pending)? {
            return Ok(true);
        }
        if shutdown.is_some_and(|s| s.load(std::sync::atomic::Ordering::SeqCst)) {
            return Ok(true);
        }
        let Some(next) = fetch()? else {
            return Ok(true);
        };
        // Progress = how many of THIS pass's candidates left the pending set.
        // Concurrent inflow can grow `next` without bound; only ZERO progress
        // terminates, so unindexable atoms (which never leave pending) still break
        // the loop while genuine progress keeps draining until shutdown.
        let remaining = next.iter().filter(|(id, _)| processed.contains(id)).count();
        let cleared = processed.len() - remaining;
        pending = next;
        if cleared == 0 {
            return Ok(true);
        }
    }
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
        // STEP 6 drain loop (shared D7 helper): the cost-guard holder recomputes
        // candidates until a pass clears none BEFORE releasing backfill.lock, so
        // atoms committed by concurrent writers (races N1/C5) — including processes
        // that SKIPPED because we hold the lock — still get indexed this run instead
        // of waiting for next startup.
        let batch_size = embedder.batch_size().max(1);
        let did_work = drain_backfill_until_no_progress(
            || self.prepare_bounded_memory_vec_backfill().map(Some),
            |pending| {
                run_backfill_chunks_with_control(pending, batch_size, shutdown, |chunk| {
                    self.run_backfill_vec_chunk(embedder, chunk)
                })
            },
            shutdown,
        )?;

        // 零候选启动保持静默（与 drain 前的 early-return 一致）：仅有实际回填才记
        // complete 日志，避免差分 harness 的 stderr golden 多出一行。
        if did_work {
            let final_count: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM vec_bounded_memory", [], |r| r.get(0))
                .unwrap_or(0);
            tracing::info!(
                target: "asuna_memory::index::db",
                "vec_bounded_memory backfill complete: {} vectors total",
                final_count
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    fn tmp_dir(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "asuna_bflock_{}_{}_{}_{}",
            tag,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
            seq
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// First attempt acquires; a FRESH handle while held is Contended (skip);
    /// after the holder drops, a new attempt acquires again. Same-process
    /// separate handles conflict — a valid proxy for cross-process flock.
    #[test]
    fn backfill_lock_acquire_contend_release() {
        let dir = tmp_dir("guard");
        let first = try_acquire_backfill_lock(&dir);
        assert!(matches!(first, BackfillLockOutcome::Acquired(_)));
        assert!(dir.join("backfill.lock").exists());
        assert!(
            matches!(
                try_acquire_backfill_lock(&dir),
                BackfillLockOutcome::Contended
            ),
            "second fresh handle must be excluded while held"
        );
        drop(first);
        let third = try_acquire_backfill_lock(&dir);
        assert!(
            matches!(third, BackfillLockOutcome::Acquired(_)),
            "lock must be reusable after the holder drops"
        );
        drop(third);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// `backfill.lock` matches neither the tmp-sweep shape nor offload tmp, so
    /// the sweeper (min_age=0) removes orphan tmp files but never the lock file.
    #[test]
    fn backfill_lock_survives_tmp_sweep() {
        let dir = tmp_dir("sweep");
        drop(try_acquire_backfill_lock(&dir));
        let lock = dir.join("backfill.lock");
        assert!(lock.exists());
        let orphan = dir.join(".memory.db.999999.0.tmp");
        std::fs::write(&orphan, b"stale").unwrap();
        let removed = crate::util::sweep_orphan_tmp_files(&dir, Duration::ZERO);
        assert_eq!(removed, 1, "orphan tmp must be swept");
        assert!(lock.exists(), "backfill.lock must survive the sweep");
        assert!(!orphan.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Open/lock failure degrades to UNGUARDED (warn + Degraded), never panics or
    /// blocks startup. Here profile_dir is a regular FILE, so create_dir_all fails.
    #[test]
    fn backfill_lock_degrades_non_blocking_on_error() {
        let dir = tmp_dir("degrade");
        let file_as_dir = dir.join("not_a_dir");
        std::fs::write(&file_as_dir, b"x").unwrap();
        let outcome = try_acquire_backfill_lock(&file_as_dir);
        assert!(matches!(outcome, BackfillLockOutcome::Degraded));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// D7 drain helper: concurrent inflow grows the candidate set beyond the processed
    /// count, but genuine progress was made — the loop must KEEP draining (old guard
    /// `pending.len() >= processed` broke early and stranded the inflow atoms).
    #[test]
    fn drain_keeps_progress_under_inflow() {
        use std::cell::RefCell;
        let pending = RefCell::new(vec![(1i64, "a".to_string()), (2, "b".to_string())]);
        let passes = RefCell::new(0usize);
        let fetch = || Ok(Some(pending.borrow().clone()));
        let run_pass = |batch: &[(i64, String)]| {
            let ids: HashSet<i64> = batch.iter().map(|(id, _)| *id).collect();
            let mut p = pending.borrow_mut();
            p.retain(|(id, _)| !ids.contains(id));
            let mut n = passes.borrow_mut();
            *n += 1;
            if *n == 1 {
                // inflow: 3 new atoms arrive mid-drain (next.len()=3 > processed=2).
                p.extend([
                    (3, "c".to_string()),
                    (4, "d".to_string()),
                    (5, "e".to_string()),
                ]);
            }
            Ok(true)
        };
        drain_backfill_until_no_progress(fetch, run_pass, None).unwrap();
        assert!(
            pending.borrow().is_empty(),
            "inflow atoms must be drained too"
        );
        assert_eq!(
            *passes.borrow(),
            2,
            "one pass per non-empty candidate snapshot"
        );
    }

    /// D7 drain helper: a pass that clears ZERO candidates (unindexable atom never
    /// leaves pending) must terminate after one pass, not spin forever.
    #[test]
    fn drain_terminates_on_zero_progress() {
        use std::cell::RefCell;
        let pending = RefCell::new(vec![(1i64, "invalid".to_string())]);
        let passes = RefCell::new(0usize);
        let fetch = || Ok(Some(pending.borrow().clone()));
        let run_pass = |_batch: &[(i64, String)]| {
            *passes.borrow_mut() += 1;
            Ok(true) // clears nothing: zero/invalid vector skipped by in-tx recheck
        };
        drain_backfill_until_no_progress(fetch, run_pass, None).unwrap();
        assert_eq!(
            *passes.borrow(),
            1,
            "zero-progress pass must break, not spin"
        );
        assert_eq!(pending.borrow().len(), 1, "unindexable atom stays pending");
    }
}
