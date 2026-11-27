//! JSONL 会话文件的读写（SessionHeader / Turn 的序列化与目录枚举）。
//!
//! 归属理由（J37）：JSONL 会话文件是记忆系统的**存储格式**，属存储层
//! （`index` = 存储/索引层，SQLite 侧的同层兄弟是 `db`/`schema`），而不是
//! 事实层的业务逻辑；`rebuild.rs` 按文件内容重建 DB 索引是其主要读者。
//! 从 `fact/` 移到此处，消除了原先 `rebuild → 本模块` 造成的 fact↔index
//! 双向依赖。`fact/mod.rs` 保留了兼容再导出，crate 内
//! `fact::conversation::…` 路径继续可用。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// 会话头（JSONL 第一行）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionHeader {
    pub v: u32,
    #[serde(rename = "type")]
    pub header_type: String,
    pub session_id: String,
    pub start_time: String,
    pub profile_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// 对话轮次
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub ts: String,
    pub seq: u32,
    pub role: String,
    pub content: String,
    /// 任意扩展字段 (model, usage, tool_name, arguments 等)
    #[serde(flatten)]
    pub metadata: Option<serde_json::Value>,
}

/// 计算会话 JSONL 文件的目标路径（不创建目录、不写盘）
///
/// 命名契约（U5）：`<dir>/<YYYY>/<MM>/<DD>/<YYYYMMDD>T<HHMMSS>_<hash8>.jsonl`，
/// `<hash8>` = sha256(session_id) 的前 8 位 hex。**没有任何代码从文件名反解析
/// session_id 或时间戳**——rebuild 按文件内容第一行的 header 识别会话，
/// `cleanup_old_jsonl` 按 DB `sessions.file_path` 定位旧文件。因此文件名只要求
/// 唯一性：同一 session_id 同一秒 → 同一路径（重存覆盖，语义本就如此）；
/// 不同 session_id 即使共享前缀（如 "session-1"/"session-2"）也几乎必然不同名。
///
/// 跨版本升级缝隙（U5 改名 × J33b file_path 改写）：v2.6.2 及之前经 REST
/// /capture 写出的会话，磁盘上是**旧命名**文件而 DB `file_path` 是
/// `gateway://` 伪 URI——改名后任何写路径的清理都定位不到旧文件，同一
/// session_id 会在磁盘上永久并存两个文件（rebuild 会把两份 turns 混插，
/// 增量检测恒判不一致）。`SessionStore` 的 Append/Overwrite 在写盘后按
/// `compute_legacy_session_path`（crate 内部）定位并迁移/清理该旧文件，封死此缝隙。
pub fn compute_session_path(
    conversations_dir: &Path,
    header: &SessionHeader,
) -> anyhow::Result<PathBuf> {
    let (dir, compact_time) = session_path_parts(conversations_dir, &header.start_time)?;
    use sha2::{Digest, Sha256};
    // U5: was `session_id.chars().take(8)` — two ids sharing an 8-char prefix
    // written in the same second overwrote each other's JSONL.
    let mut hasher = Sha256::new();
    hasher.update(header.session_id.as_bytes());
    let digest = hasher.finalize();
    let short_id: String = digest
        .iter()
        .take(4)
        .map(|b| format!("{:02x}", b))
        .collect();
    let filename = format!("{}_{}.jsonl", compact_time, short_id);
    Ok(dir.join(&filename))
}

/// U5 改名**之前**的命名规则（文件名后缀 = `session_id` 前 8 字符）计算路径。
///
/// 仅供 `SessionStore` 升级清理使用（pub(crate)：模块 doc 已声明除此之外任何
/// 代码都不得依赖旧命名规则）：旧文件与新路径共享同一 start_time 串
/// （v2.6.2 与 HEAD 的 /capture 都以 `unix_ms_to_iso(DB start_ts)` 命名），
/// 故同一 header 串下只差后缀，可从 header 精确复原。
pub(crate) fn compute_legacy_session_path(
    conversations_dir: &Path,
    session_id: &str,
    start_time: &str,
) -> anyhow::Result<PathBuf> {
    crate::util::validate_session_id(session_id).map_err(|e| anyhow::anyhow!(e))?;
    let (dir, compact_time) = session_path_parts(conversations_dir, start_time)?;
    let short_id: String = session_id.chars().take(8).collect();
    Ok(dir.join(format!("{}_{}.jsonl", compact_time, short_id)))
}

/// 两个命名规则共用的部分：解析 start_time → (日期目录, 紧凑时间串)。
fn session_path_parts(
    conversations_dir: &Path,
    start_time: &str,
) -> anyhow::Result<(PathBuf, String)> {
    let start_dt = chrono::DateTime::parse_from_rfc3339(start_time)
        .or_else(|_| {
            let naive = chrono::NaiveDateTime::parse_from_str(start_time, "%Y-%m-%dT%H:%M:%S%.f")?;
            Ok::<_, anyhow::Error>(naive.and_utc().fixed_offset())
        })
        // D2-5: 与 util::time::ts_to_unix_ms 对称——也接受紧凑格式，否则紧凑
        // start_time 的会话写路径/旧文件清理定位失败（时间能解析、路径算不出）。
        .or_else(|_| {
            let naive = chrono::NaiveDateTime::parse_from_str(start_time, "%Y%m%dT%H%M%S")?;
            Ok::<_, anyhow::Error>(naive.and_utc().fixed_offset())
        })?;

    let dir = conversations_dir
        .join(start_dt.format("%Y").to_string())
        .join(start_dt.format("%m").to_string())
        .join(start_dt.format("%d").to_string());
    let compact_time = start_dt.format("%Y%m%dT%H%M%S").to_string();
    Ok((dir, compact_time))
}

/// 汇总会话所有 turn 的 token 数（usage.input_tokens + output_tokens）。
///
/// M14 共享语义（save_overwrite 与 rebuild 共用，显式选定 rebuild 侧口径）：
/// 逐字段容错——input/output 任一缺失按 0 计；单 turn 和非正时按 0；
/// 跨 turn 用 saturating 折叠防溢出。save_append 按契约不维护 total_tokens，
/// 不调用本函数。
pub fn session_total_tokens(turns: &[Turn]) -> i64 {
    turns
        .iter()
        .map(|t| turn_tokens(t.metadata.as_ref()))
        .fold(0i64, i64::saturating_add)
}

/// 计算会话结束时间戳（M6 共享语义：`save_overwrite` 与 `rebuild` 共用）：
/// 取全部 turn 时间戳（不可解析时按 turn 入库口径回退 `start_ts`）的最大值；
/// 空 turns 返回 `None`。
pub fn session_end_ts(turns: &[Turn], start_ts: i64) -> Option<i64> {
    turns
        .iter()
        .map(|t| crate::util::time::ts_to_unix_ms(&t.ts).unwrap_or(start_ts))
        .max()
}

/// 单条 turn 的 token 数：逐字段容错（缺失字段按 0），和非正时返回 0。
fn turn_tokens(metadata: Option<&serde_json::Value>) -> i64 {
    let Some(u) = metadata.and_then(|m| m.get("usage")) else {
        return 0;
    };
    let inp = u
        .get("input_tokens")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let out = u
        .get("output_tokens")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let sum = inp.saturating_add(out);
    sum.max(0)
}

/// 把 (header, turns) 序列化为 JSONL 原子写到指定路径（创建父目录）
pub fn write_session_at(
    file_path: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<()> {
    let mut content = serde_json::to_string(header)?;
    content.push('\n');
    for turn in turns {
        content.push_str(&serde_json::to_string(turn)?);
        content.push('\n');
    }
    crate::util::atomic_write(file_path, content)?;
    Ok(())
}

/// 写入一个会话为 JSONL 文件，返回文件路径（仅测试使用）
#[cfg(test)]
pub fn write_session(
    conversations_dir: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<PathBuf> {
    let file_path = compute_session_path(conversations_dir, header)?;
    write_session_at(&file_path, header, turns)?;
    Ok(file_path)
}

/// 读取 JSONL 文件，返回 (SessionHeader, Vec<Turn>)
pub fn read_session(path: &Path) -> anyhow::Result<(SessionHeader, Vec<Turn>)> {
    let content = std::fs::read_to_string(path)?;
    let mut lines = content.lines();

    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("JSONL 文件为空"))?;
    let header: SessionHeader = serde_json::from_str(header_line)?;

    let mut turns = Vec::new();
    let mut lines = lines.peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Turn>(line) {
            Ok(turn) => turns.push(turn),
            // 仅**末行**不可解析时按"被中断的追加留下的残缺尾行"处理（warn + 丢弃），
            // 而不是让整文件 Err。否则一个 SIGKILL 残行会让 rebuild 的 C1/W1 守卫
            // 永久拒绝重建，阻塞**所有**会话的索引恢复（残行会话若不再被 Append
            // 就永远不会自愈）。中间行损坏仍整文件 Err——无法判定是残缺还是真损坏。
            Err(e) => {
                if lines.clone().all(|l| l.trim().is_empty()) {
                    tracing::warn!(
                        "JSONL {} 末行不可解析，按被中断的追加残行丢弃: {}",
                        path.display(),
                        e
                    );
                } else {
                    return Err(e.into());
                }
            }
        }
    }

    Ok((header, turns))
}

/// W5.3: 轻量会话摘要（首行 header + 去重后有效 turn 计数），供增量检测/doctor 使用——
/// 不反序列化完整 Turn/metadata（每次非增量 rebuild 检测+重建曾 2×O(语料) 全量解析）。
///
/// 口径：逐行解析并通过 `SeqOnly` + `HashSet<u32>` 按 `seq` 去重（与 rebuild /
/// `insert_session_turns` 保持同口径）；W14a: 与 `read_session` 对齐——仅**末行**
/// 不可解析（剩余行全为空）按被中断的追加残行 warn+丢弃、不计数，中间坏行仍计入
/// `turn_count`，确保增量检测触发完整重建由 `read_session` 暴露错误；
/// header 失败/文件不可读仍上抛。
pub fn read_session_summary(path: &Path) -> anyhow::Result<(SessionHeader, usize)> {
    use std::collections::HashSet;

    #[derive(Deserialize)]
    struct SeqOnly {
        seq: u32,
    }

    // W14a: 与 read_session 同构（全文 + peekable）。不用 BufRead::lines：
    // io::Lines 的 clone 会共享底层 File 偏移，clone 窥探尾部可能推进共享偏移
    // 导致原迭代器丢行；str::Lines 的 clone 是纯内存快照，安全。
    let content = std::fs::read_to_string(path)?;
    let mut lines = content.lines();
    let header_line = lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("JSONL 文件为空"))?;
    let header: SessionHeader = serde_json::from_str(header_line)?;
    // W6: 与 rebuild/insert_session_turns 保持同口径（按 seq 去重，保留先见者）。
    let mut seen_seqs = HashSet::new();
    let mut turn_count = 0usize;
    let mut lines = lines.peekable();
    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<SeqOnly>(line) {
            Ok(s) => {
                if seen_seqs.insert(s.seq) {
                    turn_count += 1;
                }
            }
            // W14a: 残缺末行（剩余行全空）按 read_session 口径 warn+丢弃、不计数——
            // 否则摘要恒比 read_session/DB 多 1，增量检测恒判不一致，启动陷入
            // 无限完整重建/重嵌入循环。中间坏行仍计数以触发完整重建。
            Err(e) => {
                if lines.clone().all(|l| l.trim().is_empty()) {
                    tracing::warn!(
                        "JSONL {} 末行不可解析，按被中断的追加残行丢弃: {}",
                        path.display(),
                        e
                    );
                } else {
                    turn_count += 1;
                }
            }
        }
    }
    Ok((header, turn_count))
}

/// C5: JSONL 真相源中的最大 `seq`（容忍坏行；文件缺失/不可读返回 0）。
///
/// Append 分配新 seq 时必须同时尊重 DB 与 JSONL：`delete-turn` 有意只删 DB 行而
/// 保留 JSONL 行，此后仅按 `DB MAX(seq)` 分配会与文件里已存在的 seq 撞号，而
/// rebuild 的同文件去重"保留先见者"会把新捕获的 turn 永久丢弃。
/// 与 `read_session_summary` 不同，本函数不因坏行失败——它只取能解析出的最大 seq。
pub fn max_session_seq(path: &Path) -> i64 {
    use std::io::BufRead;

    #[derive(Deserialize)]
    struct SeqOnly {
        seq: u32,
    }

    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    let mut max_seq = 0i64;
    for line in std::io::BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if let Ok(s) = serde_json::from_str::<SeqOnly>(&line) {
            max_seq = max_seq.max(i64::from(s.seq));
        }
    }
    max_seq
}

/// 只读 JSONL 首行取 session_id（中间行损坏时整文件 `read_session` 会失败，但
/// header 仍可解析）。rebuild 用它识别"文件损坏但会话身份已知"的情形，从而把该
/// 会话的 DB 行**原样保留**而不是连带清空——无法识别时才拒绝重建。
pub fn read_session_id_only(path: &Path) -> Option<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    let first = std::io::BufReader::new(file).lines().next()?.ok()?;
    let header: SessionHeader = serde_json::from_str(&first).ok()?;
    Some(header.session_id)
}

/// 整批重放检测（JSONL 写成功但 DB COMMIT 失败后的重试幂等）。
///
/// 目标文件**尾部 N 行**是否与入参整批**按序逐条全等**（role/content/ts，忽略 seq）？
/// 命中即判定为同一批次的重放，返回这些行的**起始 seq**，供调用方复用同一 seq 区间
/// 并跳过追加——否则重试会以更高的 seq 再写一份相同内容，而 rebuild 只按 seq 去重，
/// 两份都会被索引（真相源静默重复）。
///
/// 判据刻意收窄到"整批按序全等"而非通用内容去重：用户合法发送的两条相同 turn
/// 只要不是整批重放就不会被吞。**不依赖 DB 失败后状态**——SQLite 官方明确
/// COMMIT 类错误后事务"might"被自动回滚（不保证），只比对文件字节才稳健。
pub fn trailing_batch_replay(path: &Path, batch: &[Turn]) -> Option<u32> {
    if batch.is_empty() {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let mut existing: Vec<Turn> = Vec::new();
    for line in content.lines().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(t) = serde_json::from_str::<Turn>(line) {
            existing.push(t);
        }
    }
    if existing.len() < batch.len() {
        return None;
    }
    let tail = &existing[existing.len() - batch.len()..];
    replay_first_if_contiguous(tail, batch)
}

/// C5: 整批重放的共享判据——尾部 N 行与入参按序逐条全等（role/content/ts，忽略
/// seq）**且** seq 连续（`seq[k] == first + k`）。非连续尾批（delete-turn / 部分
/// 缺口留下的空洞）不得认定为重放，否则复用 `first..first+N` 会与文件里已有的
/// 非连续 seq 撞号或复活已删 turn。返回尾批起始 seq。
fn replay_first_if_contiguous(tail: &[Turn], batch: &[Turn]) -> Option<u32> {
    if tail.is_empty() || tail.len() != batch.len() {
        return None;
    }
    let same = tail
        .iter()
        .zip(batch)
        .all(|(a, b)| a.role == b.role && a.content == b.content && a.ts == b.ts);
    if !same {
        return None;
    }
    let first = tail[0].seq;
    let contiguous = tail
        .iter()
        .enumerate()
        .all(|(k, t)| u64::from(t.seq) == u64::from(first) + k as u64);
    contiguous.then_some(first)
}

/// W27a: Append 读阶段的单遍扫描结果——`(最大 seq, 整批重放起始 seq)`。
pub struct AppendScan {
    /// JSONL 中能解析出的最大 `seq`（容忍坏行；文件缺失/不可读为 0）。
    pub max_seq: i64,
    /// 尾部整批重放命中时的起始 seq（判据同 [`trailing_batch_replay`]，含连续性）。
    pub replay_first_seq: Option<u32>,
}

/// W27a: 单遍读取目标 JSONL，同时算出 `max_seq` 与整批重放候选，替代
/// `max_session_seq` + `trailing_batch_replay` 各自一次的全量读（O(N²) 次文件读）。
///
/// 逐行只解析轻量的 `SeqOnly` 取最大 seq；用一个容量为 `batch.len()` 的环形缓冲
/// 保留最后 N 个非空行，扫描结束后才把这 N 行解析为完整 `Turn` 做重放比对——
/// 全程只读一遍文件。坏行容忍口径与 `max_session_seq` 一致（跳过）；尾批含不可
/// 解析行时保守判为非重放（宁可正常追加新 seq，也不复用可能撞号的区间）。
pub fn scan_jsonl_for_append(path: &Path, batch: &[Turn]) -> AppendScan {
    use std::collections::VecDeque;
    use std::io::BufRead;

    #[derive(Deserialize)]
    struct SeqOnly {
        seq: u32,
    }

    let mut scan = AppendScan {
        max_seq: 0,
        replay_first_seq: None,
    };
    let Ok(file) = std::fs::File::open(path) else {
        return scan;
    };
    let n = batch.len();
    let mut tail: VecDeque<String> = VecDeque::with_capacity(n);
    for (idx, line) in std::io::BufReader::new(file).lines().enumerate() {
        let Ok(line) = line else { break };
        if idx == 0 || line.trim().is_empty() {
            continue; // 首行是 header，无 seq
        }
        if let Ok(s) = serde_json::from_str::<SeqOnly>(&line) {
            scan.max_seq = scan.max_seq.max(i64::from(s.seq));
        }
        if n > 0 {
            if tail.len() == n {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    }
    if n > 0 && tail.len() == n {
        let parsed: Option<Vec<Turn>> = tail
            .iter()
            .map(|l| serde_json::from_str::<Turn>(l).ok())
            .collect();
        if let Some(turns) = parsed {
            scan.replay_first_seq = replay_first_if_contiguous(&turns, batch);
        }
    }
    scan
}

/// 列出所有 JSONL 会话文件，出错时返回错误（不跟随符号链接）。
/// 若 `conversations_dir` 尚不存在，返回空 Vec。
pub fn try_list_sessions(conversations_dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !conversations_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    collect_jsonl_files(conversations_dir, &mut files)?;
    files.sort();
    Ok(files)
}

/// 列出所有 JSONL 会话文件（测试便捷包装，生产路径请用 [`try_list_sessions`]）
#[cfg(test)]
pub fn list_sessions(conversations_dir: &Path) -> Vec<PathBuf> {
    match try_list_sessions(conversations_dir) {
        Ok(files) => files,
        Err(e) => {
            tracing::warn!("读取会话目录 {} 失败: {}", conversations_dir.display(), e);
            Vec::new()
        }
    }
}

/// D7-5: 目录递归深度上限——命名契约是 YYYY/MM/DD（3 层），16 已极宽裕；
/// 极深树（如 bind-mount 回环，symlink 虽已排除）不再栈溢出，超限 warn+跳过子树。
const MAX_JSONL_DIR_DEPTH: usize = 16;

fn collect_jsonl_files(dir: &Path, out: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    collect_jsonl_files_at(dir, out, 0)
}

fn collect_jsonl_files_at(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> anyhow::Result<()> {
    if depth > MAX_JSONL_DIR_DEPTH {
        tracing::warn!(
            "会话目录递归超过深度上限 {}，跳过子树: {}",
            MAX_JSONL_DIR_DEPTH,
            dir.display()
        );
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            collect_jsonl_files_at(&path, out, depth + 1)?;
        } else if ft.is_file()
            // D7-5: 扩展名大小写不敏感——Windows 文件系统忽略大小写，.JSONL 同样是会话文件。
            && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl"))
        {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "conversation_tests.rs"]
mod tests;
