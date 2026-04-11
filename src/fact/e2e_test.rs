#[cfg(test)]
mod tests {
    use crate::fact::conversation::{SessionHeader, Turn};
    use crate::fact::search::{search_sessions, SearchMode, SearchParams};
    use crate::fact::session_store::SessionStore;
    use crate::index::db::Db;
    use crate::index::rebuild;

    fn make_header(session_id: &str) -> SessionHeader {
        SessionHeader {
            v: 1,
            header_type: "session_header".to_string(),
            session_id: session_id.to_string(),
            start_time: "2026-04-11T22:00:00+08:00".to_string(),
            profile_id: "default".to_string(),
            source: Some("e2e-test".to_string()),
            agent_model: None,
            title: Some("端到端测试".to_string()),
            tags: vec![],
        }
    }

    fn setup_db() -> (Db, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        (db, tmp)
    }

    fn keyword_search(db: &Db, query: &str) -> Vec<crate::fact::search::SearchResult> {
        let params = SearchParams {
            query: query.to_string(),
            search_mode: SearchMode::Keyword,
            top_k: 10,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        search_sessions(db, None, &params).unwrap()
    }

    fn hybrid_search(db: &Db, query: &str) -> Vec<crate::fact::search::SearchResult> {
        let params = SearchParams {
            query: query.to_string(),
            search_mode: SearchMode::Hybrid,
            top_k: 10,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        search_sessions(db, None, &params).unwrap()
    }

    // ──────────────────────────────────────────────────
    // 场景 1：导入后立即搜索（不 rebuild）
    // ──────────────────────────────────────────────────

    #[test]
    fn test_e2e_keyword_search_immediately_after_save() {
        let (db, tmp) = setup_db();
        let store = SessionStore::new(tmp.path(), &db);

        let turns = vec![
            Turn {
                ts: "2026-04-11T22:00:00+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "亚丝娜已经配置好系统了".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-11T22:00:05+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "做事必须完美，这是原则".to_string(),
                metadata: None,
            },
        ];

        // save（不 rebuild）
        store.save(&make_header("e2e-immediate"), &turns, None).unwrap();

        // 1. turns 表有记录
        let turn_count: i64 = db.conn().query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0)).unwrap();
        assert_eq!(turn_count, 2, "turns 表应有 2 条记录");

        // 2. turns_fts 有对应 rowid
        let fts_count: i64 = db.conn().query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(fts_count, 2, "turns_fts 应有 2 条记录");

        // 3. FTS MATCH 命中
        let tokenized = crate::util::text::tokenize_chinese("亚丝娜");
        let match_count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH ?1",
            [&tokenized], |r| r.get(0)
        ).unwrap_or(0);
        assert!(match_count > 0, "FTS MATCH '亚丝娜' 应命中，实际 {}", match_count);

        // 4. CLI keyword 搜索立即命中
        let results = keyword_search(&db, "亚丝娜");
        assert!(!results.is_empty(), "save 后立即 keyword 搜索 '亚丝娜' 必须命中");
        assert!(results.iter().any(|r| r.preview.contains("亚丝娜")));

        // 5. 另一个关键词也能命中
        let results2 = keyword_search(&db, "做事必须完美");
        assert!(!results2.is_empty(), "save 后立即 keyword 搜索 '做事必须完美' 必须命中");
    }

    // ──────────────────────────────────────────────────
    // 场景 2：rebuild 后一致性
    // ──────────────────────────────────────────────────

    #[test]
    fn test_e2e_consistency_after_rebuild() {
        let (db, tmp) = setup_db();
        let store = SessionStore::new(tmp.path(), &db);

        let turns = vec![
            Turn {
                ts: "2026-04-11T22:00:00+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "Rust 的所有权机制很独特".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-11T22:00:05+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "是的，Rust 通过所有权和借用检查器保证内存安全".to_string(),
                metadata: None,
            },
        ];

        store.save(&make_header("e2e-rebuild"), &turns, None).unwrap();

        // 重建索引
        let stats = rebuild::rebuild_from_jsonl(tmp.path(), &db, None).unwrap();
        assert_eq!(stats.sessions_processed, 1);
        assert_eq!(stats.turns_indexed, 2);
        assert!(stats.errors.is_empty(), "rebuild 不应有错误: {:?}", stats.errors);

        // 1. keyword 命中
        let kw = keyword_search(&db, "Rust 所有权");
        assert!(!kw.is_empty(), "rebuild 后 keyword 'Rust 所有权' 必须命中");

        // 2. hybrid 命中
        let hy = hybrid_search(&db, "Rust 所有权");
        assert!(!hy.is_empty(), "rebuild 后 hybrid 'Rust 所有权' 必须命中");

        // 3. 数据库一致性：turns 和 turns_fts 行数一致
        let turn_count: i64 = db.conn().query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0)).unwrap();
        let fts_count: i64 = db.conn().query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(turn_count, fts_count, "rebuild 后 turns ({}) 和 turns_fts ({}) 数量应一致", turn_count, fts_count);
    }

    // ──────────────────────────────────────────────────
    // 场景 3：数据库一致性（FTS rowid 映射）
    // ──────────────────────────────────────────────────

    #[test]
    fn test_e2e_fts_rowid_consistency() {
        let (db, tmp) = setup_db();
        let store = SessionStore::new(tmp.path(), &db);

        let turns = vec![
            Turn {
                ts: "2026-04-11T22:00:00+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "记忆进化是一个重要功能".to_string(),
                metadata: None,
            },
        ];

        store.save(&make_header("e2e-consistency"), &turns, None).unwrap();

        // turns 有记录
        let turn_id: i64 = db.conn().query_row("SELECT id FROM turns LIMIT 1", [], |r| r.get(0)).unwrap();

        // turns_fts 有对应 rowid
        let fts_rowid: i64 = db.conn().query_row(
            "SELECT rowid FROM turns_fts WHERE rowid = ?1",
            [turn_id], |r| r.get(0)
        ).unwrap();
        assert_eq!(turn_id, fts_rowid, "turns.id 和 turns_fts.rowid 应一致");

        // MATCH 命中
        let tokenized = crate::util::text::tokenize_chinese("记忆进化");
        let match_count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM turns_fts WHERE turns_fts MATCH ?1",
            [&tokenized], |r| r.get(0)
        ).unwrap();
        assert!(match_count > 0, "FTS MATCH 应命中");

        // keyword 命中
        let results = keyword_search(&db, "记忆进化");
        assert!(!results.is_empty(), "keyword '记忆进化' 必须命中");
    }

    // ──────────────────────────────────────────────────
    // 场景 4：覆盖写入后旧词不再命中
    // ──────────────────────────────────────────────────

    #[test]
    fn test_e2e_overwrite_old_keywords_no_longer_match() {
        let (db, tmp) = setup_db();
        let store = SessionStore::new(tmp.path(), &db);

        let header = make_header("e2e-overwrite");

        // 第一次保存
        let turns_v1 = vec![Turn {
            ts: "2026-04-11T22:00:00+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "我喜欢吃苹果".to_string(),
            metadata: None,
        }];
        store.save(&header, &turns_v1, None).unwrap();

        // 确认 "苹果" 能搜到
        let results = keyword_search(&db, "苹果");
        assert!(!results.is_empty(), "v1 后 '苹果' 应该能搜到");

        // 第二次保存（覆盖同一个 session_id）
        let turns_v2 = vec![Turn {
            ts: "2026-04-11T22:01:00+08:00".to_string(),
            seq: 1,
            role: "user".to_string(),
            content: "我喜欢吃香蕉".to_string(),
            metadata: None,
        }];
        store.save(&header, &turns_v2, None).unwrap();

        // "香蕉" 应该能搜到
        let results_new = keyword_search(&db, "香蕉");
        assert!(!results_new.is_empty(), "v2 后 '香蕉' 应该能搜到");

        // "苹果" 应该搜不到了（旧内容已被覆盖）
        let results_old = keyword_search(&db, "苹果");
        assert!(results_old.is_empty(), "v2 后 '苹果' 不应再命中，实际找到 {} 条", results_old.len());

        // turns 表只有 1 条
        let turn_count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = 'e2e-overwrite'",
            [], |r| r.get(0)
        ).unwrap();
        assert_eq!(turn_count, 1, "覆盖后 turns 表应只有 1 条");
    }

    // ──────────────────────────────────────────────────
    // 场景 5：删除 turn 后不再残留命中
    // ──────────────────────────────────────────────────

    #[test]
    fn test_e2e_delete_turn_no_residual_hits() {
        let (db, tmp) = setup_db();
        let store = SessionStore::new(tmp.path(), &db);

        let header = make_header("e2e-delete");

        let turns = vec![
            Turn {
                ts: "2026-04-11T22:00:00+08:00".to_string(),
                seq: 1,
                role: "user".to_string(),
                content: "这是一条会被删除的紫色大象记录".to_string(),
                metadata: None,
            },
            Turn {
                ts: "2026-04-11T22:00:05+08:00".to_string(),
                seq: 2,
                role: "assistant".to_string(),
                content: "好的，我记住了".to_string(),
                metadata: None,
            },
        ];
        store.save(&header, &turns, None).unwrap();

        // 确认 "紫色大象" 能搜到
        let results = keyword_search(&db, "紫色大象");
        assert!(!results.is_empty(), "save 后 '紫色大象' 应该能搜到");

        // 覆盖为只有第 2 条 turn
        let turns_after_delete = vec![Turn {
            ts: "2026-04-11T22:00:05+08:00".to_string(),
            seq: 2,
            role: "assistant".to_string(),
            content: "好的，我记住了".to_string(),
            metadata: None,
        }];
        store.save(&header, &turns_after_delete, None).unwrap();

        // "紫色大象" 应搜不到了
        let results_after = keyword_search(&db, "紫色大象");
        assert!(results_after.is_empty(), "删除后 '紫色大象' 不应再命中，实际找到 {} 条", results_after.len());

        // 但 "记住了" 应能搜到
        let results_remaining = keyword_search(&db, "记住了");
        assert!(!results_remaining.is_empty(), "保留的 turn '记住了' 应该能搜到");

        // turns 表只有 1 条
        let turn_count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = 'e2e-delete'",
            [], |r| r.get(0)
        ).unwrap();
        assert_eq!(turn_count, 1);
    }

    // ──────────────────────────────────────────────────
    // 场景 6：多次 save + rebuild 的完整生命周期
    // ──────────────────────────────────────────────────

    #[test]
    fn test_e2e_full_lifecycle_save_rebuild_search() {
        let (db, tmp) = setup_db();
        let store = SessionStore::new(tmp.path(), &db);

        // 写入 2 个 session
        store.save(&make_header("lifecycle-1"), &vec![
            Turn { ts: "2026-04-11T22:00:00+08:00".to_string(), seq: 1, role: "user".to_string(),
                   content: "异步编程在 Rust 中很重要".to_string(), metadata: None },
        ], None).unwrap();

        store.save(&make_header("lifecycle-2"), &vec![
            Turn { ts: "2026-04-11T22:01:00+08:00".to_string(), seq: 1, role: "user".to_string(),
                   content: "Tokio 是 Rust 的异步运行时".to_string(), metadata: None },
        ], None).unwrap();

        // save 后立即搜
        let kw1 = keyword_search(&db, "Rust 异步");
        assert!(!kw1.is_empty(), "save 后 keyword 应命中");

        // rebuild
        let stats = rebuild::rebuild_from_jsonl(tmp.path(), &db, None).unwrap();
        assert_eq!(stats.sessions_processed, 2);
        assert_eq!(stats.turns_indexed, 2);
        assert!(stats.errors.is_empty());

        // rebuild 后 keyword 仍然命中
        let kw2 = keyword_search(&db, "Rust 异步");
        assert!(!kw2.is_empty(), "rebuild 后 keyword 应命中");

        // rebuild 后 hybrid 也命中
        let hy2 = hybrid_search(&db, "Rust 异步");
        assert!(!hy2.is_empty(), "rebuild 名 hybrid 应命中");

        // 数据库一致性
        let turn_count: i64 = db.conn().query_row("SELECT COUNT(*) FROM turns", [], |r| r.get(0)).unwrap();
        let fts_count: i64 = db.conn().query_row("SELECT COUNT(*) FROM turns_fts", [], |r| r.get(0)).unwrap();
        assert_eq!(turn_count, 2);
        assert_eq!(fts_count, 2, "rebuild 后 turns_fts 应与 turns 数量一致");
    }
}
