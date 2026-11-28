use super::{
    bump_md_generation, md_generation, write_md_render_locked, BoundedMemory, PendingMdWrite,
};
use std::collections::HashSet;

pub(super) const ENTRY_SEPARATOR: &str = "\n§\n";

/// Normalize separator-position `§` variants and split into trimmed non-empty entries.
pub(super) fn split_entries(content: &str) -> Vec<String> {
    // W2.3: 拆分前先 CRLF→LF 归一——历史坏行 "a\r\n§\r\nb" 的 § 邻接 \r，
    // normalize_separators 只认 '\n' 边界会整行当单条目，split 永不收敛。
    let mut subs: Vec<String> = normalize_separators(&normalize_crlf(content))
        .split(ENTRY_SEPARATOR)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    loop {
        let mut changed = false;
        let mut next: Vec<String> = Vec::with_capacity(subs.len());
        for sub in &subs {
            let renorm = normalize_separators(sub);
            if renorm == sub.as_str() {
                next.push(sub.clone());
                continue;
            }
            for p in renorm.split(ENTRY_SEPARATOR) {
                let p = p.trim();
                if !p.is_empty() {
                    next.push(p.to_string());
                }
            }
            changed = true;
        }
        subs = next;
        if !changed {
            break;
        }
    }
    subs
}

/// Normalize boundary-adjacent `§` separators to canonical `"\n§\n"`,
/// leaving mid-content `§` (e.g. `"see §5"`) untouched.
pub(super) fn normalize_separators(content: &str) -> String {
    let chars: Vec<char> = content.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(content.len() + 16);
    for (i, &c) in chars.iter().enumerate() {
        if c != '§' {
            out.push(c);
            continue;
        }
        let prev_ok = i == 0 || chars[i - 1] == '\n';
        let next_ok = i + 1 == n || chars[i + 1] == '\n';
        if !prev_ok || !next_ok {
            out.push('§');
            continue;
        }
        if i == 0 {
            out.push('\n');
        }
        out.push('§');
        if i + 1 == n {
            out.push('\n');
        }
    }
    out
}

/// W2.3: 入口统一 CRLF→LF。write/update/remove/reinsert 存库与渲染前归一，使
/// DB 行与 extract_body（`lines()` 已把 \r\n 归一为 \n）渲染出的 .md 永远同形。
/// 否则 CRLF 多行内容恒判 DIVERGED，doctor --fix 回插 LF 版本制造永不收敛的重复。
/// 只折叠 \r\n 对；裸 \r（旧 Mac 行尾）不是渲染分歧源，保持原样不动。
/// L1 原子直插（memory::l1::plan）同样经此归一，保持同一不变量。
pub(crate) fn normalize_crlf(content: &str) -> String {
    content.replace("\r\n", "\n")
}

/// 安全截取字符串前 N 个 Unicode 字符（不会切断 UTF-8 多字节序列）
pub(super) fn truncate_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// `content` 是否含条目分隔符或其截断变体（write()/update() 的同一守卫）。
/// L1 原子直接 SQL 入库前用它拦截，否则 render_md_from_db 原样 join 会伪分隔。
/// W2.3: 守卫内部先 CRLF→LF 归一再判——normalize_separators 的 prev_ok/next_ok 只认
/// '\n'，CRLF 分隔符变体（"a\r\n§\r\nb"）会绕过原始文本守卫；L1 用原始 atom.content
/// 调它，归一收进守卫内部一处即关闭所有调用方（含 L1 直插）。
pub(crate) fn has_entry_separator(content: &str) -> bool {
    let c = normalize_crlf(content);
    let trimmed = c.trim();
    c.contains(ENTRY_SEPARATOR)
        || normalize_separators(&c) != c
        || normalize_separators(trimmed) != trimmed
}

/// 维度7: write/update/remove 共用的分隔符守卫（原三处 `contains || normalize !=`
/// 加 bail 逐字重复）。含 canonical `\n§\n` 或其截断/规范化变体即拒——一次只能写
/// 一个条目。`field` 进错误文案，`hint` 给出各入口的补救指引。
/// has_entry_separator 内部已归一 CRLF（W2.3），CRLF 分隔符变体（"a\r\n§\r\nb"）同样被拦；调用方先归一为幂等。
pub(super) fn reject_entry_separator(content: &str, field: &str, hint: &str) -> anyhow::Result<()> {
    if has_entry_separator(content) {
        anyhow::bail!("{field} 不能包含条目分隔符 '\\n§\\n' 或其截断变体（{hint}）");
    }
    Ok(())
}

/// 从完整文件内容中提取条目正文（去掉元数据头）
pub(crate) fn extract_body(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    let lines: Vec<&str> = content.lines().collect();
    // M8: 首行必须是整行 header 注释；第 2 行只有真的是空行才跳过——
    // 手改 .md 删掉空行后，旧实现会把首条正文当空行静默丢弃。
    let header_line = lines.first().is_some_and(|l| {
        let t = l.trim();
        t.starts_with("<!-- ASUNA") && t.ends_with("-->")
    });
    let start = if header_line {
        if lines.get(1).is_some_and(|l| l.trim().is_empty()) {
            2
        } else {
            1
        }
    } else {
        0
    };
    lines
        .get(start..)
        .map(|s| s.join("\n"))
        .unwrap_or_default()
        .trim()
        .to_string()
}

impl<'a> BoundedMemory<'a> {
    /// Render markdown output from DB under lock for deferred atomic write.
    pub(super) fn render_md_from_db(
        &self,
        target: &str,
    ) -> anyhow::Result<(usize, PendingMdWrite)> {
        let capacity = self.capacity(target);
        let mut stmt = self.db.conn().prepare(
            "SELECT bm.content FROM bounded_memory bm
             WHERE bm.target = ?1 AND COALESCE(bm.memory_type, 'manual') != 'scenario'
               AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
             ORDER BY bm.created_at, bm.id",
        )?;
        let all_entries: Vec<String> = stmt
            .query_map(rusqlite::params![target], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let new_body = all_entries.join(ENTRY_SEPARATOR);
        let body_chars = new_body.chars().count();
        if body_chars > capacity {
            tracing::warn!(
                "rebuild_md_from_db[{}]: 重建后总长 {} 超出容量 {}，写入会携带警告",
                target,
                body_chars,
                capacity
            );
        }
        let header = self.metadata_header(target, capacity);
        let full = if new_body.trim().is_empty() {
            format!("{}\n\n", header)
        } else {
            format!("{}\n\n{}", header, new_body)
        };
        let path = self.target_file(target)?;
        // C8: 代际在 DB 锁内、渲染之后记录——快照据此在 flush 时自证是否过期。
        let generation = md_generation(&path);
        super::set_unflushed_snapshot(&path, generation, &full);
        Ok((
            all_entries.len(),
            PendingMdWrite {
                path,
                content: full,
                generation,
                flushed: false,
                memory_limit: self.memory_limit,
                user_limit: self.user_limit,
            },
        ))
    }

    /// Immediate `.md` render-write under the cross-process flock shell.
    ///
    /// R2 lock order: in-process `write_lock` FIRST, then flock, then bump the
    /// generation, then the DB SELECT render (`render_md_from_db`, which records
    /// the unflushed snapshot), then `atomic_write`. The caller must NOT hold an
    /// open SQLite WRITE tx (R1) nor L_gw (R2) across this call — the DB SELECT
    /// here is autocommit. Does NOT call `PendingMdWrite::flush` (the std Mutex
    /// `write_lock` is non-reentrant and already held by the shell).
    ///
    /// The pre-tx read in write/update/remove still drives dup/capacity/match
    /// validation; the in-tx DB re-check guards dup, and this re-render makes the
    /// disk converge to the live DB row set (DB-authority).
    pub(crate) fn render_write_md_locked(&self, target: &str) -> anyhow::Result<usize> {
        let path = self.target_file(target)?;
        let mut count = 0usize;
        write_md_render_locked(&path, || {
            bump_md_generation(&path);
            let (n, mut pending) = self.render_md_from_db(target)?;
            count = n;
            // The shell records and clears its own snapshot; mark this inner
            // snapshot flushed so its `Drop` cannot clear the registry entry the
            // shell just recorded (W5: on write fail that snapshot must survive
            // so read() still sees the committed content).
            pending.flushed = true;
            Ok((pending.generation, std::mem::take(&mut pending.content)))
        })?;
        Ok(count)
    }

    /// 从 DB 全量重建目标 target 的 .md 文件（覆盖写入）。
    pub(super) fn rebuild_md_from_db(&self, target: &str) -> anyhow::Result<usize> {
        self.render_write_md_locked(target)
    }

    fn evict_atoms_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        atoms: &[(i64, String, bool)],
        manual_chars: usize,
        atom_budget: usize,
        capacity: usize,
    ) -> anyhow::Result<usize> {
        // 维度2: 每条目 +3 是分隔符 "\n§\n"（3 字符）的占位 allowance。render 的
        // join（render_md_from_db）实际只用 n-1 个分隔符，故总账恒多计一个分隔符
        // （3 字符）——偏保守（略微提前驱逐），安全。manual_chars 同口径，两者与
        // join 渲染面可比；不改成 n-1 以免与既有驱逐账目/测试的 +3 约定漂移。
        let mut remaining_atom: usize = atoms
            .iter()
            .filter(|(_, _, superseded)| !superseded)
            .map(|(_, c, _)| c.chars().count() + 3)
            .sum();

        let mut evicted = 0usize;
        let mut freed = 0usize;
        for (id, content, superseded) in atoms {
            if remaining_atom <= atom_budget && manual_chars + remaining_atom <= capacity {
                break;
            }
            Self::delete_evicted_atom(tx, *id)?;
            let c = content.chars().count() + 3;
            if !superseded {
                remaining_atom = remaining_atom.saturating_sub(c);
            }
            // 维度7: freed 累计被删行的内容字符（含 superseded 死重——它本就不
            // 渲染进 .md），是「清掉多少 DB 行内容」口径，而非「MEMORY.md 渲染面
            // 腾出多少字符」。remaining_atom 只对活 atom 扣减，故收敛判据不受影响。
            freed += c;
            evicted += 1;
        }
        if evicted > 0 {
            self.record_eviction_audit(evicted, freed, atom_budget, capacity);
        }
        Ok(evicted)
    }

    /// H2 eviction tombstone: instead of a hard DELETE, leave the evicted row as a
    /// self-superseding tombstone (`supersedes_id = id`). Every read surface already
    /// excludes superseded rows via `NOT EXISTS(s.supersedes_id = bm.id)`, so the
    /// tombstone vanishes from render / reconcile / recall / dup-guard, while
    /// `matches_superseded_entry` still hits its content — doctor --fix therefore
    /// does NOT revive an evicted atom (closing the edited_at-permanent-immunity
    /// capacity race). The row keeps its content + id so the supersedes FK stays
    /// valid. Tombstones are purged at the start of a later eviction round (see
    /// `prepare_sync_atoms_to_md`) ONLY once their content has left the disk .md —
    /// if a prior flush failed the content is still on disk and the tombstone stays
    /// to block doctor revival (one round of grace is the minimum, not the maximum).
    /// Derived entities + the vector index are still removed.
    fn delete_evicted_atom(tx: &rusqlite::Transaction<'_>, id: i64) -> anyhow::Result<()> {
        tx.execute(
            "UPDATE bounded_memory SET supersedes_id = NULL WHERE supersedes_id = ?1",
            rusqlite::params![id],
        )?;
        tx.execute(
            "DELETE FROM entities WHERE memory_atom_id = ?1",
            rusqlite::params![id],
        )?;
        tx.execute(
            "UPDATE bounded_memory SET supersedes_id = ?1 WHERE id = ?1",
            rusqlite::params![id],
        )?;
        // 维度3: 与 remove/split 的硬失败上抛（`?`）不同，驱逐是 best-effort 容量
        // 清理——owner 行 + entities 已在本 tx 删除，vec 去索引失败只留一个孤儿
        // 向量（检索降级，非数据损坏），不应据此中止整轮驱逐、让容量永不收敛。
        // 故此处 warn-only；策略差异是有意的，不是遗漏。
        if let Err(e) = tx.execute(
            "DELETE FROM vec_bounded_memory WHERE id = ?1",
            rusqlite::params![id],
        ) {
            tracing::warn!(
                "evict: failed to de-index vec_bounded_memory id={}: {}",
                id,
                e
            );
        }
        Ok(())
    }

    fn record_eviction_audit(
        &self,
        evicted: usize,
        freed: usize,
        atom_budget: usize,
        capacity: usize,
    ) {
        tracing::info!(
            "Atom capacity eviction: removed {} atoms (freed {} chars, atom_budget {}, capacity {})",
            evicted,
            freed,
            atom_budget,
            capacity
        );
        if let Err(e) = crate::growth::audit::log_action(
            self.db,
            "evict",
            "memory",
            &format!(
                "evicted={} freed_chars={} atom_budget={} capacity={}",
                evicted, freed, atom_budget, capacity
            ),
            None,
        ) {
            tracing::warn!("evict: audit log failed: {}", e);
        }
    }

    /// Evict excess atoms in DB and render MEMORY.md content without flushing
    /// to disk yet, so callers holding a DB mutex can drop the lock before fsync.
    pub fn prepare_sync_atoms_to_md(&self) -> anyhow::Result<(usize, PendingMdWrite)> {
        // C8: 本调用与调用方的原子入库提交同处一个 DB 锁临界区，且随后必定渲染并
        // 交付快照——在此递增代际即覆盖「入库提交 + 下面的驱逐提交」；解锁间隙里
        // 其他写者持有的旧快照 flush 时会因代际落后而跳过。
        bump_md_generation(&self.target_file("memory")?);
        let atom_budget = (self.memory_limit as f64 * self.atom_capacity_ratio) as usize;
        let capacity = self.capacity("memory");

        // H2 tombstone cleanup（窄窗修复）：只清「被驱逐内容确已离开磁盘 .md」的墓碑。
        // 若上一轮 .md flush 失败，磁盘仍留着被驱逐内容——此时清墓碑会让 doctor --fix
        // 失去 matches_superseded_entry 命中而复活该内容（跨进程 doctor 亦然）。故事务外
        // 先读磁盘（plain read，无 flock，R1 安全；atomic_write temp+rename 保证读到完整
        // 旧/新文件，绝不撕裂），把内容仍出现在磁盘条目里的墓碑留到下一轮。读在事务外偏
        // 保守：磁盘只会失去被驱逐内容（flush 从 DB 重渲染，永不写回被驱逐内容），顶多
        // 多留一轮墓碑，不会误清。文件缺失 ⇒ 盘面无被驱逐内容 ⇒ 全清。
        let disk_entries: HashSet<String> = {
            let path = self.target_file("memory")?;
            match std::fs::read_to_string(&path) {
                Ok(c) => split_entries(&extract_body(&c)).into_iter().collect(),
                Err(_) => HashSet::new(),
            }
        };

        // W8: 把预算统计 SELECT、驱逐 DELETE 与 render_md_from_db 快照读放进
        // 同一个 IMMEDIATE 事务，防止外部并发写者插进读后写前窗口。
        let tx = self.db.immediate_tx()?;

        // 选择性清墓碑：内容仍在磁盘 → 保留（挡复活）；已离开磁盘 → 先 deref 指向它的
        // 活行（FK 安全）再删除。本轮稍后新建的墓碑不在候选集，自然留到下一轮（一轮
        // grace 是下限而非上限）。归一+trim 与 reconcile 的规范化条目比对同口径。
        let tombstones: Vec<(i64, String)> = {
            let mut stmt =
                tx.prepare("SELECT id, content FROM bounded_memory WHERE supersedes_id = id")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (id, content) in tombstones {
            if disk_entries.contains(normalize_crlf(&content).trim()) {
                continue;
            }
            tx.execute(
                "UPDATE bounded_memory SET supersedes_id = NULL
                 WHERE supersedes_id = ?1 AND id != ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "DELETE FROM bounded_memory WHERE id = ?1 AND supersedes_id = id",
                rusqlite::params![id],
            )?;
        }

        let mut manual_chars = 0usize;
        {
            // W2: 受保护预算 = manual 条目 + 用户手改过的活 atom（edited_at 非空）。
            // HISTORY.md 契约：自动重写机制必须跳过带 edited_at 的行。
            let mut stmt = tx.prepare(
                "SELECT content FROM bounded_memory bm
                 WHERE bm.target = 'memory'
                   AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
                   AND (COALESCE(bm.memory_type, 'manual') NOT IN ('atom', 'scenario')
                        OR bm.edited_at IS NOT NULL)",
            )?;
            let contents = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for content in contents {
                // 维度2: +3 分隔符 allowance，与 evict_atoms_tx 的 remaining_atom
                // 同口径（恒多计一个分隔符，偏保守安全）。
                manual_chars += content.chars().count() + 3;
            }
        }

        let atoms: Vec<(i64, String, bool)> = {
            let mut stmt = tx.prepare(
                "SELECT bm.id, bm.content,
                        EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id) AS is_superseded
                 FROM bounded_memory bm
                 WHERE bm.target = 'memory' AND COALESCE(bm.memory_type, 'manual') = 'atom'
                   AND (bm.supersedes_id IS NULL OR bm.supersedes_id != bm.id)
                   AND (bm.edited_at IS NULL
                        OR EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id))
                 ORDER BY is_superseded DESC, bm.created_at ASC",
            )?;
            // E0597 陷阱：此 let-rows 绑定为 rusqlite 借用检查所必需——query_map
            // 的结果借用 stmt，须先 collect 落地为 owned Vec 再离开 stmt 作用域；
            // 改成块尾直接返回 collect 表达式会编译失败（E0597）。Sonar S1488
            // （立即返回而非临时变量）在此为假阳性，勿"简化"掉该绑定。
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                    ))
                })?
                // F10: 行解码失败上抛（与 render_md_from_db 同姿势）——静默丢行会让
                // 该 atom 逃出淘汰核算。
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };

        let evicted = self.evict_atoms_tx(&tx, &atoms, manual_chars, atom_budget, capacity)?;
        if manual_chars > capacity {
            tracing::warn!(
                "bounded memory 'memory': 受保护(manual/手改)条目共 {} 字符已超出容量 {}，无法通过淘汰 atom 收敛",
                manual_chars,
                capacity
            );
        }

        let (_, pending) = self.render_md_from_db("memory")?;
        tx.commit()?;
        Ok((evicted, pending))
    }

    #[cfg(test)]
    pub fn sync_atoms_to_md(&self) -> anyhow::Result<usize> {
        let (evicted, mut pending) = self.prepare_sync_atoms_to_md()?;
        // The immediate render-write shell re-renders current DB state and owns
        // snapshot clearing; disarm this deferred snapshot's Drop mis-clear.
        pending.flushed = true;
        self.render_write_md_locked("memory")?;
        Ok(evicted)
    }
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod tests;
