use super::cache::cache_preload_if_current;
use super::{
    apply_token_budget, build_vec_atoms_sql, consolidation_fresh, preload_recall_docs,
    rebuild_context, render_consolidation_doc, PreloadedRecallDocs, RetrievalEngine,
    CONSOLIDATION_FRESHNESS_MS, CONSOLIDATION_RENDER_CAP, PRELOAD_CACHE, RECALL_BANNER,
};
use crate::index::db::Db;
use crate::util::text::estimate_tokens;
use std::path::Path;

/// PRELOAD_CACHE 是进程级共享：封顶测试会触发清空，与"跨调用复用缓存"的
/// 断言测试互斥（否则清空落在 docs1→docs2 窗口内会翻转复用断言）。
static CACHE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn open_db() -> Db {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db
}

// ── moved from http.rs with the implementation (S14a) ──

/// v2.6.1: L2 scenarios are now wired — the pipeline writes
/// `memory_type='scenario'` rows. Pin that recall_scenarios surfaces them
/// (the read path that makes L2 live once the write path runs).
#[test]
fn test_recall_l2_surfaces_scenario_rows() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    db.conn()
        .execute_batch(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type) VALUES \
             ('memory', '旧场景（已被取代）', 500, 500, 'medium', 'scenario'), \
             ('memory', '用户在调试 Rust 的所有权与生命周期', 1000, 1000, 'medium', 'scenario'), \
             ('memory', '普通原子不应混入 L2', 1000, 1000, 'high', 'atom'), \
             ('memory', '手动条目不应混入 L2', 1000, 1000, 'high', 'manual'); \
             UPDATE bounded_memory SET supersedes_id = 1 WHERE id = 2;",
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let scenarios = engine.recall_scenarios(None, None, 10);
    assert_eq!(
        scenarios.len(),
        1,
        "only the live scenario row must surface in L2 (excluding superseded/atom/manual)"
    );
    assert_eq!(scenarios[0]["layer"], "L2");
    assert_eq!(scenarios[0]["type"], "scenario");
    assert_eq!(
        scenarios[0]["content"],
        "用户在调试 Rust 的所有权与生命周期"
    );

    // 时间窗排除与 top_k=0 边界
    assert!(
        engine.recall_scenarios(Some(1001), None, 10).is_empty(),
        "after > updated_at must exclude the scenario"
    );
    assert!(
        engine.recall_scenarios(None, Some(999), 10).is_empty(),
        "before < updated_at must exclude the scenario"
    );
    assert!(
        engine.recall_scenarios(None, None, 0).is_empty(),
        "top_k=0 must return empty"
    );
}

/// U10: the recall context must always lead with the untrusted-data
/// banner (it is injected verbatim into future prompts), while keeping
/// the layer mapping and L0 exclusion intact.
#[test]
fn test_rebuild_context_always_prefends_banner() {
    let empty = rebuild_context(&[]);
    assert_eq!(empty, RECALL_BANNER);

    let memories = vec![
        serde_json::json!({"layer": "L3", "type": "persona", "content": "用户画像"}),
        serde_json::json!({"layer": "L0", "type": "turn", "content": "被排除的原文"}),
        serde_json::json!({"layer": "L1", "type": "atom", "content": "用户偏好 Rust"}),
    ];
    let ctx = rebuild_context(&memories);
    let lines: Vec<&str> = ctx.split('\n').collect();
    assert_eq!(lines[0], RECALL_BANNER);
    assert_eq!(lines[1], "[Persona] 用户画像");
    assert_eq!(lines[2], "[atom] 用户偏好 Rust");
    assert!(!ctx.contains("被排除的原文"), "L0 stays excluded");
}

// ── engine-level L3 persona pinning (S14a: DB row wins over USER.md) ──

#[test]
fn test_recall_persona_prefers_db_row_over_user_md() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("USER.md"), "文件画像").unwrap();
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '数据库画像', 1000, 1000, 'manual')",
            [],
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let items = engine.recall_persona();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["layer"], "L3");
    assert_eq!(items[0]["content"], "数据库画像");
}

#[test]
fn test_recall_persona_falls_back_to_user_md_trimmed() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("USER.md"), "\n  文件画像  \n").unwrap();
    let db = open_db();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let items = engine.recall_persona();
    assert_eq!(items.len(), 1);
    // Legacy fallback trims the file content before surfacing it.
    assert_eq!(items[0]["content"], "文件画像");

    // Empty file → no L3 item at all.
    std::fs::write(tmp.path().join("USER.md"), "   \n").unwrap();
    assert!(engine.recall_persona().is_empty());
}

#[test]
fn test_recall_persona_skips_whitespace_only_db_row() {
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '   ', 1000, 1000, 'manual')",
            [],
        )
        .unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    // Whitespace-only DB row must not count as a persona found (falls
    // through to the USER.md path, which is absent here).
    assert!(engine.recall_persona().is_empty());
}

// ── S14b: persona.md joins the L3 fallback chain ──

/// No DB user row → persona.md is the next source and outranks USER.md.
/// It is served RAW (frontmatter included), trimmed — the same
/// presentation the /persona endpoint gives its persona.md branch.
#[test]
fn test_recall_persona_falls_back_to_persona_md_before_user_md() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(
        tmp.path().join("persona.md"),
        "\n---\ncreated_at: 1000\nupdated_at: 2000\nsupersedes_id: null\n---\n\n# User Persona\n\n## Preferences\n生成画像\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join("USER.md"), "文件画像").unwrap();
    let db = open_db();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let items = engine.recall_persona();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["layer"], "L3");
    assert_eq!(items[0]["type"], "persona");
    let content = items[0]["content"].as_str().unwrap();
    assert!(content.contains("生成画像"));
    assert!(content.starts_with("---\n"), "frontmatter stays verbatim");
    assert!(!content.contains("文件画像"), "USER.md must not surface");
}

/// S14a invariant extended: the DB user row outranks BOTH files.
#[test]
fn test_recall_persona_db_row_wins_over_persona_md() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("persona.md"), "生成画像").unwrap();
    std::fs::write(tmp.path().join("USER.md"), "文件画像").unwrap();
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '数据库画像', 1000, 1000, 'manual')",
            [],
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let items = engine.recall_persona();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["content"], "数据库画像");
}

/// A blank persona.md is skipped — the chain continues to USER.md.
#[test]
fn test_recall_persona_skips_blank_persona_md() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("persona.md"), "  \n\t\n").unwrap();
    std::fs::write(tmp.path().join("USER.md"), "文件画像").unwrap();
    let db = open_db();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let items = engine.recall_persona();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["content"], "文件画像");
}

/// W28c: the L3 file fallback is read by `preload_recall_docs` (outside the
/// DB lock). Deleting the file afterwards must not change what recall
/// serves — proof the in-lock path no longer does a synchronous fs::read.
#[test]
fn test_preloaded_persona_fallback_survives_file_removal() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("persona.md"), "预载画像").unwrap();
    let docs = preload_recall_docs(tmp.path());
    std::fs::remove_file(tmp.path().join("persona.md")).unwrap();

    let db = open_db();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000).with_preloaded_docs(docs);
    let items = engine.recall_persona();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["layer"], "L3");
    assert_eq!(items[0]["content"], "预载画像");
}

/// W28c: the DB user row still outranks the preloaded file fallback, and a
/// preload with no files yields no L3 item (no in-lock read sneaks back).
#[test]
fn test_preloaded_persona_fallback_db_row_still_wins() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("USER.md"), "文件画像").unwrap();
    let docs = preload_recall_docs(tmp.path());
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '数据库画像', 1000, 1000, 'manual')",
            [],
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000).with_preloaded_docs(docs);
    assert_eq!(engine.recall_persona()[0]["content"], "数据库画像");

    let dir2 = tempfile::TempDir::new().unwrap();
    let empty = preload_recall_docs(dir2.path());
    let db2 = open_db();
    let engine2 = RetrievalEngine::new(&db2, dir2.path(), 2000).with_preloaded_docs(empty);
    assert!(engine2.recall_persona().is_empty());
}

#[test]
fn test_apply_token_budget_greedy_prefix() {
    // "你好世界" = 4 CJK tokens each; a mid-list overflow drops the tail
    // even though later items would individually fit (prefix semantics).
    let mut memories = vec![
        serde_json::json!({"layer": "L3", "type": "persona", "content": "你好世界"}),
        serde_json::json!({"layer": "L1", "type": "atom", "content": "abcdefgh"}), // ~3 light tokens
        serde_json::json!({"layer": "L1", "type": "atom", "content": "好"}), // 1 token, would fit
    ];
    let truncated = apply_token_budget(&mut memories, 6);
    assert!(truncated);
    assert_eq!(memories.len(), 1);
    // Exact-equal fit keeps the item (budget == first size).
    let mut single = vec![serde_json::json!({"content": "你好世界"})];
    assert!(!apply_token_budget(&mut single, 4));
    assert_eq!(single.len(), 1);
    assert!(apply_token_budget(&mut single, 3));
    assert!(single.is_empty());
    // No content key → estimated 0, never blocks the prefix.
    let mut odd = vec![
        serde_json::json!({"layer": "L0"}),
        serde_json::json!({"content": "好"}),
    ];
    assert!(!apply_token_budget(&mut odd, 1));
    assert_eq!(odd.len(), 2);
}

// ── S14c: L4 mental models / L5 intent predictions on /recall ──

/// Write all five consolidation documents with the given (unix seconds)
/// `updated_at` through the modules' own save functions — so the test
/// exercises the real file format, not a hand-guessed one.
fn write_consolidation_docs(dir: &Path, updated_at: i64) {
    let llm = std::sync::Arc::new(crate::memory::llm::LlmClient::new("test", "test", "test"));
    let mm = crate::memory::mental_model::MentalModelGenerator::new(llm.clone(), dir.to_path_buf());
    mm.save_workflow_patterns(&crate::memory::mental_model::WorkflowPatterns {
        patterns: vec!["先写测试".to_string(), "小步提交".to_string()],
        updated_at,
    })
    .unwrap();
    mm.save_decision_framework(&crate::memory::mental_model::DecisionFramework {
        criteria: vec!["性能优先".to_string()],
        updated_at,
    })
    .unwrap();
    mm.save_communication_style(&crate::memory::mental_model::CommunicationStyle {
        preferences: vec!["简洁中文".to_string()],
        updated_at,
    })
    .unwrap();
    let ip = crate::memory::intent_prediction::IntentPredictor::new(llm, dir.to_path_buf());
    ip.save_likely_topics(&crate::memory::intent_prediction::LikelyNextTopics {
        topics: vec!["Rust 生命周期".to_string()],
        updated_at,
    })
    .unwrap();
    ip.save_anticipated_needs(&crate::memory::intent_prediction::AnticipatedNeeds {
        needs: vec!["部署脚本模板".to_string()],
        updated_at,
    })
    .unwrap();
}

/// Seed one row per DB-backed layer: L3 persona (target='user'), L2
/// scenario and an L1 atom matching the query word 工作流.
fn seed_db_layers(db: &Db) {
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '画像用户', 1000, 1000, 'manual')",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('memory', '场景摘要内容', 1000, 1000, 'scenario')",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('memory', '用户常用 Rust 工作流', 1000, 1000, 'atom')",
            [],
        )
        .unwrap();
}

fn layers_of(memories: &[serde_json::Value]) -> Vec<&str> {
    memories
        .iter()
        .map(|m| m["layer"].as_str().unwrap_or("?"))
        .collect()
}

/// 7b: fresh L4/L5 docs surface as one item per document, ordered after
/// the L3 persona and before L2 (progressive disclosure, abstract first);
/// `context` carries the layer labels in the same order.
#[test]
fn test_recall_l4_l5_fresh_docs_ordered_after_l3_before_l2() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_consolidation_docs(tmp.path(), chrono::Utc::now().timestamp());
    let db = open_db();
    seed_db_layers(&db);
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let outcome = engine.recall("工作流", 10, None, None, None).unwrap();
    assert_eq!(
        layers_of(&outcome.memories),
        vec!["L3", "L4", "L4", "L4", "L5", "L5", "L2", "L1"],
        "greedy fill order L3 -> L4 -> L5 -> L2 -> L1, got {:?}",
        outcome.memories
    );
    assert_eq!(outcome.memories[1]["type"], "workflow_patterns");
    assert_eq!(outcome.memories[2]["type"], "decision_framework");
    assert_eq!(outcome.memories[3]["type"], "communication_style");
    assert_eq!(outcome.memories[4]["type"], "likely_topics");
    assert_eq!(outcome.memories[5]["type"], "anticipated_needs");
    assert_eq!(
        outcome.memories[1]["content"].as_str().unwrap(),
        "Workflow Patterns: 先写测试; 小步提交"
    );
    let lines: Vec<&str> = outcome.context.split('\n').collect();
    assert_eq!(lines[0], RECALL_BANNER);
    assert!(lines[1].starts_with("[Persona] "));
    assert!(lines[2].starts_with("[MentalModel] Workflow Patterns: "));
    assert!(lines[4].starts_with("[MentalModel] Communication Style: "));
    assert!(lines[5].starts_with("[Intent] Likely Next Topics: "));
    assert!(lines[7].starts_with("[Scenario] "));
}

/// 7b (gate): a doc whose `Updated:` stamp is older than the freshness
/// window vanishes from both `memories` and `context`; `updated_at = 0`
/// (undatable legacy doc) likewise; an item-less doc contributes nothing.
#[test]
fn test_recall_l4_l5_stale_or_undated_docs_skipped() {
    let tmp = tempfile::TempDir::new().unwrap();
    let now = chrono::Utc::now().timestamp();
    let db = open_db();
    seed_db_layers(&db);
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);

    // 8 days old → every L4/L5 item gone, the rest of the chain intact.
    write_consolidation_docs(tmp.path(), now - 8 * 24 * 3600);
    let outcome = engine.recall("工作流", 10, None, None, None).unwrap();
    assert_eq!(layers_of(&outcome.memories), vec!["L3", "L2", "L1"]);
    assert!(!outcome.context.contains("[MentalModel]"));
    assert!(!outcome.context.contains("[Intent]"));

    // Boundary inside the window (6 days) → all five back.
    write_consolidation_docs(tmp.path(), now - 6 * 24 * 3600);
    let outcome = engine.recall("工作流", 10, None, None, None).unwrap();
    assert_eq!(layers_of(&outcome.memories).len(), 8);

    // updated_at = 0 (pre-J9 / mtime-failed files) → not fresh.
    write_consolidation_docs(tmp.path(), 0);
    let outcome = engine.recall("工作流", 10, None, None, None).unwrap();
    assert_eq!(layers_of(&outcome.memories), vec!["L3", "L2", "L1"]);

    // Fresh but empty document → skipped (no blank context line).
    write_consolidation_docs(tmp.path(), now);
    let llm = std::sync::Arc::new(crate::memory::llm::LlmClient::new("t", "t", "t"));
    crate::memory::mental_model::MentalModelGenerator::new(llm, tmp.path().to_path_buf())
        .save_workflow_patterns(&crate::memory::mental_model::WorkflowPatterns {
            patterns: Vec::new(),
            updated_at: now,
        })
        .unwrap();
    let outcome = engine.recall("工作流", 10, None, None, None).unwrap();
    assert_eq!(
        layers_of(&outcome.memories),
        vec!["L3", "L4", "L4", "L5", "L5", "L2", "L1"],
        "only the emptied document drops out"
    );
}

/// No consolidation files at all → the /recall chain is bit-identical to
/// its pre-S14c shape (the S14a/S14b guardrails extended: these tests
/// pin the whole list, so any phantom L4/L5 item would fail them).
#[test]
fn test_recall_without_consolidation_docs_unchanged_layers() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    seed_db_layers(&db);
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let outcome = engine.recall("工作流", 10, None, None, None).unwrap();
    assert_eq!(layers_of(&outcome.memories), vec!["L3", "L2", "L1"]);
}

/// 7c: L4/L5 items join the greedy prefix budget at their chain
/// position — a budget that covers L3+L4+L5 (and nothing more) keeps all
/// six consolidation items, drops the L2/L1 tail and reports truncated.
#[test]
fn test_recall_consolidation_items_participate_in_budget() {
    let tmp = tempfile::TempDir::new().unwrap();
    write_consolidation_docs(tmp.path(), chrono::Utc::now().timestamp());
    let db = open_db();
    seed_db_layers(&db);
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);

    // Exact budget = sum of L3+L4(3)+L5(2) estimates (same yardstick the
    // budget applies): the first item that does not fit (the L2 scenario,
    // index 6 in the full fill order) and everything after it are dropped
    // whole.
    let full = engine.recall("工作流", 10, None, None, None).unwrap();
    let consolidation_budget: usize = full.memories[..6]
        .iter()
        .map(|m| estimate_tokens(m["content"].as_str().unwrap()))
        .sum();
    let outcome = engine
        .recall("工作流", 10, None, None, Some(consolidation_budget))
        .unwrap();
    assert_eq!(
        layers_of(&outcome.memories),
        vec!["L3", "L4", "L4", "L4", "L5", "L5"],
        "abstract layers are kept ahead of L2/L1 under a tight budget"
    );
    assert!(outcome.truncated);
    assert!(outcome.context.contains("[MentalModel]"));
    assert!(!outcome.context.contains("[Scenario]"));
}

/// Freshness gate boundaries (pure): future-skew fresh, exactly-inside
/// fresh, one day beyond stale, 0/negative stale.
#[test]
fn test_consolidation_fresh_boundaries() {
    let now_secs = crate::util::time::now_unix_ms() / 1000;
    assert!(consolidation_fresh(now_secs + 60), "small clock skew fresh");
    assert!(
        !consolidation_fresh(now_secs + 48 * 3600),
        "far-future stamp is stale, not eternally fresh (NB3)"
    );
    assert!(consolidation_fresh(now_secs - 6 * 24 * 3600));
    assert!(
        consolidation_fresh(now_secs - 7 * 24 * 3600 + 60),
        "inside the window inclusive"
    );
    assert!(!consolidation_fresh(now_secs - 8 * 24 * 3600));
    assert!(!consolidation_fresh(0), "undatable document is stale");
    assert!(!consolidation_fresh(-5));
}

/// Render cap: ≤ CONSOLIDATION_RENDER_CAP chars, CJK-safe truncation with
/// an ellipsis; short documents render verbatim.
#[test]
fn test_render_consolidation_doc_caps_at_500_chars() {
    let short = render_consolidation_doc("Likely Next Topics", &["a".to_string()]);
    assert_eq!(short, "Likely Next Topics: a");
    let long = vec!["汉".to_string(); 600];
    let rendered = render_consolidation_doc("Workflow Patterns", &long);
    assert_eq!(rendered.chars().count(), CONSOLIDATION_RENDER_CAP);
    assert!(rendered.ends_with('…'));
    // Byte length must NOT be the yardstick (CJK is 3 bytes/char).
    assert!(rendered.len() > CONSOLIDATION_RENDER_CAP);
}

#[test]
fn test_recall_persona_concatenates_live_db_rows_and_skips_superseded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '旧画像会被覆盖', 500, 500, 'manual')",
            [],
        )
        .unwrap();
    let old_id = db.conn().last_insert_rowid();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type, supersedes_id) \
             VALUES ('user', '用户是系统工程师', 1000, 1000, 'manual', ?1)",
            [old_id],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('user', '常驻上海工作', 2000, 2000, 'manual')",
            [],
        )
        .unwrap();

    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let items = engine.recall_persona();
    assert_eq!(items.len(), 1);
    let content = items[0]["content"].as_str().unwrap();
    assert_eq!(content, "常驻上海工作\n用户是系统工程师");
}

#[test]
fn test_recall_persona_caps_huge_persona_to_protect_budget() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("USER.md"), "我".repeat(3000)).unwrap();
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('memory', '系统并发架构讨论', 1000, 1000, 'atom')",
            [],
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let outcome = engine.recall("系统", 10, None, None, None).unwrap();
    let persona_content = outcome.memories[0]["content"].as_str().unwrap();
    assert!(estimate_tokens(persona_content) <= 800);
    assert!(persona_content.ends_with('…'));
    assert!(
        outcome.memories.iter().any(|m| m["layer"] == "L1"),
        "capped L3 must leave budget room for L1"
    );
}

#[test]
fn test_recall_l2_time_window_filters_by_updated_at() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, memory_type) \
             VALUES ('memory', '早期场景', 1000, 1000, 'scenario'), \
                    ('memory', '近期场景', 5000, 5000, 'scenario')",
            [],
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let recent = engine.recall_scenarios(Some(3000), Some(6000), 10);
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0]["content"], "近期场景");
    assert_eq!(recent[0]["updated_at"], 5000);
}

#[test]
fn test_recall_l1_hybrid_vec_and_fts_rrf_filters_orthogonal_and_superseded() {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();

    let insert_atom = |content: &str, vec: &[f32], supersedes: Option<i64>| -> i64 {
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, supersedes_id) \
                 VALUES ('memory', ?1, 1000, 1000, 'high', 'atom', ?2)",
                rusqlite::params![content, supersedes],
            )
            .unwrap();
        let id = db.conn().last_insert_rowid();
        db.conn()
            .execute(
                "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![id, crate::embedder::quantize_to_int8(vec)],
            )
            .unwrap();
        id
    };

    let buried_id = insert_atom("旧语义事实", &[1.0, 0.0, 0.0], None);
    let _replacement_id = insert_atom("语义同义但无关键词命中", &[0.99, 0.1, 0.0], Some(buried_id));
    let _both_id = insert_atom("Rust 内存管理技巧", &[0.95, 0.3, 0.0], None);
    let _orthogonal_id = insert_atom("完全正交主题", &[0.0, 1.0, 0.0], None);

    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let outcome = engine
        .recall_with_vec("Rust", Some(&[1.0, 0.0, 0.0]), 10, None, None, None)
        .unwrap();
    let l1: Vec<&str> = outcome
        .memories
        .iter()
        .filter(|m| m["layer"] == "L1")
        .filter_map(|m| m["content"].as_str())
        .collect();

    assert!(
        l1.contains(&"Rust 内存管理技巧"),
        "both-hit atom missing: {l1:?}"
    );
    assert!(
        l1.contains(&"语义同义但无关键词命中"),
        "semantic-only atom missing: {l1:?}"
    );
    assert!(
        !l1.contains(&"旧语义事实"),
        "superseded atom leaked: {l1:?}"
    );
    assert!(
        !l1.contains(&"完全正交主题"),
        "orthogonal atom exceeded distance threshold: {l1:?}"
    );
    assert_eq!(
        l1[0], "Rust 内存管理技巧",
        "dual-hit atom should win RRF rank 1"
    );
}

#[test]
fn test_recall_l0_uses_turns_fts_and_punctuation_fallback() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at) \
             VALUES ('s1', 0, 'f.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute_batch(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview) VALUES \
             ('s1', 1, 1000, 'user', '我们在讨论 Rust 的所有权与生命周期'), \
             ('s1', 2, 2000, 'assistant', '标记符号 @@ 测试');",
        )
        .unwrap();

    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let multi = engine
        .recall("Rust 生命周期", 10, None, None, None)
        .unwrap();
    assert!(
        multi
            .memories
            .iter()
            .any(|m| m["layer"] == "L0" && m["content"] == "我们在讨论 Rust 的所有权与生命周期"),
        "multi-word query must match non-adjacent tokens in L0 turns_fts"
    );

    let punct = engine.recall("@@", 10, None, None, None).unwrap();
    assert!(
        punct
            .memories
            .iter()
            .any(|m| m["layer"] == "L0" && m["content"] == "标记符号 @@ 测试"),
        "punctuation-only query must fall back to LIKE"
    );
}

#[test]
fn test_recall_l1_excludes_l2_scenarios_and_l3_persona() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    db.conn()
        .execute_batch(
            "INSERT INTO bounded_memory (id, target, content, created_at, updated_at, confidence, memory_type) VALUES
             (1, 'user', 'alphaunique persona pref', 1000, 1000, 'high', 'manual'),
             (2, 'memory', 'alphaunique scenario summary', 1000, 1000, 'high', 'scenario'),
             (3, 'memory', 'alphaunique atom fact', 1000, 1000, 'high', 'atom');",
        )
        .unwrap();

    let mut qv = vec![0.0f32; db.dimensions()];
    qv[0] = 1.0;
    let vec_bytes = crate::embedder::quantize_to_int8(&qv);
    for id in [1i64, 2, 3] {
        db.conn()
            .execute(
                "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                rusqlite::params![id, &vec_bytes],
            )
            .unwrap();
    }

    let engine = RetrievalEngine::new(&db, tmp.path(), 4000);
    let res = engine
        .recall_with_vec("alphaunique", Some(&qv), 10, None, None, None)
        .unwrap();
    let l1: Vec<&str> = res
        .memories
        .iter()
        .filter(|m| m["layer"] == "L1")
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert_eq!(l1, vec!["alphaunique atom fact"]);
}

/// after+before 双绑定：占位符序号与 time_binds 顺序必须一致；before-only
/// 仍从 ?3 起（before 分支 next+=1 对称修复的回归钉）。
#[test]
fn test_build_vec_atoms_sql_binds_after_and_before() {
    let (sql, binds) = build_vec_atoms_sql(Some(100), Some(200));
    assert!(sql.contains("bm.created_at >= ?3"), "{sql}");
    assert!(sql.contains("bm.created_at <= ?4"), "{sql}");
    assert_eq!(binds, vec![100, 200]);

    let (sql, binds) = build_vec_atoms_sql(None, Some(200));
    assert!(sql.contains("bm.created_at <= ?3"), "{sql}");
    assert!(!sql.contains("bm.created_at >="), "{sql}");
    assert_eq!(binds, vec![200]);

    let (sql, binds) = build_vec_atoms_sql(Some(100), None);
    assert!(sql.contains("bm.created_at >= ?3"), "{sql}");
    assert!(!sql.contains("bm.created_at <="), "{sql}");
    assert_eq!(binds, vec![100]);

    let (sql, binds) = build_vec_atoms_sql(None, None);
    assert!(!sql.contains("bm.created_at >="), "{sql}");
    assert!(!sql.contains("bm.created_at <="), "{sql}");
    assert!(binds.is_empty());
}

/// 查询向量判据：维度不符 / 全零 / 含 NaN/Inf 一律 warn+拒（降级 FTS），
/// 合法向量放行。
#[test]
fn test_is_valid_recall_vec_rejects_invalid_vectors() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();
    let engine = RetrievalEngine::new(&db, Path::new("."), 2000);

    assert!(engine.is_valid_recall_vec(&[1.0, 0.0, 0.0]));
    assert!(!engine.is_valid_recall_vec(&[1.0, 0.0]), "wrong dim");
    assert!(!engine.is_valid_recall_vec(&[0.0, 0.0, 0.0]), "all zero");
    assert!(!engine.is_valid_recall_vec(&[f32::NAN, 0.0, 0.0]), "NaN");
    assert!(
        !engine.is_valid_recall_vec(&[f32::INFINITY, 0.0, 0.0]),
        "infinity"
    );
    assert!(!engine.is_valid_recall_vec(&[]), "empty");
}

use crate::test_support::WarnLog;

/// A4: L0 查询失败（turns/turns_fts 缺失）→ /recall 仍 Ok 且含 L3/L2/L1，
/// L0 空 + warn；FTS 与 LIKE 两条腿都 degrade-to-skip，不再整体失败。
#[test]
fn test_recall_l0_query_failure_degrades_to_skip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    seed_db_layers(&db);
    db.conn()
        .execute_batch("DROP TABLE turns_fts; DROP TABLE turns;")
        .unwrap();

    let engine = RetrievalEngine::new(&db, tmp.path(), 4000);
    let log = WarnLog::default();
    let res = crate::test_support::with_warn_default(log.clone(), || {
        engine.recall("工作流", 10, None, None, None).unwrap()
    });
    let layers = layers_of(&res.memories);
    assert!(layers.contains(&"L3"), "L3 must survive: {layers:?}");
    assert!(layers.contains(&"L2"), "L2 must survive: {layers:?}");
    assert!(layers.contains(&"L1"), "L1 must survive: {layers:?}");
    assert!(!layers.contains(&"L0"), "L0 must be skipped, not fatal");
    let msgs = log.0.lock().unwrap().clone();
    assert!(
        msgs.iter()
            .any(|m| m.contains("recall L0") && m.contains("skipping L0")),
        "L0 failure must warn: {msgs:?}"
    );

    // LIKE 腿（纯标点查询）同样降级，不整体失败。
    let log2 = WarnLog::default();
    let punct = crate::test_support::with_warn_default(log2.clone(), || {
        engine.recall("@@", 10, None, None, None).unwrap()
    });
    assert!(!layers_of(&punct.memories).contains(&"L0"));
    assert!(
        log2.0
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.contains("recall L0")),
        "LIKE leg failure must warn too"
    );
}

/// C13: (mtime,len) 不变复用缓存——同长内容改写但 mtime 复原仍命中
/// （钉住签名口径，证明未全量重读）；mtime 前进则失效重读；目录键隔离。
#[test]
fn test_preload_cache_reuses_unchanged_and_invalidates_on_mtime() {
    let _guard = CACHE_TEST_LOCK.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let persona = tmp.path().join("persona.md");
    std::fs::write(&persona, "画像甲").unwrap();
    let docs1 = preload_recall_docs(tmp.path());
    assert_eq!(docs1.persona_fallback.as_deref(), Some("画像甲"));

    let orig_mtime = std::fs::metadata(&persona).unwrap().modified().unwrap();
    std::fs::write(&persona, "画像乙").unwrap(); // 同长（9 字节）
    std::fs::File::options()
        .write(true)
        .open(&persona)
        .unwrap()
        .set_modified(orig_mtime)
        .unwrap();
    let docs2 = preload_recall_docs(tmp.path());
    assert_eq!(
        docs2.persona_fallback.as_deref(),
        Some("画像甲"),
        "signature unchanged must reuse the cache, not re-read"
    );

    // 正常写入（mtime 前进）→ 失效重读。
    std::fs::write(&persona, "画像丙").unwrap();
    let docs3 = preload_recall_docs(tmp.path());
    assert_eq!(docs3.persona_fallback.as_deref(), Some("画像丙"));

    // 不同目录键隔离。
    let tmp2 = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp2.path().join("persona.md"), "另一画像").unwrap();
    let other = preload_recall_docs(tmp2.path());
    assert_eq!(other.persona_fallback.as_deref(), Some("另一画像"));
}

/// C13: 缓存不得活过新鲜度门——文档到期后即使文件未变（签名命中）也
/// 必须重建重 gate。构造 updated_at = now - 7d + 2s，约 2s 后到期。
#[test]
fn test_preload_cache_expires_with_freshness_gate() {
    let tmp = tempfile::TempDir::new().unwrap();
    let almost_expired = chrono::Utc::now().timestamp() - CONSOLIDATION_FRESHNESS_MS / 1000 + 2;
    write_consolidation_docs(tmp.path(), almost_expired);
    let docs1 = preload_recall_docs(tmp.path());
    assert_eq!(docs1.mental_models.len(), 3, "fresh docs must be cached");
    assert_eq!(docs1.intents.len(), 2);

    std::thread::sleep(std::time::Duration::from_millis(2100));
    let docs2 = preload_recall_docs(tmp.path());
    assert!(
        docs2.mental_models.is_empty(),
        "expired docs must be re-gated, not served from cache"
    );
    assert!(docs2.intents.is_empty());
}

/// W13 + W15 回归：非 NotFound 的文件读取失败须记 warn，且不得将残缺结果写入 PRELOAD_CACHE。
#[test]
fn test_w13_preload_cache_skips_caching_on_non_notfound_io_error() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("USER.md"), "fallback user doc").unwrap();
    // 把 persona.md 建为目录：std::fs::read_to_string 会触发非 NotFound I/O 错误
    std::fs::create_dir_all(tmp.path().join("persona.md")).unwrap();

    let log = WarnLog::default();
    let docs =
        crate::test_support::with_warn_default(log.clone(), || preload_recall_docs(tmp.path()));
    assert_eq!(docs.persona_fallback.as_deref(), Some("fallback user doc"));
    let msgs = log.0.lock().unwrap().clone();
    assert!(
        msgs.iter()
            .any(|m| m.contains("recall persona fallback read error")),
        "非 NotFound I/O 错误必须记 warn: {msgs:?}"
    );

    let cache = PRELOAD_CACHE.get().unwrap();
    let guard = crate::util::recover_poison(cache);
    assert!(
        !guard.contains_key(tmp.path()),
        "W13: 存在 I/O 失败时不完整结果不得入 PRELOAD_CACHE"
    );
}

/// W16 回归：签名快照早于读盘——读盘窗口内文件代际前进后，陈旧结果不得
/// 覆盖更新的缓存条目。cache_preload_if_current 入缓存前复核签名，
/// 不一致即放弃（vec![None; 7] 在 persona.md 存在时必与真实签名不符）。
#[test]
fn test_w16_preload_cache_stale_insert_does_not_overwrite_newer() {
    let _guard = CACHE_TEST_LOCK.lock().unwrap();
    let tmp = tempfile::TempDir::new().unwrap();
    let persona = tmp.path().join("persona.md");
    std::fs::write(&persona, "画像甲").unwrap();
    let docs1 = preload_recall_docs(tmp.path());
    assert_eq!(docs1.persona_fallback.as_deref(), Some("画像甲"));

    let stale = PreloadedRecallDocs {
        mental_models: Vec::new(),
        intents: Vec::new(),
        persona_fallback: Some("陈旧画像".to_string()),
    };
    // 陈旧代际的插入尝试必须被拒（签名复核失败）。
    cache_preload_if_current(tmp.path(), vec![None; 7], &stale, i64::MAX);
    let docs2 = preload_recall_docs(tmp.path());
    assert_eq!(
        docs2.persona_fallback.as_deref(),
        Some("画像甲"),
        "签名不符的陈旧结果不得入缓存"
    );

    // 代际前进 → 新条目入缓存；再来一次陈旧插入仍不得覆盖。
    std::fs::write(&persona, "画像乙").unwrap();
    let docs3 = preload_recall_docs(tmp.path());
    assert_eq!(docs3.persona_fallback.as_deref(), Some("画像乙"));
    cache_preload_if_current(tmp.path(), vec![None; 7], &stale, i64::MAX);
    let docs4 = preload_recall_docs(tmp.path());
    assert_eq!(
        docs4.persona_fallback.as_deref(),
        Some("画像乙"),
        "陈旧插入不得覆盖更新的缓存代际"
    );
}

/// W16 回归：缓存条目数封顶（≥32 清空再插），任意时刻持锁观测 ≤32。
#[test]
fn test_w16_preload_cache_capacity_bounded() {
    let _guard = CACHE_TEST_LOCK.lock().unwrap();
    let mut tmps = Vec::new();
    for i in 0..40 {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("persona.md"), format!("画像{i}")).unwrap();
        preload_recall_docs(tmp.path());
        tmps.push(tmp);
    }
    let cache = PRELOAD_CACHE.get().unwrap();
    let guard = crate::util::recover_poison(cache);
    assert!(
        guard.len() <= 32,
        "cache must stay bounded: {}",
        guard.len()
    );
}

/// M6b 回归：空/纯空白查询不得触发 LIKE '%%' 全表扫描或任意空白匹配——
/// L0 直接返回空（/recall 入口已 400 拒空，此为引擎层防御）。
#[test]
fn test_m6b_recall_turns_empty_and_whitespace_queries_return_empty() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at) \
             VALUES ('s1', 0, 'f.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    db.conn()
        .execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview) \
             VALUES ('s1', 1, 1000, 'user', '含  双空格与@@标记的正文')",
            [],
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    assert!(engine.recall_turns("", None, None, 10).is_empty());
    assert!(engine.recall_turns("   ", None, None, 10).is_empty());
    assert!(engine.recall_turns("\t\n", None, None, 10).is_empty());
    // 非空白标点查询的 LIKE 回退不受影响（既有契约）。
    assert!(!engine.recall_turns("@@", None, None, 10).is_empty());
}

/// M6b 回归：recall_scenarios 与写侧同口径过滤 target='memory'——
/// 非 memory 目标的杂散 scenario 行不得混入 L2。
#[test]
fn test_m6b_recall_scenarios_filters_non_memory_target() {
    let tmp = tempfile::TempDir::new().unwrap();
    let db = open_db();
    db.conn()
        .execute_batch(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type) VALUES \
             ('memory', '正常场景', 1000, 1000, 'medium', 'scenario'), \
             ('user', '杂散用户目标场景', 2000, 2000, 'medium', 'scenario');",
        )
        .unwrap();
    let engine = RetrievalEngine::new(&db, tmp.path(), 2000);
    let scenarios = engine.recall_scenarios(None, None, 10);
    assert_eq!(scenarios.len(), 1, "only target='memory' rows surface");
    assert_eq!(scenarios[0]["content"], "正常场景");
}

/// M6b 边界：budget=0 丢弃一切非零估算项（truncated=true），零估算项不阻塞。
#[test]
fn test_m6b_apply_token_budget_zero_budget() {
    let mut memories = vec![
        serde_json::json!({"layer": "L3", "content": "好"}),
        serde_json::json!({"layer": "L0"}), // 无 content → 估算 0
    ];
    assert!(apply_token_budget(&mut memories, 0));
    assert!(
        memories.is_empty(),
        "budget=0 drops the first non-zero item and the tail"
    );
    let mut zero_only = vec![serde_json::json!({"layer": "L0"})];
    assert!(!apply_token_budget(&mut zero_only, 0));
    assert_eq!(zero_only.len(), 1);
}
