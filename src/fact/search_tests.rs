use super::*;

fn setup_test_data(db: &Db) {
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
         VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();

    db.conn()
        .execute_batch(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview) VALUES
         (1, 's1', 1, 1000, 'user', 'Rust is a systems programming language'),
         (2, 's1', 2, 2000, 'assistant', 'Python is great for data science'),
         (3, 's1', 3, 3000, 'user', 'I love Rust for its memory safety');",
        )
        .unwrap();
}

#[test]
fn test_keyword_search() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Keyword,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };

    let results = search_sessions(&db, None, &params).unwrap();
    assert_eq!(results.len(), 2, "only the 2 Rust turns must match");
    let mut ids: Vec<i64> = results.iter().map(|r| r.turn_id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 3], "matched turn IDs must be exactly [1, 3]");
    assert!(
        results
            .iter()
            .all(|r| r.preview.contains("Rust") && !r.preview.contains("Python")),
        "Python distractor turn must never leak into Rust keyword results: {results:?}"
    );

    // 大小写无关匹配应返回相同集合
    let lower = SearchParams {
        query: "rust".to_string(),
        ..params
    };
    let lower_res = search_sessions(&db, None, &lower).unwrap();
    assert_eq!(lower_res.len(), 2);

    // 无关词必须返回空结果
    let miss = SearchParams {
        query: "Haskell".to_string(),
        ..lower
    };
    assert!(
        search_sessions(&db, None, &miss).unwrap().is_empty(),
        "non-matching keyword must return empty"
    );
}

#[test]
fn test_hybrid_without_embedder() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };

    // 没有 embedder 时，hybrid 退化为 keyword
    let results = search_sessions(&db, None, &params).unwrap();
    assert!(results.len() >= 2);
}

/// Hybrid + Some(embedder) 但 embed_query 失败 → warn + keyword-only 降级
/// （包装层 embed-failure 臂；既有测试只覆盖 None-embedder 与 with_vec 腿失败）。
/// 离线构造：api_url="broken/url" → ureq InvalidUrl 非重试，立即 Err，零网络 I/O。
#[test]
fn test_hybrid_embed_failure_degrades_to_keyword() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);
    let mut cfg = crate::config::Config::default().embedding;
    cfg.api_url = "broken/url".to_string();
    cfg.api_model = "m".to_string();
    let emb = crate::embedder::LazyEmbedder::from_config(&cfg)
        .expect("非空 URL/model 构造成功（InvalidUrl 在请求期才失败）");
    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let results = search_sessions(&db, Some(&emb), &params).unwrap();
    assert!(results.len() >= 2, "降级 keyword-only 仍须命中 Rust turns");
    for r in &results {
        let bd = r.scores.as_ref().unwrap();
        assert!(bd.semantic.is_none(), "embed 失败不得有 semantic 分量");
    }
}

#[test]
fn test_time_range_filter() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Keyword,
        top_k: 5,
        after_ms: Some(1500),
        before_ms: None,
        role: None,
    };

    let results = search_sessions(&db, None, &params).unwrap();
    assert_eq!(results.len(), 1);
}

/// v2.6 score transparency: keyword mode exposes the keyword component only,
/// and it equals the top-level score.
#[test]
fn test_score_breakdown_keyword() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Keyword,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };

    let results = search_sessions(&db, None, &params).unwrap();
    assert!(!results.is_empty());
    for r in &results {
        let bd = r
            .scores
            .as_ref()
            .expect("keyword results carry a breakdown");
        assert!(
            bd.semantic.is_none(),
            "keyword mode has no semantic component"
        );
        assert_eq!(bd.keyword, Some(r.score), "keyword component equals score");
    }
}

/// Hybrid without embedder degrades to keyword-only RRF: the breakdown must
/// carry keyword only, and the components must sum back to the fused score.
#[test]
fn test_score_breakdown_hybrid_without_embedder() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };

    let results = search_sessions(&db, None, &params).unwrap();
    assert!(!results.is_empty());
    for r in &results {
        let bd = r.scores.as_ref().expect("hybrid results carry a breakdown");
        assert!(
            bd.semantic.is_none(),
            "no embedder → no semantic contribution"
        );
        let sum = bd.semantic.unwrap_or(0.0) + bd.keyword.unwrap_or(0.0);
        assert!(
            (sum - r.score).abs() < 1e-12,
            "components must sum to the fused score: {:?} vs {}",
            bd,
            r.score
        );
    }
}

#[test]
fn test_role_filter() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Keyword,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: Some("assistant".to_string()),
    };

    let results = search_sessions(&db, None, &params).unwrap();
    assert_eq!(results.len(), 0); // assistant 回复里没有 "Rust"
}

/// Role filter must not under-return: when more than top_k matching-role turns
/// exist (interleaved with distractor turns that rank among them), the filtered
/// keyword search should still return a full top_k.
#[test]
fn test_role_filter_does_not_under_return() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
         VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    // 6 user + 6 assistant turns all matching "Rust"
    for i in 0..6 {
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('s1', ?1, ?2, 'user', 'Rust topic')",
                rusqlite::params![i * 2 + 1, (i * 2 + 1) as i64],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('s1', ?1, ?2, 'assistant', 'Rust reply')",
                rusqlite::params![i * 2 + 2, (i * 2 + 2) as i64],
            )
            .unwrap();
    }

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Keyword,
        top_k: 3,
        after_ms: None,
        before_ms: None,
        role: Some("user".to_string()),
    };

    let results = search_sessions(&db, None, &params).unwrap();
    assert_eq!(
        results.len(),
        3,
        "should return full top_k when enough matching-role turns exist"
    );
    assert!(results.iter().all(|r| r.role == "user"));
}

// ── U15: search_sessions_with_vec（预计算向量入口，无内部网络调用） ──

/// 注入轴向量：turn 1 → [1,0,..]，turn 3 → [0,1,..]（正交），
/// 量化往返对 0/1 分量精确，KNN 排序可断言。
fn insert_axis_vecs(db: &Db) {
    let store = VectorStore::new(db);
    let mut v1 = vec![0.0f32; db.dimensions()];
    v1[0] = 1.0;
    store.insert(1, &v1).unwrap();
    let mut v3 = vec![0.0f32; db.dimensions()];
    v3[1] = 1.0;
    store.insert(3, &v3).unwrap();
}

fn axis_query(db: &Db, axis: usize) -> Vec<f32> {
    let mut q = vec![0.0f32; db.dimensions()];
    q[axis] = 1.0;
    q
}

/// with_vec 的 semantic 路径直接使用注入向量（无需 embedder）：
/// 与轴 0 向量同向的 turn 1 排第一且余弦距离 ≈ 0（score ≈ 1.0）。
#[test]
fn test_with_vec_semantic_uses_injected_vector() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);
    insert_axis_vecs(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Semantic,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let results = search_sessions_with_vec(&db, Some(axis_query(&db, 0)), &params).unwrap();
    assert_eq!(results.len(), 2, "两个已索引向量都参与 KNN");
    assert_eq!(results[0].turn_id, 1, "同方向量的 turn 排第一");
    assert!(
        (results[0].score - 1.0).abs() < 0.01,
        "同向余弦相似度 ≈ 1.0, got {}",
        results[0].score
    );
    assert_eq!(results[1].turn_id, 3);
    assert!(results[1].score < results[0].score);
    let bd = results[0].scores.as_ref().unwrap();
    assert!(bd.semantic.is_some() && bd.keyword.is_none());
}

/// semantic_result 时间过滤臂 + 语义 over-fetch 触发：after_ms/before_ms 在
/// KNN 之后于 Rust 侧过滤（既有时间过滤测试只走 Keyword，语义侧零覆盖）。
#[test]
fn test_with_vec_semantic_time_filter() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db); // turn1 ts1000、turn3 ts3000（均注入轴向量）
    insert_axis_vecs(&db);

    // after_ms=1500 → turn1(1000) 被滤，仅 turn3(3000)
    let after = search_sessions_with_vec(
        &db,
        Some(axis_query(&db, 0)),
        &SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Semantic,
            top_k: 5,
            after_ms: Some(1500),
            before_ms: None,
            role: None,
        },
    )
    .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].turn_id, 3);

    // before_ms=1500 → turn3(3000) 被滤，仅 turn1(1000)
    let before = search_sessions_with_vec(
        &db,
        Some(axis_query(&db, 0)),
        &SearchParams {
            query: "Rust".to_string(),
            search_mode: SearchMode::Semantic,
            top_k: 5,
            after_ms: None,
            before_ms: Some(1500),
            role: None,
        },
    )
    .unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].turn_id, 1);
}

/// semantic_search 空 KNN 早退与孤儿向量跳过（semantic_result turns.get None）：
/// 无向量 → 空；有向量但 turns 行已删（vec0 无 FK，向量残留）→ 孤儿静默跳过 → 空。
#[test]
fn test_with_vec_semantic_empty_knn_and_orphan_skip() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);
    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Semantic,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    // 无向量 → KNN 空 → 早退空
    assert!(
        search_sessions_with_vec(&db, Some(axis_query(&db, 0)), &params)
            .unwrap()
            .is_empty()
    );
    // 注入向量后删除全部 turns 行 → 向量成孤儿
    insert_axis_vecs(&db);
    db.conn().execute_batch("DELETE FROM turns;").unwrap();
    let orphan = search_sessions_with_vec(&db, Some(axis_query(&db, 0)), &params).unwrap();
    assert!(orphan.is_empty(), "孤儿向量（turns 行缺失）须被静默跳过");
}

/// with_vec semantic 模式 + query_vec=None → 保持原错误消息
/// "语义搜索需要嵌入引擎"（包装与直接入口一致）。
#[test]
fn test_with_vec_semantic_without_vector_errors() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Semantic,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let err = search_sessions_with_vec(&db, None, &params).unwrap_err();
    assert!(err.to_string().contains("语义搜索需要嵌入引擎"));
    // 包装（无 embedder）产生同一消息
    let err2 = search_sessions(&db, None, &params).unwrap_err();
    assert_eq!(err.to_string(), err2.to_string());
}

/// with_vec hybrid + query_vec=None → 既有 keyword-only 降级路径：
/// 命中含 "Rust" 的 turn，且 breakdown 无 semantic 分量。
#[test]
fn test_with_vec_hybrid_none_degrades_to_keyword() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);
    // 即使向量存在，None 向量也不参与——只走关键词
    insert_axis_vecs(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let results = search_sessions_with_vec(&db, None, &params).unwrap();
    assert!(results.len() >= 2, "keyword-only 降级仍须命中 Rust turns");
    for r in &results {
        let bd = r.scores.as_ref().unwrap();
        assert!(bd.semantic.is_none(), "无向量不得有 semantic 分量");
    }
}

/// with_vec hybrid + Some(向量) → 语义与关键词两路 RRF 融合：
/// 同时命中的 turn 1 两项分量齐备（排名第一不断言——BM25 名次可致 RRF 并列）。
#[test]
fn test_with_vec_hybrid_fuses_injected_vector() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);
    insert_axis_vecs(&db);

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let results = search_sessions_with_vec(&db, Some(axis_query(&db, 0)), &params).unwrap();
    assert!(!results.is_empty());
    // turn 1 同时被语义（同向量）与关键词（"Rust"）命中 → 双贡献。
    // 不断言其排名第一：keyword 侧 BM25 名次可致 RRF 总分并列。
    let t1 = results
        .iter()
        .find(|r| r.turn_id == 1)
        .expect("turn 1 must survive RRF fusion");
    let bd = t1.scores.as_ref().unwrap();
    assert!(
        bd.semantic.is_some() && bd.keyword.is_some(),
        "semantic+keyword 双贡献：{:?}",
        bd
    );
    let sum = bd.semantic.unwrap() + bd.keyword.unwrap();
    assert!((sum - t1.score).abs() < 1e-12);
}

#[test]
fn test_search_top_k_zero_short_circuits() {
    let db = Db::open_memory().unwrap();
    db.init_schema().unwrap();
    setup_test_data(&db);

    for mode in [
        SearchMode::Keyword,
        SearchMode::Semantic,
        SearchMode::Hybrid,
    ] {
        let params = SearchParams {
            query: "Rust".to_string(),
            search_mode: mode,
            top_k: 0,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        assert!(search_sessions(&db, None, &params).unwrap().is_empty());
        assert!(search_sessions_with_vec(&db, None, &params)
            .unwrap()
            .is_empty());
    }
}

#[test]
fn test_hybrid_surfaces_error_when_both_legs_fail() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();
    setup_test_data(&db);
    // Drop FTS and vector tables so both legs fail deterministically.
    db.conn()
        .execute_batch("DROP TABLE turns_fts; DROP TABLE vec_turns;")
        .unwrap();

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let err_kw = search_sessions_with_vec(&db, None, &params).unwrap_err();
    assert!(!err_kw.to_string().is_empty());

    let err_both = search_sessions_with_vec(&db, Some(vec![1.0, 0.0, 0.0]), &params)
        .unwrap_err()
        .to_string();
    assert!(
        err_both.contains("hybrid search failed on both legs"),
        "unexpected error: {err_both}"
    );
}

#[test]
fn test_hybrid_wider_candidate_pool_and_deterministic_tie_break() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    let vec_store = VectorStore::new(&db);
    // Seed 3 turns that all contain "Rust" AND have identical semantic vectors
    // with distinct timestamps: newest (timestamp=3000, turn_id=3) must rank predictably on ties.
    for (id, ts) in [(1i64, 1000i64), (2, 2000), (3, 3000)] {
        db.conn()
            .execute(
                "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview)
                 VALUES (?1, 's1', ?1, ?2, 'user', 'Rust async note')",
                rusqlite::params![id, ts],
            )
            .unwrap();
        vec_store.insert(id, &[1.0, 0.0, 0.0]).unwrap();
    }
    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 2,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let first = search_sessions_with_vec(&db, Some(vec![1.0, 0.0, 0.0]), &params).unwrap();
    let second = search_sessions_with_vec(&db, Some(vec![1.0, 0.0, 0.0]), &params).unwrap();
    assert_eq!(first.len(), 2);
    let ids1: Vec<i64> = first.iter().map(|r| r.turn_id).collect();
    let ids2: Vec<i64> = second.iter().map(|r| r.turn_id).collect();
    assert_eq!(
        ids1, ids2,
        "hybrid tie-breaking must be deterministic across calls"
    );
}

#[test]
fn test_search_mode_from_str_and_cli() {
    use std::str::FromStr;

    assert_eq!(SearchMode::from_str("hybrid").unwrap(), SearchMode::Hybrid);
    assert_eq!(
        SearchMode::from_str("semantic").unwrap(),
        SearchMode::Semantic
    );
    assert_eq!(
        SearchMode::from_str("keyword").unwrap(),
        SearchMode::Keyword
    );
    assert!(SearchMode::from_str("vector").is_err());
    assert!(SearchMode::from_str("unknown").is_err());

    assert_eq!(SearchMode::from_cli_str("vector"), SearchMode::Semantic);
    assert_eq!(SearchMode::from_cli_str("semantic"), SearchMode::Semantic);
    assert_eq!(SearchMode::from_cli_str("fts"), SearchMode::Keyword);
    assert_eq!(SearchMode::from_cli_str("keyword"), SearchMode::Keyword);
    assert_eq!(SearchMode::from_cli_str("other"), SearchMode::Hybrid);
}

/// hybrid 单腿失败降级（语义腿）：vec_turns 缺失 → 语义腿 Err、关键词腿 Ok
/// → resolve_hybrid_legs 降级为仅关键词（warn），不整体失败。
#[test]
fn test_hybrid_semantic_leg_failure_degrades_to_keyword() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();
    setup_test_data(&db);
    db.conn().execute_batch("DROP TABLE vec_turns;").unwrap();

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let results = search_sessions_with_vec(&db, Some(vec![1.0, 0.0, 0.0]), &params).unwrap();
    assert!(!results.is_empty(), "语义腿失败应降级为关键词而非整体失败");
    for r in &results {
        let bd = r.scores.as_ref().unwrap();
        assert!(bd.semantic.is_none(), "降级后不得有 semantic 分量");
        assert!(bd.keyword.is_some());
    }
}

/// hybrid 单腿失败降级（关键词腿）：turns_fts 缺失 → 关键词腿 Err、语义腿 Ok
/// → 降级为仅语义（warn），不整体失败。
#[test]
fn test_hybrid_keyword_leg_failure_degrades_to_semantic() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();
    setup_test_data(&db);
    insert_axis_vecs(&db);
    db.conn().execute_batch("DROP TABLE turns_fts;").unwrap();

    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Hybrid,
        top_k: 5,
        after_ms: None,
        before_ms: None,
        role: None,
    };
    let results = search_sessions_with_vec(&db, Some(axis_query(&db, 0)), &params).unwrap();
    assert!(!results.is_empty(), "关键词腿失败应降级为语义而非整体失败");
    for r in &results {
        let bd = r.scores.as_ref().unwrap();
        assert!(bd.keyword.is_none(), "降级后不得有 keyword 分量");
        assert!(bd.semantic.is_some());
    }
}

/// 语义腿带 role 过滤时 over-fetch（×5）保证不欠返回：12 个同向量 turn
/// （6 user + 6 assistant），top_k=3 + role=user → 过滤后仍满 3 条 user。
#[test]
fn test_semantic_role_filter_overfetch_full_top_k() {
    let mut db = Db::open_memory().unwrap();
    db.set_dimensions(3);
    db.init_schema().unwrap();
    db.conn()
        .execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('s1', 0, 'test.jsonl', 0, 0)",
            [],
        )
        .unwrap();
    let vec_store = VectorStore::new(&db);
    for i in 0..12i64 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        db.conn()
            .execute(
                "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview)
                 VALUES (?1, 's1', ?1, ?2, ?3, 'Rust note')",
                rusqlite::params![i + 1, i + 1, role],
            )
            .unwrap();
        vec_store.insert(i + 1, &[1.0, 0.0, 0.0]).unwrap();
    }
    let params = SearchParams {
        query: "Rust".to_string(),
        search_mode: SearchMode::Semantic,
        top_k: 3,
        after_ms: None,
        before_ms: None,
        role: Some("user".to_string()),
    };
    let results = search_sessions_with_vec(&db, Some(vec![1.0, 0.0, 0.0]), &params).unwrap();
    assert_eq!(results.len(), 3, "over-fetch 后过滤仍须满 top_k");
    assert!(results.iter().all(|r| r.role == "user"));
}
