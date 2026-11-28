use super::SessionStore;
use crate::fact::conversation::{self, SessionHeader, Turn};
use crate::util::time;
use std::path::{Path, PathBuf};

impl SessionStore<'_> {
    /// U5 升级缝隙（改名 × `gateway://` 伪 URI，见
    /// [`crate::fact::conversation::compute_legacy_session_path`] 文档）：Append 写盘
    /// 前对候选旧文件的处置。候选按 header session_id 校验——前缀碰撞的外来
    /// 文件与解析失败的文件一律不碰。
    ///
    /// - 目标文件不存在（升级后首写）：返回旧文件的 turns（跨候选按 seq 去重，
    ///   首见优先——后见者是被 DB 取代的旧版本，warn 后随文件一起丢弃），旧文件
    ///   登记为“写盘成功后删除”；第三个返回值为可补齐当前 header 缺失元数据的
    ///   旧 header（M7，首见优先）；
    /// - 目标文件已存在（上次写成功但删旧失败的崩溃残留）：仅当旧文件的 seq 集
    ///   ⊆ 目标文件（已被覆盖）才登记删除，否则保留并 warn（宁并存不误删）。
    pub(super) fn prior_jsonl_for_append(
        &self,
        header: &SessionHeader,
        target: &Path,
        candidates: &[PathBuf],
    ) -> (Vec<Turn>, Vec<PathBuf>, Option<SessionHeader>) {
        let present: Vec<PathBuf> = candidates
            .iter()
            .filter(|p| {
                // W10: 先 exists() 再规范对规范比较（避免对不存在候选
                // canonicalize）；same_file 防大小写别名自删/自迁移。
                p.exists()
                    && !same_file(p, target)
                    && crate::util::is_contained_in_dir(self.conversations_dir, p)
            })
            .cloned()
            .collect();
        if present.is_empty() {
            return (Vec::new(), Vec::new(), None);
        }

        // C3/C6: 本函数在写事务**外**预读（候选枚举 / legacy 读取都是 O(文件) IO，
        // 不得跨 BEGIN IMMEDIATE 写锁）。target.exists() 与上方候选 exists() 取的是
        // 事务前快照；多写进程并发首写时，对手可能在本扫描与 BEGIN IMMEDIATE 之间
        // 已迁移并提交——该缝隙由 append_jsonl_in_tx 的事务内 exists() 复检封死
        // （GAP-4：目标已在则丢弃 pre-tx 迁移 turns）。exists() 是 best-effort 哨兵
        // （目标已在 → 走覆盖清理而非迁移）。
        if !target.exists() {
            collect_migration_turns(header, present)
        } else {
            let stale = collect_covered_legacy_files(header, target, present);
            (Vec::new(), stale, None)
        }
    }
}

fn collect_migration_turns(
    header: &SessionHeader,
    present: Vec<PathBuf>,
) -> (Vec<Turn>, Vec<PathBuf>, Option<SessionHeader>) {
    let mut turns: Vec<Turn> = Vec::new();
    let mut stale: Vec<PathBuf> = Vec::new();
    let mut legacy_header: Option<SessionHeader> = None;
    let mut seen: std::collections::HashSet<u32> = std::collections::HashSet::new();
    for cand in present {
        merge_migration_candidate(
            header,
            cand,
            &mut seen,
            &mut turns,
            &mut stale,
            &mut legacy_header,
        );
    }
    (turns, stale, legacy_header)
}

fn merge_migration_candidate(
    header: &SessionHeader,
    cand: PathBuf,
    seen: &mut std::collections::HashSet<u32>,
    turns: &mut Vec<Turn>,
    stale: &mut Vec<PathBuf>,
    legacy_header: &mut Option<SessionHeader>,
) {
    let (h, cand_turns) = match conversation::read_session(&cand) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::fact::session_store",
                "升级迁移：旧文件解析失败，不迁移不删除 {}: {}",
                cand.display(),
                e
            );
            return;
        }
    };
    if h.session_id != header.session_id {
        tracing::debug!(
            target: "asuna_memory::fact::session_store",
            "升级清理：旧命名文件 session_id 不匹配（前缀碰撞），不触碰 {}",
            cand.display()
        );
        return;
    }
    // M7: 保留首个匹配候选的旧 header，供调用方补齐当前 header 缺失的元数据。
    if legacy_header.is_none() {
        *legacy_header = Some(h.clone());
    }
    stale.push(cand.clone());
    for t in cand_turns {
        if seen.insert(t.seq) {
            turns.push(t);
        } else {
            tracing::warn!(
                target: "asuna_memory::fact::session_store",
                "升级迁移：旧文件 {} 中 seq {} 与已迁移内容重复，按 DB 现状丢弃",
                cand.display(),
                t.seq
            );
        }
    }
}

fn collect_covered_legacy_files(
    header: &SessionHeader,
    target: &Path,
    present: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let target_seqs: std::collections::HashSet<u32> = match conversation::read_session(target) {
        Ok((_, ts)) => ts.iter().map(|t| t.seq).collect(),
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::fact::session_store",
                "升级清理：目标文件读取失败，本轮旧文件处理跳过 {}: {}",
                target.display(),
                e
            );
            return Vec::new();
        }
    };
    present
        .into_iter()
        .filter(|cand| is_legacy_file_covered(cand, &header.session_id, &target_seqs))
        .collect()
}

fn is_legacy_file_covered(
    cand: &Path,
    session_id: &str,
    target_seqs: &std::collections::HashSet<u32>,
) -> bool {
    match conversation::read_session(cand) {
        Ok((h, cand_turns)) if h.session_id == session_id => {
            if cand_turns.iter().all(|t| target_seqs.contains(&t.seq)) {
                return true;
            }
            tracing::warn!(
                target: "asuna_memory::fact::session_store",
                "升级清理：旧文件 {} 含目标文件未覆盖的 turns，保留（该会话并存两个 JSONL，需人工合并）",
                cand.display()
            );
            false
        }
        Ok(_) => false,
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::fact::session_store",
                "升级清理：旧文件解析失败，保留 {}: {}",
                cand.display(),
                e
            );
            false
        }
    }
}

/// U5 升级缝隙的候选旧文件路径集（见
/// [`crate::fact::conversation::compute_legacy_session_path`]）：
/// ① DB `file_path` 记录的旧路径（`gateway://` 伪 URI 磁盘上不存在，由调用方
///    的 exists() 自然过滤）；
/// ② 按改名前命名规则从 header.start_time 串复原——REST /capture 两版命名都
///    用 unix_ms_to_iso(DB start_ts)/first_ts 同源串，此复原对升级主场景精确；
/// ③ 同规则再从 DB start_ts 复原一次，覆盖客户端 header 串与当年命名字符串
///    表示不一致的偏移情形。
/// 保序去重。
pub(super) fn legacy_jsonl_candidates(
    conversations_dir: &Path,
    header: &SessionHeader,
    prior_db: Option<(&str, i64)>,
) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some((rel, db_start_ts)) = prior_db {
        let p = conversations_dir.join(rel);
        if !out.contains(&p) {
            out.push(p);
        }
        if let Ok(p) = conversation::compute_legacy_session_path(
            conversations_dir,
            &header.session_id,
            &time::unix_ms_to_iso(db_start_ts),
        ) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    if let Ok(p) = conversation::compute_legacy_session_path(
        conversations_dir,
        &header.session_id,
        &header.start_time,
    ) {
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// W10: 规范对规范比较两个路径是否指向同一文件。大小写不敏感 FS 上
/// legacy 名（sha256-hex8 小写）与新名（session_id 前 8 字符保留大小写）
/// 可能字面不同却指同一文件；Windows \\?\ verbatim 前缀也只能规范对规范
/// 比较，不可与原始路径混比。任一 canonicalize 失败（如候选不存在）时
/// 回退字面比较。
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Overwrite 侧的旧命名文件清理：必须位于 `conversations_dir` 内且 header session_id
/// 匹配才删；越界路径、前缀碰撞或解析失败的文件均不触碰。
pub(super) fn remove_verified_legacy_jsonl(
    conversations_dir: &Path,
    candidates: &[PathBuf],
    keep: &Path,
    session_id: &str,
) {
    for cand in candidates {
        // W10: 先 exists() 再规范对规范比较，防大小写别名候选自删刚写好的目标。
        if cand.exists() && !same_file(cand, keep) {
            try_remove_legacy_candidate(conversations_dir, cand, session_id);
        }
    }
}

fn try_remove_legacy_candidate(conversations_dir: &Path, cand: &Path, session_id: &str) {
    if !crate::util::is_contained_in_dir(conversations_dir, cand) {
        tracing::warn!(
            target: "asuna_memory::fact::session_store",
            "安全拦截：旧 JSONL 路径超出 conversations_dir 边界，拒绝删除 {}",
            cand.display()
        );
        return;
    }
    remove_if_matching_session(cand, session_id);
}

fn remove_if_matching_session(cand: &Path, session_id: &str) {
    let Ok((header, _)) = conversation::read_session(cand) else {
        tracing::warn!(
            target: "asuna_memory::fact::session_store",
            "升级清理：旧命名文件解析失败，不删除 {}",
            cand.display()
        );
        return;
    };
    if header.session_id != session_id {
        tracing::debug!(
            target: "asuna_memory::fact::session_store",
            "升级清理：旧命名文件 session_id 不匹配（前缀碰撞），不触碰 {}",
            cand.display()
        );
        return;
    }
    if let Err(e) = std::fs::remove_file(cand) {
        tracing::warn!(
            target: "asuna_memory::fact::session_store",
            "升级改名前旧 JSONL 清理失败 {}: {}",
            cand.display(),
            e
        );
    }
}
