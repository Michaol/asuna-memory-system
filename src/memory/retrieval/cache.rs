use super::layers::{recall_intents_from, recall_mental_models_from};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Preloaded L4/L5 consolidation docs (and the L3 file fallback) read outside
/// the DB mutex lock.
#[derive(Clone)]
pub struct PreloadedRecallDocs {
    pub(super) mental_models: Vec<serde_json::Value>,
    pub(super) intents: Vec<serde_json::Value>,
    /// W28c: L3 file fallback (`persona.md` → `USER.md`) read before the lock,
    /// so `/recall` never does a synchronous `fs::read` while holding the guard.
    pub(super) persona_fallback: Option<String>,
}

/// C13: one cached preload — the source files' `(mtime, len)` signature at
/// read time, the rendered docs, and the earliest L4/L5 freshness expiry.
pub(super) struct PreloadCacheEntry {
    signature: Vec<Option<(std::time::SystemTime, u64)>>,
    docs: PreloadedRecallDocs,
    /// 缓存不得活过新鲜度门： consolidation 停摆（文件不再改写）时文档仍会
    /// 到期，纯 mtime 缓存会把过期文档无限期当新鲜供应——恰是 freshness gate
    /// 设计要防的场景。到期即重建（重读 + 重 gate）。
    expires_at_ms: i64,
}

/// C13: process-wide preload cache keyed by `memory_dir`. Production serves one
/// memory dir per process; the cap below bounds test/temp-dir accumulation.
pub(super) static PRELOAD_CACHE: std::sync::OnceLock<
    std::sync::Mutex<HashMap<PathBuf, PreloadCacheEntry>>,
> = std::sync::OnceLock::new();

/// The files `preload_recall_docs` reads, in fixed order (signature alignment).
fn preload_source_paths(memory_dir: &Path) -> [PathBuf; 7] {
    [
        memory_dir
            .join("mental_models")
            .join("workflow-patterns.md"),
        memory_dir
            .join("mental_models")
            .join("decision-framework.md"),
        memory_dir
            .join("mental_models")
            .join("communication-style.md"),
        memory_dir.join("intent").join("likely-next-topics.md"),
        memory_dir.join("intent").join("anticipated-needs.md"),
        memory_dir.join("persona.md"),
        memory_dir.join("USER.md"),
    ]
}

fn preload_signatures(memory_dir: &Path) -> (Vec<Option<(std::time::SystemTime, u64)>>, bool) {
    let mut had_error = false;
    let sigs = preload_source_paths(memory_dir)
        .iter()
        .map(|p| match std::fs::metadata(p) {
            Ok(md) => match md.modified() {
                Ok(mtime) => Some((mtime, md.len())),
                Err(e) => {
                    had_error = true;
                    tracing::warn!(
                        target: "asuna_memory::memory::retrieval",
                        "recall preload mtime read error for {}: {}",
                        p.display(),
                        e
                    );
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                had_error = true;
                tracing::warn!(
                    target: "asuna_memory::memory::retrieval",
                    "recall preload stat error for {}: {}",
                    p.display(),
                    e
                );
                None
            }
        })
        .collect();
    (sigs, had_error)
}

/// Read L4/L5 consolidation `.md` documents (and the L3 persona file fallback)
/// before acquiring the DB lock.
///
/// C13 / W13: results are cached per `memory_dir` and reused while every source
/// file's `(mtime, len)` is unchanged and no cached document has passed its
/// freshness expiry. Mutex is dropped before disk reads and entries are cached
/// only when no non-`NotFound` I/O error occurred so partial reads never poison
/// the cache.
pub fn preload_recall_docs(memory_dir: &Path) -> PreloadedRecallDocs {
    let (signature, mut had_error) = preload_signatures(memory_dir);

    let cache = PRELOAD_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let now_ms = crate::util::time::now_unix_ms();
    {
        let map = crate::util::recover_poison(cache);
        if let Some(entry) = map.get(memory_dir) {
            if entry.signature == signature && now_ms <= entry.expires_at_ms {
                return entry.docs.clone();
            }
        }
    }

    let (mental_models, mm_expiry, mm_err) = recall_mental_models_from(memory_dir);
    let (intents, it_expiry, it_err) = recall_intents_from(memory_dir);
    let (persona_fallback, pf_err) =
        read_persona_fallback_checked(&memory_dir.join("persona.md"), &memory_dir.join("USER.md"));
    had_error |= mm_err || it_err || pf_err;

    let docs = PreloadedRecallDocs {
        mental_models,
        intents,
        persona_fallback,
    };
    if !had_error {
        cache_preload_if_current(memory_dir, signature, &docs, mm_expiry.min(it_expiry));
    }
    docs
}

/// W16: 签名快照在读盘**之前**采集——读盘窗口内并发写入会推进文件代际，
/// 这份（可能已陈旧的）结果不得覆盖更新的缓存条目。入缓存前在锁内复核
/// 签名仍与磁盘一致才插入；不一致则放弃缓存（本次返回值仍照常供应，
/// 下次调用按新签名重读）。封顶清空策略不变（生产单目录，命中局部性
/// 不值得更多机器）。
pub(super) fn cache_preload_if_current(
    memory_dir: &Path,
    signature: Vec<Option<(std::time::SystemTime, u64)>>,
    docs: &PreloadedRecallDocs,
    expires_at_ms: i64,
) {
    let cache = PRELOAD_CACHE.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut map = crate::util::recover_poison(cache);
    let (current, _) = preload_signatures(memory_dir);
    if current != signature {
        return;
    }
    if map.len() >= 32 {
        map.clear();
    }
    map.insert(
        memory_dir.to_path_buf(),
        PreloadCacheEntry {
            signature,
            docs: docs.clone(),
            expires_at_ms,
        },
    );
}

fn read_persona_fallback_checked(persona_md: &Path, user_md: &Path) -> (Option<String>, bool) {
    let mut had_error = false;
    for path in [persona_md, user_md] {
        let (text, err) = read_trimmed_nonempty_checked(path);
        had_error |= err;
        if let Some(content) = text {
            return (Some(content), had_error);
        }
    }
    (None, had_error)
}

/// First non-blank of the two L3 fallback files (same order/semantics as the
/// in-lock chain: trimmed content, `None` when missing/unreadable/blank).
pub(super) fn read_persona_fallback(persona_md: &Path, user_md: &Path) -> Option<String> {
    read_persona_fallback_checked(persona_md, user_md).0
}

/// Read a file as trimmed text; distinguishes `NotFound` from real I/O errors
/// (`(None, false)` vs warn + `(None, true)`).
fn read_trimmed_nonempty_checked(path: &Path) -> (Option<String>, bool) {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            ((!trimmed.is_empty()).then(|| trimmed.to_string()), false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (None, false),
        Err(e) => {
            tracing::warn!(
                target: "asuna_memory::memory::retrieval",
                "recall persona fallback read error for {}: {}",
                path.display(),
                e
            );
            (None, true)
        }
    }
}
