pub mod frontmatter;
pub mod http_client;
pub mod rrf;
pub mod text;
pub mod time;

/// Valid conversation turn roles shared across HTTP `/capture` and MCP `save_session`.
pub const VALID_ROLES: &[&str] = &["user", "assistant", "tool_call", "system"];

/// Lock a `std::sync::Mutex` recovering from poisoning.
///
/// A panic while the guard is held poisons the mutex; without recovery every
/// later acquire fails forever. Callers protecting a DB connection should pair
/// this with `rollback_if_in_tx()` before issuing new SQL statements.
pub fn recover_poison<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| {
        tracing::error!(
            "Mutex poisoned (a task panicked while holding it); recovering guard: {}",
            e
        );
        let guard = e.into_inner();
        // NEW-8: 清除 poison 标志——否则一次 panic 后进程余生每次加锁都重发 error
        // （DB 热路径每请求一行，淹掉原始 panic 并涨盘）。恢复后后续 lock 不再 poisoned。
        m.clear_poison();
        guard
    })
}

fn env_var_trimmed(name: &str) -> Option<std::path::PathBuf> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
}

/// Resolve the user's home directory (`USERPROFILE` then `HOME` on Windows,
/// `HOME` then `USERPROFILE` on Unix/other, falling back to `"."`).
pub fn dirs_home() -> std::path::PathBuf {
    // NEW-5: set-but-empty/纯空白 env 视同未设置（与 config "纯空白=未设置" 尺标一致），
    // 避免数据目录静默漂移到进程 cwd。
    #[cfg(windows)]
    {
        env_var_trimmed("USERPROFILE")
            .or_else(|| env_var_trimmed("HOME"))
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }
    #[cfg(not(windows))]
    {
        env_var_trimmed("HOME")
            .or_else(|| env_var_trimmed("USERPROFILE"))
            .unwrap_or_else(|| std::path::PathBuf::from("."))
    }
}

/// Canonical containment guard: verifies `candidate` resolves strictly inside
/// `base_dir` (blocking `..` segments, external paths, equal directory, and symlink escapes).
pub fn is_contained_in_dir(base_dir: &std::path::Path, candidate: &std::path::Path) -> bool {
    let Ok(base) = std::fs::canonicalize(base_dir) else {
        return false;
    };
    let Ok(cand) = std::fs::canonicalize(candidate) else {
        return false;
    };
    cand != base && cand.starts_with(&base)
}

/// Return `true` if `name`'s stem (before the first `.`) is a reserved Windows
/// DOS device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1..COM9`, `LPT1..LPT9`),
/// case-insensitively.
pub fn is_windows_reserved_device_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    if stem.len() < 3 || stem.len() > 4 {
        return false;
    }
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Validate a profile identifier to prevent path traversal and reserved device
/// filenames across OSes.
pub fn validate_profile_id(profile_id: &str) -> Result<(), &'static str> {
    if profile_id.is_empty() {
        return Err("profile_id cannot be empty");
    }
    if profile_id.len() > 128 {
        return Err("profile_id too long (max 128 characters)");
    }
    if !profile_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("profile_id must contain only ASCII letters, digits, '_' or '-'");
    }
    if is_windows_reserved_device_name(profile_id) {
        return Err("profile_id cannot be a reserved device name");
    }
    Ok(())
}

/// Shared `session_id` gate across HTTP `/capture`, `/session/end`, MCP
/// `save_session`, and legacy path computation.
pub fn validate_session_id(session_id: &str) -> Result<(), &'static str> {
    if session_id.trim().is_empty() {
        return Err("session_id is required");
    }
    if session_id.chars().count() > 255 {
        return Err("session_id too long (max 255 characters)");
    }
    if session_id.chars().any(char::is_control) {
        return Err("session_id must not contain control characters");
    }
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return Err("session_id must not contain path separators or '..'");
    }
    // v4-M: ':' 与 profile/task_id 尺标对齐（Windows 文件名/ADS 非法字符）。
    // 新 JSONL 文件名走 sha256，不受影响；legacy 含 ':' 的会话只影响旧路径
    // 复原候选（compute_legacy_session_path 的调用方对 Err 静默跳过，DB
    // file_path 候选仍在），写入口一律拒绝。
    if session_id.contains(':') {
        return Err("session_id must not contain ':'");
    }
    if session_id == "." {
        return Err("session_id cannot be '.'");
    }
    Ok(())
}

/// Write `data` atomically to `path` by creating parent directories, writing to
/// a unique sibling temporary file with `sync_all`, and renaming into place.
pub fn atomic_write(path: &std::path::Path, data: impl AsRef<[u8]>) -> anyhow::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);

    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => std::path::Path::new("."),
    };
    std::fs::create_dir_all(parent)?;

    const MAX_ATTEMPTS: usize = 16;
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or("tmp");
    let pid = std::process::id();

    let mut tmp_path = None;
    for attempt in 0..MAX_ATTEMPTS {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{file_name}.{pid}.{seq}.tmp"));
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&candidate) {
            Ok(mut f) => {
                if let Err(e) = f.write_all(data.as_ref()).and_then(|_| f.sync_all()) {
                    drop(f);
                    let _ = std::fs::remove_file(&candidate);
                    return Err(e.into());
                }
                tmp_path = Some(candidate);
                break;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::AlreadyExists && attempt + 1 < MAX_ATTEMPTS =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let tmp_path = tmp_path.expect("loop returns on error or sets tmp_path");

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    // NEW-7: POSIX 掉电持久性需 fsync 父目录（让 rename 的目录条目落盘）；
    // 仅 sync 文件不覆盖崩溃持久性。Windows 无法 fsync 目录，跳过（语义不同）。
    #[cfg(unix)]
    {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    Ok(())
}

/// `atomic_write` 的临时文件名形状：`.{file_name}.{pid}.{seq}.tmp`，
/// 末两段必须是纯数字（offload 的 `.offload_{pid}_{nanos}_{seq}.tmp` 全用
/// 下划线分隔，不会命中）。
fn is_atomic_write_tmp(name: &str) -> bool {
    let Some(stem) = name.strip_prefix('.').and_then(|s| s.strip_suffix(".tmp")) else {
        return false;
    };
    let Some((rest, seq)) = stem.rsplit_once('.') else {
        return false;
    };
    let Some((file_name, pid)) = rest.rsplit_once('.') else {
        return false;
    };
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    !file_name.is_empty() && numeric(pid) && numeric(seq)
}

/// NEW-7 残留：进程在 create 与 rename 之间崩溃会留下孤儿 tmp 文件。
fn try_sweep_stale_tmp_file(entry: &std::fs::DirEntry, min_age: std::time::Duration) -> bool {
    let Some(name) = entry.file_name().to_str().map(str::to_string) else {
        return false;
    };
    if !is_atomic_write_tmp(&name) {
        return false;
    }
    let stale = entry
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= min_age);
    stale && std::fs::remove_file(entry.path()).is_ok()
}

/// 启动时（`Config::ensure_dirs`）递归清扫 `root` 下命中 `atomic_write`
/// 命名形状且 mtime 超过 `min_age` 的文件——宽限期保护并发写者的在途
/// 临时文件；不跟随/不删除符号链接；IO 错误一律跳过（best-effort）。
/// 返回删除的文件数。
pub fn sweep_orphan_tmp_files(root: &std::path::Path, min_age: std::time::Duration) -> usize {
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if file_type.is_file() && try_sweep_stale_tmp_file(&entry, min_age) {
                removed += 1;
            }
        }
    }
    removed
}

fn is_valid_url_scheme(s: &str) -> bool {
    let mut bytes = s.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn looks_like_host_with_port(authority: &str) -> bool {
    let Some((host, port_str)) = authority.split_once(':') else {
        return false;
    };
    let Ok(port) = port_str.parse::<u16>() else {
        return false;
    };
    port > 0 && (host.contains('.') || host.eq_ignore_ascii_case("localhost"))
}

fn strip_userinfo_segment(rest: &str) -> Option<String> {
    let split_idx = rest.find(['/', '?', '#']);
    let (authority, tail) = match split_idx {
        Some(idx) => (&rest[..idx], Some(&rest[idx..])),
        None => (rest, None),
    };
    if let Some((_, host_port)) = authority.rsplit_once('@') {
        let joined = match tail {
            Some(t) => format!("{host_port}{t}"),
            None => host_port.to_string(),
        };
        return Some(
            joined
                .split(['?', '#'])
                .next()
                .unwrap_or(&joined)
                .to_string(),
        );
    }
    if authority.starts_with('[') {
        return None;
    }
    let before_qf = rest.split(['?', '#']).next().unwrap_or(rest);
    if authority.contains(':') && !looks_like_host_with_port(authority) {
        if let Some((_, host_and_path)) = rest.rsplit_once('@') {
            return Some(
                host_and_path
                    .split(['?', '#'])
                    .next()
                    .unwrap_or(host_and_path)
                    .to_string(),
            );
        }
    }
    if !authority.contains(':')
        && !authority.contains('.')
        && !authority.eq_ignore_ascii_case("localhost")
    {
        if let Some((_, after_at)) = before_qf.rsplit_once('@') {
            if after_at.contains(['.', ':', '/']) {
                return Some(after_at.to_string());
            }
        }
    }
    None
}

/// Strip `userinfo@`, `?query`, and `#fragment` from a URL before logging so
/// embedded credentials or query tokens cannot leak into logs.
pub fn sanitize_url_for_log(url: &str) -> String {
    let trimmed = url.trim();
    if let Some((scheme, after_colon)) = trimmed.split_once(':') {
        if is_valid_url_scheme(scheme) && after_colon.starts_with('/') {
            let slash_count = after_colon.bytes().take_while(|&b| b == b'/').count();
            let (slashes, after_slashes) = after_colon.split_at(slash_count);
            if slash_count >= 2
                || matches!(
                    scheme.to_ascii_lowercase().as_str(),
                    "http"
                        | "https"
                        | "ws"
                        | "wss"
                        | "ftp"
                        | "socks4"
                        | "socks4a"
                        | "socks5"
                        | "socks5h"
                )
            {
                if let Some(stripped) = strip_userinfo_segment(after_slashes) {
                    return format!("{scheme}:{slashes}{stripped}");
                }
                return trimmed
                    .split(['?', '#'])
                    .next()
                    .unwrap_or(trimmed)
                    .to_string();
            }
        }
    }
    // F3 + W16: scheme-less / protocol-relative ("//", "///", "user:pass@host/path")
    let schemeless = trimmed.trim_start_matches('/');
    if let Some(stripped) = strip_userinfo_segment(schemeless) {
        return stripped;
    }
    trimmed
        .split(['?', '#'])
        .next()
        .unwrap_or(trimmed)
        .to_string()
}

/// Truncate a possibly-large payload (e.g. a raw LLM response) for log and
/// error surfaces (J28: `chat_json` parse failures used to embed the whole
/// response — potentially the entire conversation — into every log line).
///
/// Character-based, never splits a UTF-8 multi-byte sequence.
pub fn truncate_for_log(s: &str) -> String {
    const MAX_CHARS: usize = 200;
    if s.len() <= MAX_CHARS {
        return s.to_string();
    }
    let total = s.chars().count();
    if total <= MAX_CHARS {
        return s.to_string();
    }
    let cut: String = s.chars().take(MAX_CHARS).collect();
    format!("{} (truncated, {} chars total)", cut, total)
}

/// Classify a configured API base URL's scheme (J28). Returns a human-readable
/// problem description when the URL is not https, `None` when it is.
///
/// Deliberately NOT a rejection: plain `http://` to a local dev endpoint
/// (Ollama, vLLM on localhost) is legitimate, and a non-http(s) scheme is
/// mostly a typo whose request will fail anyway — a startup warning is the
/// proportionate response for both. Callers pass non-empty URLs only.
pub fn url_scheme_issue(url: &str) -> Option<&'static str> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") {
        None
    } else if lower.starts_with("http://") {
        Some("plain http: the API key and request payloads travel unencrypted; prefer https unless this is a trusted local endpoint")
    } else {
        Some("scheme is neither http nor https; requests will likely fail")
    }
}

#[cfg(test)]
mod tests;
