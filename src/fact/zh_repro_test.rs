use crate::fact::search::{search_sessions, SearchMode, SearchParams};
use crate::index::db::Db;

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_zh_data(db: &Db) {
        db.conn()
            .execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('test-zh-001', 0, 'test-zh.jsonl', 0, 0)",
                [],
            )
            .unwrap();

        db.conn().execute(
            "INSERT INTO turns (id, session_id, seq, timestamp_ms, role, preview) VALUES
             (100, 'test-zh-001', 1, 1000, 'assistant', '亚丝娜已经配置好 OpenClaw 的 MCP 集成，现在可以自动保存对话了。')",
            []
        ).unwrap();

        // 手动模拟触发器行为：在存入 FTS 前调用应用层分词逻辑
        let tokenized_zh = crate::util::text::tokenize_chinese(
            "亚丝娜已经配置好 OpenClaw 的 MCP 集成，现在可以自动保存对话了。",
        );
        db.conn()
            .execute(
                "INSERT INTO turns_fts(rowid, preview) VALUES (100, ?1)",
                [tokenized_zh],
            )
            .unwrap();
    }

    #[test]
    fn test_chinese_keyword_search_verification() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        setup_zh_data(&db);

        let params = SearchParams {
            query: "亚丝娜".to_string(),
            search_mode: SearchMode::Keyword,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };

        let results = search_sessions(&db, None, &params).unwrap();

        println!("Search for '亚丝娜' found {} results", results.len());
        assert!(
            results.iter().any(|r| r.preview.contains("亚丝娜")),
            "FTS5 should now find Chinese words after fix"
        );
    }
}
