use crate::fact::conversation::{SessionHeader, Turn};
use std::path::{Path, PathBuf};

/// 追加阶段的文件计划：读阶段（候选枚举 / legacy 读取 / seq 扫描 / 重放检测）都在
/// 写事务**外**算好，这里只把结果传进事务内的写阶段。收成一个结构也避免
/// `append_jsonl_and_cleanup` 超过 7 参数上限。
pub(super) struct AppendFilePlan {
    pub(super) prior_turns: Vec<Turn>,
    pub(super) stale_files: Vec<PathBuf>,
    /// 整批重放：文件里已有逐条全等的这一批，复用其 seq 区间并跳过追加。
    pub(super) replay: bool,
}

/// Append turn lines to a JSONL file. Creates the file with header if it doesn't exist,
/// otherwise appends lines only. Ensures parent directory exists.
///
/// J33 收敛时自 transport/http.rs（旧 /capture 的归档路径）逐位迁入，
/// 仅供 [`super::SaveMode::Append`] 使用；Overwrite 模式走
/// `conversation::write_session_at` 全量重写。
pub(super) fn append_jsonl_turns(
    jsonl_path: &Path,
    header: &SessionHeader,
    turns: &[Turn],
) -> anyhow::Result<()> {
    // Ensure parent directory exists (W2 fix)
    if let Some(parent) = jsonl_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(jsonl_path)?;

    // W2.7: 记录追加前的文件长度（复用下方 metadata 读取，避免二次 syscall）。
    // write_all/sync_all 中途失败（ENOSPC/EIO）会留下半行 JSON：下次 Append 因
    // len>0 不写 header，新行拼接残行 → 整会话文件永久损坏（read_session 任一
    // 坏行即整文件 Err，会话在 rebuild/迁移/清理永久缺席且不自愈）。失败时
    // best-effort 截回 pre_len 回滚部分追加，保持 W3“失败批次不留痕”契约。
    let pre_len = file.metadata()?.len();
    // C7: 上次追加若被 SIGKILL/掉电打断，文件可能以半行 JSON 结尾。直接追加会把
    // 新行拼到残行上，而 read_session 任一坏行即整文件 Err → 整会话永久损坏且
    // 不自愈（每次追加加深损坏）。追加前截回最后一个完整行。
    let (pre_len, needs_leading_newline) = repair_torn_jsonl_tail(jsonl_path, pre_len);
    let mut buf = String::new();
    // Check file size AFTER opening (avoids TOCTOU race between exists() and open());
    // propagate metadata() failure rather than silently appending a duplicate header.
    if pre_len == 0 {
        buf.push_str(&serde_json::to_string(header)?);
        buf.push('\n');
    } else if needs_leading_newline {
        // W6: 末行是完整 Turn 但缺结尾换行——补一个换行，不截断内容。
        buf.push('\n');
    }
    for turn in turns {
        buf.push_str(&serde_json::to_string(turn)?);
        buf.push('\n');
    }
    write_all_or_rollback(&mut file, pre_len, buf.as_bytes(), jsonl_path)?;

    Ok(())
}

/// C7: 追加前修复被中断的追加留下的残缺尾行。返回 `(修复后长度, 是否需要先补一个
/// 换行)`，长度作为 `write_all_or_rollback` 的回滚基准。
///
/// 从尾部按块回扫最后一个 `'\n'`；整个文件都没有换行（连 header 都残缺）则截到 0，
/// 让调用方重写 header。**关键判别（W6）**：末段若能解析为完整 `Turn`，说明它只是
/// 缺少结尾换行（例如 write_all 落完整对象但没落最后的 '\n'，或外部/历史文件），
/// 此时**不得截断**——改为让调用方先补一个 '\n'，否则一条完整 turn 被销毁。
/// W14b: ① 回扫上限 16 MiB——超限仍未找到 '\n' 时 warn 并原样返回 `(len, false)`，
/// 不截断、不把整段尾巴塞进内存（防 OOM）；② `cut == 0`（全文无换行）时除 `Turn`
/// 外还尝试解析 `SessionHeader`——完整的 header 只是缺结尾换行，必须保留并补 '\n'，
/// 不得截到 0 销毁 header。
/// Windows 陷阱同 `rollback_partial_append`：append 句柄只有 FILE_APPEND_DATA，
/// `set_len` 需 GENERIC_WRITE，故另开 write 句柄截断。任一 IO 失败都退回原长度
/// （best-effort，不阻断本次保存）。
pub(super) fn repair_torn_jsonl_tail(jsonl_path: &Path, len: u64) -> (u64, bool) {
    use std::io::{Read, Seek, SeekFrom};
    if len == 0 {
        return (0, false);
    }
    let mut read = match std::fs::File::open(jsonl_path) {
        Ok(f) => f,
        Err(_) => return (len, false),
    };
    // W14b: 回扫窗口上限 16 MiB。floor 之下的字节不再扫描。
    const MAX_TAIL_SCAN: u64 = 16 * 1024 * 1024;
    let floor = len.saturating_sub(MAX_TAIL_SCAN);
    let mut buf = [0u8; 8192];
    let mut pos = len;
    let mut cut = 0u64;
    while pos > floor {
        let n = buf.len().min((pos - floor) as usize);
        pos -= n as u64;
        if read.seek(SeekFrom::Start(pos)).is_err() || read.read_exact(&mut buf[..n]).is_err() {
            return (len, false);
        }
        if let Some(i) = buf[..n].iter().rposition(|&b| b == b'\n') {
            cut = pos + i as u64 + 1;
            break;
        }
    }
    if cut == len {
        return (len, false);
    }
    // W14b: 超过回扫上限仍未找到换行（cut==0 且 len>上限）——不截断、不读整段
    // 尾部进内存，原样返回让调用方按原长度追加（best-effort，不阻断本次保存）。
    if cut == 0 && len > MAX_TAIL_SCAN {
        tracing::warn!(
            target: "asuna_memory::fact::session_store",
            "JSONL {} 尾部 {} 字节内无换行（超过 {} 字节回扫上限），跳过残缺尾行修复",
            jsonl_path.display(),
            len,
            MAX_TAIL_SCAN
        );
        return (len, false);
    }
    // W6: 末段是完整 Turn 吗？是则只缺换行，保留内容并补 '\n'。
    let tail_len = (len - cut) as usize;
    let mut tail = vec![0u8; tail_len];
    if read.seek(SeekFrom::Start(cut)).is_ok() && read.read_exact(&mut tail).is_ok() {
        if serde_json::from_slice::<Turn>(&tail).is_ok() {
            return (len, true);
        }
        // W14b: cut==0（全文无换行）时末段也可能是完整 header——保留并补 '\n'，
        // 否则截到 0 会销毁 header，下次追加重写 header 丢失原会话身份/元数据。
        if cut == 0 && serde_json::from_slice::<SessionHeader>(&tail).is_ok() {
            return (len, true);
        }
    }
    tracing::warn!(
        target: "asuna_memory::fact::session_store",
        "JSONL {} 尾部有 {} 字节残缺行（上次追加被中断），追加前截回最后一个完整行",
        jsonl_path.display(),
        len - cut
    );
    match std::fs::OpenOptions::new()
        .write(true)
        .open(jsonl_path)
        .and_then(|f| f.set_len(cut))
    {
        Ok(()) => (cut, false),
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::fact::session_store",
                "残缺尾行截断失败 {}（本次追加可能拼接残行）: {}",
                jsonl_path.display(),
                e
            );
            (len, false)
        }
    }
}

/// W2.7: write_all + sync_all，任一失败即 best-effort 截回 pre_len 回滚部分追加，
/// 再返回原始写错误（调用方 save_append 按 W3 记 warn，不阻断保存）。
pub(super) fn write_all_or_rollback(
    file: &mut std::fs::File,
    pre_len: u64,
    bytes: &[u8],
    jsonl_path: &Path,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Err(e) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        rollback_partial_append(file, pre_len, jsonl_path);
        return Err(e);
    }
    Ok(())
}

/// W2.7: 追加写失败后 best-effort 截回写前长度。
/// Windows 陷阱：append-only 句柄仅 FILE_APPEND_DATA，set_len(SetEndOfFile) 需
/// GENERIC_WRITE——同句柄 set_len 在 Windows 失败（实测 rolled_len 不变）。故重新
/// 以 write 打开截断（单进程红线 best-effort；原 append 句柄仍开但不再写，默认
/// share mode 允许 reopen）。Unix append 隐含 write，reopen 同样可行。set_len 自身
/// 失败（磁盘完全不可写等）只能 warn——此时可能残留半行，原始写错误照常上抛。
pub(super) fn rollback_partial_append(
    _append_handle: &std::fs::File,
    pre_len: u64,
    jsonl_path: &Path,
) {
    let truncated = std::fs::OpenOptions::new()
        .write(true)
        .open(jsonl_path)
        .and_then(|f| f.set_len(pre_len));
    if let Err(e) = truncated {
        tracing::warn!(
            target: "asuna_memory::fact::session_store",
            "JSONL 追加失败后回滚截断也失败 {}（可能残留半行）: {}",
            jsonl_path.display(),
            e
        );
    }
}
