use super::{lock_db, AtomicFlagGuard};
use crate::config::Config;
use crate::index::db::Db;
use crate::memory::l1::TurnContent;
use crate::memory::llm::LlmClient;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const PERSONA_INPUT_SCENARIOS: i64 = 20;
const CONSOLIDATION_INPUT_ATOMS: i64 = 30;

/// digest 只取最近 8 轮：L4/L5 刷新需要"当前会话主题"信号，过长历史
/// 稀释主题并撑大 prompt（启发式预算，与 DIGEST_SUMMARY_CHARS 配套）。
const DIGEST_RECENT_TURNS: usize = 8;
/// digest summary 字符帽：digest 仅是 consolidation 输入之一，限长防止
/// 挤占 atom 输入的 prompt 预算（启发式预算）。
const DIGEST_SUMMARY_CHARS: usize = 400;

/// L3/L4/L5 共用的刷新 due 判定（旧名 persona_due 误导：并非仅服务
/// L3 persona，L4/L5 的 due 也走同一阈值）。
pub(super) fn layer_due(sessions_since: i64, trigger_every_n: i64) -> bool {
    trigger_every_n > 0 && sessions_since >= trigger_every_n
}

/// 行解码失败不再静默丢弃（filter_map(Result::ok)）——计数 + warn 首个
/// 错误，保留可解码行（persona/consolidation 输入是 best-effort LLM 上下文，
/// 缺行仅缩小输入；与 W26 整批放弃口径的差异在此是有意的）。
fn collect_rows_warn<T>(
    rows: impl Iterator<Item = rusqlite::Result<T>>,
    what: &str,
    session_id: &str,
) -> Vec<T> {
    let mut kept = Vec::new();
    let mut dropped = 0usize;
    let mut first_err: Option<String> = None;
    for row in rows {
        match row {
            Ok(v) => kept.push(v),
            Err(e) => {
                dropped += 1;
                if first_err.is_none() {
                    first_err = Some(e.to_string());
                }
            }
        }
    }
    if dropped > 0 {
        tracing::warn!(
            "{}: {} input rows dropped on decode failure for {} (first: {})",
            what,
            dropped,
            session_id,
            first_err.as_deref().unwrap_or("unknown")
        );
    }
    kept
}

pub(super) fn last_persona_ts(persona_path: &std::path::Path) -> i64 {
    if let Ok(content) = std::fs::read_to_string(persona_path) {
        if let Some(fm) = crate::util::frontmatter::extract_block(&content) {
            if let Some(updated_at) = crate::util::frontmatter::parse_i64(fm, "updated_at") {
                return updated_at;
            }
        }
    }
    persona_path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(super) fn mirror_scenario_title(
    scenarios_dir: &std::path::Path,
    created_at: i64,
    db_id: i64,
) -> Option<String> {
    let content =
        std::fs::read_to_string(scenarios_dir.join(format!("{}_{}.md", created_at, db_id))).ok()?;
    let fm = crate::util::frontmatter::extract_block(&content)?;
    crate::util::frontmatter::parse_string(fm, "title")
        .map(|t| t.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(super) fn persona_inputs(
    db: &Arc<Mutex<Db>>,
    scenarios_dir: &std::path::Path,
    session_id: &str,
) -> Vec<crate::memory::scenario::Scenario> {
    let rows: Vec<(i64, String, i64, i64)> = {
        let db_guard = lock_db(db);
        let out = db_guard
            .conn()
            .prepare(
                "SELECT id, content, created_at, updated_at FROM bounded_memory \
                 WHERE memory_type = 'scenario' ORDER BY updated_at DESC LIMIT ?1",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![PERSONA_INPUT_SCENARIOS], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                })
                .map(|rows| collect_rows_warn(rows, "L3 persona input", session_id))
            });
        match out {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "L3: persona input scenario query failed for {}: {}",
                    session_id,
                    e
                );
                return Vec::new();
            }
        }
    };
    // M10d: 目录清单只读一次，仅对真实存在的镜像发 read_to_string——旧实现
    // 逐行 N+1 读盘，镜像缺失（被驱逐/从未写）时是 N 次注定失败的 IO。
    let mirror_files: std::collections::HashSet<String> = std::fs::read_dir(scenarios_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    rows.into_iter()
        .map(|(id, content, created_at, updated_at)| {
            let title = mirror_files
                .contains(&format!("{}_{}.md", created_at, id))
                .then(|| mirror_scenario_title(scenarios_dir, created_at, id))
                .flatten()
                .unwrap_or_else(|| content.chars().take(30).collect());
            crate::memory::scenario::Scenario {
                title,
                atom_ids: Vec::new(),
                summary: content,
                created_at,
                updated_at,
            }
        })
        .collect()
}

pub(super) fn consolidation_inputs(
    db: &Arc<Mutex<Db>>,
    session_id: &str,
) -> Vec<crate::memory::scenario::Scenario> {
    let contents: Vec<String> = {
        let db_guard = lock_db(db);
        let out = db_guard
            .conn()
            .prepare(
                "SELECT bm.content FROM bounded_memory bm
                 WHERE bm.memory_type = 'atom'
                   AND NOT EXISTS (SELECT 1 FROM bounded_memory s WHERE s.supersedes_id = bm.id)
                 ORDER BY bm.updated_at DESC LIMIT ?1",
            )
            .and_then(|mut stmt| {
                stmt.query_map(rusqlite::params![CONSOLIDATION_INPUT_ATOMS], |r| {
                    r.get::<_, String>(0)
                })
                .map(|rows| collect_rows_warn(rows, "L4/L5 atom input", session_id))
            });
        match out {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("L4/L5: atom input query failed for {}: {}", session_id, e);
                return Vec::new();
            }
        }
    };
    contents
        .into_iter()
        .map(|content| crate::memory::scenario::Scenario {
            title: content.chars().take(30).collect(),
            atom_ids: Vec::new(),
            summary: content,
            created_at: 0,
            updated_at: 0,
        })
        .collect()
}

pub(super) fn session_digest(
    session_id: &str,
    turns: &[TurnContent],
) -> Option<crate::memory::scenario::Scenario> {
    let safe_turns: Vec<&TurnContent> = turns
        .iter()
        .filter(|t| crate::growth::security::scan_content(&t.content).is_safe())
        .collect();
    if safe_turns.is_empty() {
        return None;
    }
    let recent = &safe_turns[safe_turns.len().saturating_sub(DIGEST_RECENT_TURNS)..];
    let joined = recent
        .iter()
        .map(|t| format!("{}: {}", t.role, t.content))
        .collect::<Vec<_>>()
        .join(" | ");
    Some(crate::memory::scenario::Scenario {
        title: format!("current session {}", session_id),
        atom_ids: Vec::new(),
        summary: joined.chars().take(DIGEST_SUMMARY_CHARS).collect(),
        created_at: 0,
        updated_at: 0,
    })
}

fn persona_passes_security_scan(
    persona: &crate::memory::persona::Persona,
    session_id: &str,
) -> bool {
    if let Err(reason) = crate::growth::security::scan_fields(&[
        ("preferences", persona.preferences.as_str()),
        ("identity", persona.identity.as_str()),
        ("workflow", persona.workflow.as_str()),
        ("tech_stack", persona.tech_stack.as_str()),
        ("communication_style", persona.communication_style.as_str()),
    ]) {
        tracing::warn!(
            "L3: generated persona rejected by security scan for {}: {}",
            session_id,
            crate::growth::security::redact_credentials(&reason)
        );
        return false;
    }
    true
}

fn persist_l3_persona(
    generator: &crate::memory::persona::PersonaGenerator,
    persona: &crate::memory::persona::Persona,
    scenario_count: usize,
    session_id: &str,
) {
    match generator.save_persona(persona) {
        Ok(path) => tracing::info!(
            "Pipeline L3: persona regenerated from {} scenarios -> {} (session {})",
            scenario_count,
            path.display(),
            session_id
        ),
        Err(e) => tracing::warn!("L3: persona.md save failed for {}: {}", session_id, e),
    }
}

pub(super) fn refresh_l3_persona(
    db: &Arc<Mutex<Db>>,
    llm: &Arc<LlmClient>,
    memory_dir: &std::path::Path,
    session_id: &str,
) -> Option<crate::memory::persona::Persona> {
    let scenarios = persona_inputs(db, &memory_dir.join("scenarios"), session_id);
    if scenarios.is_empty() {
        tracing::debug!(
            "L3: no scenario rows for persona generation, skipping ({})",
            session_id
        );
        return None;
    }
    let generator = crate::memory::persona::PersonaGenerator::new(llm, memory_dir);
    let persona = match generator.generate(&scenarios) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("L3: persona generation failed for {}: {}", session_id, e);
            return None;
        }
    };
    if !persona_passes_security_scan(&persona, session_id) {
        return None;
    }
    persist_l3_persona(&generator, &persona, scenarios.len(), session_id);
    Some(persona)
}

fn retain_safe_list_items(layer: &str, items: &mut Vec<String>) {
    items.retain(|item| {
        let scan = crate::growth::security::scan_content(item);
        if scan.is_safe() {
            true
        } else {
            tracing::warn!(
                "{}: dropping generated item flagged by security scan ({})",
                layer,
                scan.reason()
            );
            false
        }
    });
}

struct ListDocMeta {
    layer: &'static str,
    scan_label: &'static str,
    noun: &'static str,
    gen_label: &'static str,
    gen_verb: &'static str,
    doc_file: &'static str,
}

fn persist_filtered_doc<T, S>(meta: &ListDocMeta, session_id: &str, doc: &T, save: S)
where
    S: FnOnce(&T) -> anyhow::Result<std::path::PathBuf>,
{
    match save(doc) {
        Ok(_) => tracing::info!(
            "Pipeline {}: {} refreshed ({})",
            meta.layer,
            meta.gen_label,
            session_id
        ),
        Err(e) => tracing::warn!("{}: {} save failed: {}", meta.layer, meta.doc_file, e),
    }
}

fn save_filtered_list<T, F, S>(
    meta: ListDocMeta,
    session_id: &str,
    result: anyhow::Result<T>,
    items_of: F,
    save: S,
) where
    F: FnOnce(&mut T) -> &mut Vec<String>,
    S: FnOnce(&T) -> anyhow::Result<std::path::PathBuf>,
{
    let mut doc = match result {
        Ok(doc) => doc,
        Err(e) => {
            tracing::warn!(
                "{}: {} {} failed: {}",
                meta.layer,
                meta.gen_label,
                meta.gen_verb,
                e
            );
            return;
        }
    };
    let items = items_of(&mut doc);
    // W21: 全空白 bullets 与空列表同效——否则 atomic_write 会用空文档覆盖旧文档。
    let was_empty = items.iter().all(|item| item.trim().is_empty());
    retain_safe_list_items(meta.scan_label, items);
    if items.iter().all(|item| item.trim().is_empty()) {
        if was_empty {
            tracing::debug!("{}: no {} returned, skipping save", meta.layer, meta.noun);
        } else {
            tracing::warn!(
                "{}: all {} dropped by security scan, skipping save",
                meta.layer,
                meta.noun
            );
        }
        return;
    }
    persist_filtered_doc(&meta, session_id, &doc, save);
}

pub(super) fn refresh_l4_mental_models(
    generator: &crate::memory::mental_model::MentalModelGenerator,
    inputs: &[crate::memory::scenario::Scenario],
    persona: Option<&crate::memory::persona::Persona>,
    session_id: &str,
) {
    let all = match generator.generate_all(inputs, persona) {
        Ok(all) => all,
        Err(e) => {
            tracing::warn!("L4: batched generation failed ({}): {}", session_id, e);
            return;
        }
    };
    save_filtered_list(
        ListDocMeta {
            layer: "L4",
            scan_label: "L4 workflow_patterns",
            noun: "workflow patterns",
            gen_label: "workflow patterns",
            gen_verb: "generation",
            doc_file: "workflow-patterns.md",
        },
        session_id,
        all.workflow,
        |w| &mut w.patterns,
        |w| generator.save_workflow_patterns(w),
    );
    save_filtered_list(
        ListDocMeta {
            layer: "L4",
            scan_label: "L4 decision_framework",
            noun: "decision criteria",
            gen_label: "decision framework",
            gen_verb: "generation",
            doc_file: "decision-framework.md",
        },
        session_id,
        all.decision,
        |d| &mut d.criteria,
        |d| generator.save_decision_framework(d),
    );
    save_filtered_list(
        ListDocMeta {
            layer: "L4",
            scan_label: "L4 communication_style",
            noun: "communication preferences",
            gen_label: "communication style",
            gen_verb: "generation",
            doc_file: "communication-style.md",
        },
        session_id,
        all.communication,
        |c| &mut c.preferences,
        |c| generator.save_communication_style(c),
    );
}

pub(super) fn refresh_l5_intents(
    predictor: &crate::memory::intent_prediction::IntentPredictor,
    generator: &crate::memory::mental_model::MentalModelGenerator,
    inputs: &[crate::memory::scenario::Scenario],
    session_id: &str,
) {
    let all = match predictor.predict_all(inputs, generator) {
        Ok(all) => all,
        Err(e) => {
            tracing::warn!("L5: batched prediction failed ({}): {}", session_id, e);
            return;
        }
    };
    save_filtered_list(
        ListDocMeta {
            layer: "L5",
            scan_label: "L5 likely_topics",
            noun: "likely topics",
            gen_label: "likely topics",
            gen_verb: "prediction",
            doc_file: "likely-next-topics.md",
        },
        session_id,
        all.likely_topics,
        |t| &mut t.topics,
        |t| predictor.save_likely_topics(t),
    );
    save_filtered_list(
        ListDocMeta {
            layer: "L5",
            scan_label: "L5 anticipated_needs",
            noun: "anticipated needs",
            gen_label: "anticipated needs",
            gen_verb: "prediction",
            doc_file: "anticipated-needs.md",
        },
        session_id,
        all.anticipated_needs,
        |n| &mut n.needs,
        |n| predictor.save_anticipated_needs(n),
    );
}

fn try_acquire_consolidation_guard<'a>(
    flag: Option<&'a AtomicBool>,
    session_id: &str,
) -> Option<Option<AtomicFlagGuard<'a>>> {
    let Some(flag) = flag else {
        return Some(None);
    };
    if flag
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        tracing::info!(
            "Pipeline L3-L5: consolidation already running, skipping session {}",
            session_id
        );
        return None;
    }
    Some(Some(AtomicFlagGuard(flag)))
}

fn query_sessions_since(
    db: &Arc<Mutex<Db>>,
    memory_dir: &std::path::Path,
    session_id: &str,
) -> Option<(i64, i64, i64)> {
    let l3_ts = last_persona_ts(&memory_dir.join("persona.md"));
    let l4_ts = last_dir_mtime(
        &crate::memory::mental_model::docs_dir(memory_dir),
        &[
            "workflow-patterns.md",
            "decision-framework.md",
            "communication-style.md",
        ],
    );
    let l5_ts = last_dir_mtime(
        &crate::memory::intent_prediction::docs_dir(memory_dir),
        &["likely-next-topics.md", "anticipated-needs.md"],
    );

    let db_guard = lock_db(db);
    db_guard
        .conn()
        .query_row(
            "SELECT COALESCE(SUM(updated_at > ?1), 0), \
                    COALESCE(SUM(updated_at > ?2), 0), \
                    COALESCE(SUM(updated_at > ?3), 0) \
             FROM sessions",
            rusqlite::params![l3_ts, l4_ts, l5_ts],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .map_err(|e| tracing::warn!("L3-L5: session-count failed for {}: {}", session_id, e))
        .ok()
}

/// L3-L5 consolidation refresh: per-layer anchor tracking, single-flight guard,
/// short DB lock for counts/inputs, lock-free LLM calls and file writes.
pub(super) fn run_consolidation(
    db: Arc<Mutex<Db>>,
    llm: Arc<LlmClient>,
    config: Arc<Config>,
    session_id: &str,
    turns: &[TurnContent],
    consolidation_running: Option<&AtomicBool>,
) {
    let Some(_consolidation_guard) =
        try_acquire_consolidation_guard(consolidation_running, session_id)
    else {
        return;
    };

    let memory_dir = config.memory_dir();
    let trigger_every_n = i64::try_from(config.persona.trigger_every_n).unwrap_or(i64::MAX);
    let Some((since_l3, since_l4, since_l5)) = query_sessions_since(&db, &memory_dir, session_id)
    else {
        return;
    };
    let l3_due = layer_due(since_l3, trigger_every_n);
    let l4_due = layer_due(since_l4, trigger_every_n);
    let l5_due = layer_due(since_l5, trigger_every_n);
    if !l3_due && !l4_due && !l5_due {
        return;
    }

    let persona = l3_due
        .then(|| refresh_l3_persona(&db, &llm, &memory_dir, session_id))
        .flatten();
    if !l4_due && !l5_due {
        return;
    }

    let mut inputs = consolidation_inputs(&db, session_id);
    if inputs.is_empty() {
        tracing::debug!(
            "L4/L5: no live atom rows for consolidation input, skipping ({})",
            session_id
        );
        return;
    }
    if let Some(digest) = session_digest(session_id, turns) {
        inputs.insert(0, digest);
    }
    // W16: L4 due 但本轮没有新 persona（L3 未 due 或生成失败）时，读回磁盘
    // 已有 persona.md 镜像作输入——旧实现只在 l3_due 时传 persona，L4 的
    // 生成上下文会静默丢失画像节。
    let disk_persona = (l4_due && persona.is_none())
        .then(|| crate::memory::persona::load_persona_mirror(&memory_dir))
        .flatten();
    let persona_for_l4 = persona.as_ref().or(disk_persona.as_ref());
    let mm =
        crate::memory::mental_model::MentalModelGenerator::new(llm.clone(), memory_dir.clone());
    if l4_due {
        refresh_l4_mental_models(&mm, &inputs, persona_for_l4, session_id);
    }
    if l5_due {
        let predictor =
            crate::memory::intent_prediction::IntentPredictor::new(llm.clone(), memory_dir);
        refresh_l5_intents(&predictor, &mm, &inputs, session_id);
    }
}

/// W16: 按**固定文档清单**取 `.min()`（最旧文档锚定），而非目录全文件 `.max()`：
/// 同组某档刚被改写时，`.max()` 会把锚推到最新，让失败/缺失的兄弟档饿死
/// （计数永远凑不够阈值）。清单内缺失/不可 stat 的文档计 0（= 从未生成），
/// 保证该层持续 due 直到整组补齐。清单与各自 save/load 的文件名一致。
pub(super) fn last_dir_mtime(dir: &std::path::Path, files: &[&str]) -> i64 {
    files
        .iter()
        .map(|name| {
            dir.join(name)
                .metadata()
                .ok()
                .filter(std::fs::Metadata::is_file)
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0)
        })
        .min()
        .unwrap_or(0)
}
