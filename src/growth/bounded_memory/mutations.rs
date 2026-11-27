//! BoundedMemory 写面（write/update/remove）与级联删除助手。
//! M14 自 mod.rs 拆出；读面/构造/校验仍在 mod.rs，写状态机在 write_state.rs。

use super::format::{
    extract_body, has_entry_separator, normalize_crlf, reject_entry_separator, truncate_chars,
    ENTRY_SEPARATOR,
};
use super::reconcile::matching_row_ids;
use super::{bump_md_generation_with_snapshot, write_md_serialized, BoundedMemory};
use crate::util::time;

/// 维度7: 审计 detail 预览长度提常量（原 100/50 魔法数散落 write/update/remove）。
/// write 记整条内容预览；update/remove 记 old/new 差异预览（双侧各此长度）。
const AUDIT_WRITE_PREVIEW_CHARS: usize = 100;
const AUDIT_DIFF_PREVIEW_CHARS: usize = 50;

/// 维度3: commit 后 .md 刷新失败的统一指引——DB 已提交、文件落后，重试 write
/// 会撞 dup-check，须用 doctor --fix 从 DB 重建 .md 收敛。
const MD_FLUSH_FAILED_HINT: &str =
    "DB 已提交但 .md 刷新失败（DB 领先于文件，重试 write 会撞 dup-check）；请运行 doctor --fix 从 DB 重建 .md";

/// F8: 审计 detail 先脱敏再截断——凭据原文不得落进 audit_log
/// （与 growth::audit::record_unsafe_turn 同姿势）。
fn redacted_preview(text: &str, limit: usize) -> String {
    truncate_chars(&crate::growth::security::redact_credentials(text), limit)
}

impl<'a> BoundedMemory<'a> {
    /// 写入新条目（追加）
    pub fn write(
        &self,
        target: &str,
        content: &str,
        confidence: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // W2.3: 入口 CRLF→LF 归一后再校验/存库/渲染，DB 行与 .md 永远同形。
        let normalized = normalize_crlf(content);
        let content = normalized.as_str();
        if content.trim().is_empty() {
            anyhow::bail!("content 不能为空");
        }
        if !matches!(confidence, "high" | "medium" | "low") {
            anyhow::bail!(
                "非法 confidence: '{}' (仅支持 'high' / 'medium' / 'low')",
                confidence
            );
        }
        self.run_scan(content)?;

        reject_entry_separator(
            content,
            "content",
            "'\\n§' 结尾 / '§\\n' 开头会被当作分隔符；一次只能写一个条目，多条请用多次 write",
        )?;

        let path = self.target_file(target)?;
        let capacity = self.capacity(target);
        let current = self.read(target)?;
        let body = extract_body(&current);

        let trimmed = content.trim();
        let md_dup = body.split(ENTRY_SEPARATOR).any(|e| e.trim() == trimmed);
        if md_dup {
            anyhow::bail!("条目已存在，拒绝重复写入");
        }

        let new_body = if body.is_empty() {
            content.to_string()
        } else {
            format!("{}{}{}", body, ENTRY_SEPARATOR, content)
        };

        if new_body.chars().count() > capacity {
            anyhow::bail!(
                "超出容量上限: {}/{} 字符。请先整合或删除旧条目",
                new_body.chars().count(),
                capacity
            );
        }

        let now = time::now_unix_ms();
        let tx = self.db.immediate_tx()?;
        // NEW-B-growth: db_dup 复查搬入 IMMEDIATE 事务，消除跨进程 dup-check→INSERT
        // 窗口。进程内 DB 本就串行（MCP 单线程 Rc<Db>；HTTP 外部 Arc<Mutex<Db>>），
        // 故事务内复查主要防外部 CLI/sqlite 直写并发——只有事务内读才与 INSERT 一致。
        let db_dup: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM bounded_memory bm
                WHERE bm.target = ?1 AND TRIM(REPLACE(bm.content, char(13)||char(10), char(10)), char(9)||char(10)||char(13)||' ') = ?2
                  AND COALESCE(bm.memory_type, 'manual') != 'scenario'
                  AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
             )",
            rusqlite::params![target, trimmed],
            |r| r.get(0),
        )?;
        if db_dup {
            anyhow::bail!("条目已存在，拒绝重复写入");
        }
        tx.execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, source_session, confidence)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![target, content, now, now, session_id, confidence],
        )?;
        crate::growth::audit::log_action(
            self.db,
            "write",
            target,
            &serde_json::json!({"content_preview": redacted_preview(content, AUDIT_WRITE_PREVIEW_CHARS)}).to_string(),
            session_id,
        )?;
        tx.commit()?;
        let header = self.metadata_header(target, capacity);
        let full = format!("{}\n\n{}", header, new_body);
        // C7/C8: 提交即递增代际并记录未刷盘快照——解锁间隙里 flush 的旧快照据此
        // 自行跳过；写盘失败时 read() 仍读到刚提交的内容，不回退陈旧盘面。
        bump_md_generation_with_snapshot(&path, &full);
        write_md_serialized(&path, full).map_err(|e| e.context(MD_FLUSH_FAILED_HINT))?;
        Ok(())
    }

    /// 按条目（§ 分隔）匹配并整体替换。
    pub fn update(
        &self,
        target: &str,
        old_text: &str,
        new_text: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // W2.3: 入口 CRLF→LF 归一——old_text 须与 extract_body 渲染的 LF body 同形
        // 才能命中，new_text 归一后存库才不与 .md 发散。
        let old_norm = normalize_crlf(old_text);
        let old_text = old_norm.as_str();
        let new_norm = normalize_crlf(new_text);
        let new_text = new_norm.as_str();
        if old_text.is_empty() {
            anyhow::bail!("old_text 不能为空");
        }
        reject_entry_separator(old_text, "old_text", "update 仅能替换单个条目内的文本")?;
        self.run_scan(new_text)?;

        reject_entry_separator(new_text, "new_text", "update 仅能修改单个条目内容")?;

        let capacity = self.capacity(target);
        let current = self.read(target)?;
        let body = extract_body(&current);
        if !body.contains(old_text) {
            anyhow::bail!("未找到要替换的文本");
        }

        let entries: Vec<&str> = body.split(ENTRY_SEPARATOR).collect();
        let mut updated_entries: Vec<String> = Vec::with_capacity(entries.len());
        let mut replaced_hits: Vec<String> = Vec::new();
        let mut hits = 0usize;
        for (idx, entry) in entries.iter().enumerate() {
            if entry.contains(old_text) {
                let replaced = entry.replace(old_text, new_text);
                if replaced.trim().is_empty() {
                    anyhow::bail!("替换后的条目不能为空（如需删除请使用 remove）");
                }
                // C2: 拼接结果可能命中注入扫描，即使 old_text/new_text 各自 benign
                // （"忽略X" + X→"之前的指令" = "忽略之前的指令"）——必须复扫组装后的条目。
                self.run_scan(&replaced)?;
                if has_entry_separator(&replaced) {
                    anyhow::bail!("替换后的条目不能包含条目分隔符 '\\n§\\n' 或其截断变体");
                }
                // W18a: 替换结果与 .md 中另一条目全等 → 拒绝，防止 update 制造重复条目。
                let trimmed_replaced = replaced.trim();
                if entries
                    .iter()
                    .enumerate()
                    .any(|(j, other)| j != idx && other.trim() == trimmed_replaced)
                {
                    anyhow::bail!("替换后的条目与其他条目重复，拒绝更新");
                }
                replaced_hits.push(replaced.clone());
                updated_entries.push(replaced);
                hits += 1;
            } else {
                updated_entries.push(entry.to_string());
            }
        }
        if hits == 0 {
            anyhow::bail!("未找到要替换的文本");
        }
        let updated_body = updated_entries.join(ENTRY_SEPARATOR);

        let updated_char_count = updated_body.chars().count();
        if updated_char_count > capacity {
            anyhow::bail!(
                "替换后超出容量上限: {}/{} 字符",
                updated_char_count,
                capacity
            );
        }

        let header = self.metadata_header(target, capacity);
        let full = format!("{}\n\n{}", header, updated_body);
        let path = self.target_file(target)?;

        let now = time::now_unix_ms();
        let tx = self.db.immediate_tx()?;
        let affected_ids = matching_row_ids(&tx, target, old_text, false)?;
        // W18a: 事务内复查 DB 活行重复——替换结果若等于另一条**不含 old_text**
        // （即非本次被更新行）的活跃非取代行，拒绝，防止 .md 落后/缺失时制造重复。
        for replaced in &replaced_hits {
            let dup: bool = tx.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM bounded_memory bm
                    WHERE bm.target = ?1
                      AND TRIM(REPLACE(bm.content, char(13)||char(10), char(10)), char(9)||char(10)||char(13)||' ') = ?2
                      AND COALESCE(bm.memory_type, 'manual') != 'scenario'
                      AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
                      AND INSTR(REPLACE(bm.content, char(13)||char(10), char(10)), ?3) = 0
                 )",
                rusqlite::params![target, replaced.trim(), old_text],
                |r| r.get(0),
            )?;
            if dup {
                anyhow::bail!("替换后的条目与现有条目重复，拒绝更新");
            }
        }
        for id in affected_ids {
            tx.execute(
                "DELETE FROM vec_bounded_memory WHERE id = ?1",
                rusqlite::params![id],
            )?;
            tx.execute(
                "UPDATE bounded_memory
                 SET content = REPLACE(REPLACE(content, char(13)||char(10), char(10)), ?1, ?2),
                     updated_at = ?3,
                     edited_at = ?3
                 WHERE id = ?4",
                rusqlite::params![old_text, new_text, now, id],
            )?;
            // W15: 派生实体镜像必须随内容刷新——它是 `INSERT OR IGNORE … SELECT
            // content` 写入的，永不更新；否则 /graph/path 与 /graph/neighbors 继续
            // 返回旧文本。**不删 mentions 边**：派生只在原子入库时发生
            // （integrate_single_atom），update 不会再集成，删掉即永久销毁；且
            // SQL INSTR/LOWER 只折叠 ASCII、比对的是原始 name，与写侧
            // canonicalize（Unicode  aware）口径不一致，会误删无关的人名边。
            tx.execute(
                "UPDATE entities
                 SET name = (SELECT content FROM bounded_memory WHERE id = ?1)
                 WHERE memory_atom_id = ?1 AND entity_type = 'memory_atom'",
                rusqlite::params![id],
            )?;
        }
        crate::growth::audit::log_action(
            self.db,
            "update",
            target,
            &serde_json::json!({
                "old": redacted_preview(old_text, AUDIT_DIFF_PREVIEW_CHARS),
                "new": redacted_preview(new_text, AUDIT_DIFF_PREVIEW_CHARS),
                "entries_affected": hits
            })
            .to_string(),
            session_id,
        )?;
        tx.commit()?;
        // C7/C8: 提交即递增代际并记录未刷盘快照（写盘失败时 read() 不回退陈旧盘面）。
        bump_md_generation_with_snapshot(&path, &full);

        write_md_serialized(&path, full).map_err(|e| e.context(MD_FLUSH_FAILED_HINT))?;
        Ok(())
    }

    /// 按条目级匹配删除：包含 old_text 的整条条目被移除。
    pub fn remove(
        &self,
        target: &str,
        old_text: &str,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // W2.3: 入口 CRLF→LF 归一，old_text 才能命中 extract_body 渲染的 LF body。
        let old_norm = normalize_crlf(old_text);
        let old_text = old_norm.as_str();
        if old_text.is_empty() {
            anyhow::bail!("old_text 不能为空");
        }
        reject_entry_separator(old_text, "old_text", "remove 仅能按单个条目匹配删除")?;
        let path = self.target_file(target)?;
        let current = self.read(target)?;
        let body = extract_body(&current);
        if !body.contains(old_text) {
            anyhow::bail!("未找到要删除的文本");
        }

        let entries: Vec<&str> = body.split(ENTRY_SEPARATOR).collect();
        let kept: Vec<&str> = entries
            .iter()
            .copied()
            .filter(|entry| !entry.contains(old_text))
            .collect();
        if kept.len() == entries.len() {
            anyhow::bail!("未找到要删除的文本");
        }
        let new_body = kept.join(ENTRY_SEPARATOR);

        let capacity = self.capacity(target);
        let header = self.metadata_header(target, capacity);
        let full = if new_body.trim().is_empty() {
            format!("{}\n\n", header)
        } else {
            format!("{}\n\n{}", header, new_body)
        };

        let tx = self.db.immediate_tx()?;
        let affected_ids = matching_row_ids(&tx, target, old_text, true)?;
        let matched_rows = affected_ids.len();
        let mut cascaded: Vec<i64> = Vec::new();
        for id in affected_ids {
            // C4: 先收集本行取代的前驱链。删掉链头后这些前驱不再被任何行取代，
            // 会通过读取面的 NOT EXISTS 判定**复活**——用户的显式删除被静默撤销，
            // 且下一次 rebuild_md_from_db / doctor --fix 会把被否定的旧事实写回文件。
            let chain = superseded_chain(&tx, id)?;
            delete_atom_row(&tx, id)?;
            for pred in chain {
                // 仍被其他存活行取代的前驱保留（由那条链自己处置）
                let still_referenced: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM bounded_memory WHERE supersedes_id = ?1)",
                    rusqlite::params![pred],
                    |r| r.get(0),
                )?;
                if !still_referenced {
                    delete_atom_row(&tx, pred)?;
                    cascaded.push(pred);
                }
            }
        }
        crate::growth::audit::log_action(
            self.db,
            "remove",
            target,
            // F4: 级联删除的是用户可见的历史行，必须留痕（计数 + id），否则
            // 运维排查"某条旧事实去哪了"时无任何线索。
            &serde_json::json!({
                "removed": redacted_preview(old_text, AUDIT_DIFF_PREVIEW_CHARS),
                "matched_rows": matched_rows,
                "cascaded_superseded": cascaded.len(),
                "cascaded_ids": cascaded,
            })
            .to_string(),
            session_id,
        )?;
        tx.commit()?;
        // C7/C8: 提交即递增代际并记录未刷盘快照（写盘失败时 read() 不回退陈旧盘面）。
        bump_md_generation_with_snapshot(&path, &full);

        write_md_serialized(&path, full).map_err(|e| e.context(MD_FLUSH_FAILED_HINT))?;
        Ok(())
    }
}

/// C4: 删除一个原子行及其向量/派生实体，并解除指向它的 supersedes 引用。
fn delete_atom_row(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE bounded_memory SET supersedes_id = NULL WHERE supersedes_id = ?1",
        rusqlite::params![id],
    )?;
    conn.execute(
        "DELETE FROM vec_bounded_memory WHERE id = ?1",
        rusqlite::params![id],
    )?;
    conn.execute(
        "DELETE FROM entities WHERE memory_atom_id = ?1",
        rusqlite::params![id],
    )?;
    conn.execute(
        "DELETE FROM bounded_memory WHERE id = ?1",
        rusqlite::params![id],
    )?;
    Ok(())
}

/// C4: 沿 `supersedes_id` 收集 `id` 所取代的前驱链（不含自身）。
/// `seen` 防自引用/环状数据导致死循环。
pub(super) fn superseded_chain(conn: &rusqlite::Connection, id: i64) -> anyhow::Result<Vec<i64>> {
    use rusqlite::OptionalExtension;
    let pred_of = |row_id: i64| -> anyhow::Result<Option<i64>> {
        // supersedes_id 可为 NULL（绝大多数行），必须按 Option<i64> 取列，
        // 否则 rusqlite 报 Invalid column type Null。
        let v: Option<Option<i64>> = conn
            .query_row(
                "SELECT supersedes_id FROM bounded_memory WHERE id = ?1",
                rusqlite::params![row_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(v.flatten())
    };
    let mut chain = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // W18a: 起始 id 先入 seen——自引用环或绕回 id 的环都不会把 id 自身收进链
    // （与文档“不含自身”一致），否则 remove 会级联删除正在删除的行本身。
    seen.insert(id);
    let mut cur = pred_of(id)?;
    while let Some(p) = cur {
        if !seen.insert(p) {
            break;
        }
        chain.push(p);
        cur = pred_of(p)?;
    }
    Ok(chain)
}
