use crate::embedder::quantize_to_int8;
use crate::index::db::Db;

/// 向量是否无效（空/全零/含非有限分量）：量化后必为全零 int8，无法参与
/// 有意义的 cosine 检索（W13）。
/// W2.4: pub(crate) 供批量路径（rebuild 向量批 / atom 回填）复用同款守卫。
pub(crate) fn is_invalid_vector(v: &[f32]) -> bool {
    v.iter().all(|x| *x == 0.0) || v.iter().any(|x| !x.is_finite())
}

/// 向量检索操作
pub struct VectorStore<'a> {
    db: &'a Db,
}

impl<'a> VectorStore<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 插入向量（关联到 turns 表的 rowid）
    ///
    /// W13: 零/非有限向量 skip+warn，**绝不返回 Err**——经 session_store
    /// save_overwrite 的 `?` 会回滚整次保存，违背 M1 数据优先契约。
    pub fn insert(&self, turn_id: i64, embedding: &[f32]) -> anyhow::Result<()> {
        if is_invalid_vector(embedding) {
            tracing::warn!(
                "turn_id={} 向量为零/非有限，跳过向量索引（会话数据照常落库）",
                turn_id
            );
            return Ok(());
        }
        let bytes = quantize_to_int8(embedding);

        // W28f: prepare_cached 摊销热循环（逐 turn 调用）的语句编译开销
        let mut stmt = self
            .db
            .conn()
            .prepare_cached("INSERT INTO vec_turns (rowid, embedding) VALUES (?1, vec_int8(?2))")?;
        stmt.execute(rusqlite::params![turn_id, bytes])?;

        Ok(())
    }

    /// 余弦相似度搜索
    pub fn search(&self, query_vec: &[f32], top_k: usize) -> anyhow::Result<Vec<(i64, f32)>> {
        if top_k == 0 {
            return Ok(Vec::new());
        }
        // W13: 无效查询向量（全零/非有限）量化为全零 int8，cosine 距离
        // NaN/NULL 在 ORDER BY distance ASC 排最前吃掉 LIMIT k，随后被
        // is_finite 过滤致结果截断/为空。直接短路返回空。
        if is_invalid_vector(query_vec) {
            return Ok(Vec::new());
        }
        let bytes = quantize_to_int8(query_vec);

        let mut stmt = self.db.conn().prepare_cached(
            "SELECT rowid, distance FROM vec_turns
             WHERE embedding MATCH vec_int8(?1)
             ORDER BY distance
             LIMIT ?2",
        )?;

        let limit = i64::try_from(top_k).unwrap_or(i64::MAX);
        let rows = stmt.query_map(rusqlite::params![bytes, limit], |row| {
            let rowid: i64 = row.get(0)?;
            let distance: Option<f32> = row.get(1)?;
            Ok((rowid, distance))
        })?;

        let mut results = Vec::new();
        for row in rows {
            let (rowid, distance) = row?;
            if let Some(d) = distance.filter(|d| d.is_finite()) {
                results.push((rowid, d));
            }
        }
        Ok(results)
    }

    /// 删除指定 turn 的向量（仅测试使用）
    #[cfg(test)]
    pub fn delete(&self, turn_id: i64) -> anyhow::Result<()> {
        self.db.conn().execute(
            "DELETE FROM vec_turns WHERE rowid = ?1",
            rusqlite::params![turn_id],
        )?;
        Ok(())
    }

    /// 清空所有向量（仅测试使用）
    #[cfg(test)]
    pub fn clear(&self) -> anyhow::Result<()> {
        self.db.conn().execute("DELETE FROM vec_turns", [])?;
        Ok(())
    }

    /// 获取向量总数（仅测试使用）
    #[cfg(test)]
    pub fn count(&self) -> anyhow::Result<i64> {
        let count: i64 =
            self.db
                .conn()
                .query_row("SELECT count(*) FROM vec_turns_rowids", [], |r| r.get(0))?;
        Ok(count)
    }
}

#[cfg(test)]
#[path = "vector_tests.rs"]
mod tests;
