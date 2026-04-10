use crate::index::db::Db;
use crate::embedder::onnx::quantize_to_int8;

/// 向量检索操作
pub struct VectorStore<'a> {
    db: &'a Db,
}

impl<'a> VectorStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 插入向量（关联到 turns 表的 rowid）
    pub fn insert(&self, turn_id: i64, embedding: &[f32]) -> anyhow::Result<()> {
        let bytes = quantize_to_int8(embedding);

        self.db.conn().execute(
            "INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))",
            rusqlite::params![turn_id, bytes],
        )?;

        Ok(())
    }

    /// 余弦相似度搜索
    pub fn search(&self, query_vec: &[f32], top_k: usize) -> anyhow::Result<Vec<(i64, f32)>> {
        let bytes = quantize_to_int8(query_vec);

        let mut stmt = self.db.conn().prepare(
            "SELECT rowid, distance FROM vec_turns
             WHERE embedding MATCH vec_int8(?1)
             ORDER BY distance
             LIMIT ?2"
        )?;

        let rows = stmt.query_map(rusqlite::params![bytes, top_k as i64], |row| {
            let rowid: i64 = row.get(0)?;
            let distance: f32 = row.get(1)?;
            Ok((rowid, distance))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// 删除指定 turn 的向量
    pub fn delete(&self, turn_id: i64) -> anyhow::Result<()> {
        self.db.conn().execute(
            "DELETE FROM vec_turns WHERE rowid = ?1",
            rusqlite::params![turn_id],
        )?;
        Ok(())
    }

    /// 清空所有向量
    pub fn clear(&self) -> anyhow::Result<()> {
        self.db.conn().execute("DELETE FROM vec_turns", [])?;
        Ok(())
    }

    /// 获取向量总数
    pub fn count(&self) -> anyhow::Result<i64> {
        let count: i64 = self.db.conn().query_row(
            "SELECT count(*) FROM vec_turns",
            [],
            |r| r.get(0),
        )?;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成一个 384 维的测试向量（只有一个非零分量）
    fn make_test_vec(val: f32) -> Vec<f32> {
        let mut v = vec![0.0f32; 384];
        v[0] = val;
        v
    }

    #[test]
    fn test_vector_insert_and_search() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // 先插入 turn 记录
        db.conn().execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('test', 0, 'test.jsonl', 0, 0)",
            [],
        ).unwrap();

        db.conn().execute(
            "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
             VALUES ('test', 1, 0, 'user', 'hello')",
            [],
        ).unwrap();

        let turn_id = db.conn().last_insert_rowid();

        let store = VectorStore::new(&db);

        // 插入向量
        let vec1 = make_test_vec(1.0);
        store.insert(turn_id, &vec1).unwrap();
        assert_eq!(store.count().unwrap(), 1);

        // 搜索（相同向量应得 distance 0）
        let results = store.search(&vec1, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, turn_id);
        assert!(results[0].1 < 0.01, "distance should be near 0, got {}", results[0].1);

        // 删除
        store.delete(turn_id).unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }

    #[test]
    fn test_vector_clear() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        db.conn().execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('test', 0, 'test.jsonl', 0, 0)",
            [],
        ).unwrap();

        for i in 0..3 {
            db.conn().execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('test', ?1, 0, 'user', 'test')",
                rusqlite::params![i + 1],
            ).unwrap();
            let id = db.conn().last_insert_rowid();
            let store = VectorStore::new(&db);
            store.insert(id, &make_test_vec(i as f32)).unwrap();
        }

        let store = VectorStore::new(&db);
        assert_eq!(store.count().unwrap(), 3);
        store.clear().unwrap();
        assert_eq!(store.count().unwrap(), 0);
    }
}
