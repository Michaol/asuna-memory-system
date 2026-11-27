use super::ToolHandler;
use serde_json::{json, Value};

/// graph 工具共享的字段字符帽（entity/src/dst/from/to/rel 及类型字段），
/// 与 HTTP graph 端点的 entity 上限同口径；此前提取为 graph_assert 局部
/// 常量，neighbors/path/link_entity 三处手写 1000 易漂移。
const MAX_MCP_GRAPH_FIELD_CHARS: usize = 1000;

impl ToolHandler {
    fn check_graph_enabled(&self) -> Result<(), String> {
        if !self.config.graph.enabled {
            return Err("graph disabled in config".to_string());
        }
        Ok(())
    }

    pub(super) fn graph_assert(&self, args: &Value) -> Result<Value, String> {
        const MAX_MCP_TRIPLES_PER_BATCH: usize = 200;
        self.check_graph_enabled()?;
        let triples_value = args
            .get("triples")
            .ok_or_else(|| "missing triples".to_string())?;
        // N3: 先查条数再反序列化——16MiB 行上限下，超大数组会先被整体构造成
        // 数十万条 TripleInput 才拒（输入→内存放大）。非数组仍由 serde 报错。
        let triples_len = triples_value.as_array().map_or(0, Vec::len);
        if triples_len > MAX_MCP_TRIPLES_PER_BATCH {
            return Err(format!(
                "triples 数量过多（最多 {} 条）",
                MAX_MCP_TRIPLES_PER_BATCH
            ));
        }
        let triples: Vec<crate::graph::TripleInput> = serde_json::from_value(triples_value.clone())
            .map_err(|e| format!("invalid triples: {}", e))?;
        for (i, t) in triples.iter().enumerate() {
            if t.src.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS
                || t.rel.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS
                || t.dst.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS
                || t.src_type
                    .as_deref()
                    .is_some_and(|v| v.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS)
                || t.dst_type
                    .as_deref()
                    .is_some_and(|v| v.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS)
            {
                return Err(format!(
                    "triple[{}] 字段必须 <= {} 字符",
                    i, MAX_MCP_GRAPH_FIELD_CHARS
                ));
            }
            let mut fields: Vec<(&str, &str)> = vec![
                ("src", t.src.as_str()),
                ("rel", t.rel.as_str()),
                ("dst", t.dst.as_str()),
            ];
            if let Some(st) = t.src_type.as_deref() {
                fields.push(("src_type", st));
            }
            if let Some(dt) = t.dst_type.as_deref() {
                fields.push(("dst_type", dt));
            }
            if let Err(reason) = crate::growth::security::scan_fields(&fields) {
                return Err(format!("triple[{}] {}", i, reason));
            }
        }
        let stats = crate::graph::assert_triples(&self.db, &triples).map_err(|e| {
            tracing::warn!("graph_assert failed: {}", e);
            e.to_string()
        })?;
        Ok(json!({
            "status": "ok",
            "entities_created": stats.entities_created,
            "entities_updated": stats.entities_updated,
            "relations_created": stats.relations_created,
            "relations_updated": stats.relations_updated
        }))
    }

    pub(super) fn graph_neighbors(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let q: crate::graph::NeighborQuery =
            serde_json::from_value(args.clone()).map_err(|e| format!("invalid query: {}", e))?;
        if q.entity.trim().is_empty() {
            return Err("entity 不能为空".to_string());
        }
        if q.entity.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS {
            return Err(format!(
                "entity 过长（最多 {} 字符）",
                MAX_MCP_GRAPH_FIELD_CHARS
            ));
        }
        // M4: rel_type 过滤与 HTTP /graph/neighbors 同口径——1000 字符帽，
        // 用 nth(MAX) 做有界 O(MAX) 检查，不扫描任意长字符串。
        if q.rel_type
            .as_deref()
            .is_some_and(|rt| rt.chars().nth(MAX_MCP_GRAPH_FIELD_CHARS).is_some())
        {
            return Err(format!(
                "rel_type 过长（最多 {} 字符）",
                MAX_MCP_GRAPH_FIELD_CHARS
            ));
        }
        if !(1..=200).contains(&q.limit) {
            return Err(format!("limit must be in 1..=200, got {}", q.limit));
        }
        // M3: 与 REST /graph/neighbors 同契约——节点帽命中时结果是部分集，
        // 必须透出 truncated，否则 agent 把不完整邻居集当完整答案。
        let (neighbors, truncated) = crate::graph::query::neighbors_with_truncation(&self.db, &q)
            .map_err(|e| {
            tracing::warn!("graph_neighbors failed: {}", e);
            e.to_string()
        })?;
        let count = neighbors.len();
        Ok(json!({
            "status": "ok",
            "neighbors": neighbors,
            "count": count,
            "truncated": truncated
        }))
    }

    pub(super) fn graph_path(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let src = args["src"].as_str().ok_or("missing src")?;
        let dst = args["dst"].as_str().ok_or("missing dst")?;
        // M4: 空白/纯空白起点/终点在入口拒绝（与 graph_neighbors entity 同口径）。
        if src.trim().is_empty() || dst.trim().is_empty() {
            return Err("src/dst 不能为空".to_string());
        }
        if src.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS
            || dst.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS
        {
            return Err(format!(
                "src/dst 过长（最多 {} 字符）",
                MAX_MCP_GRAPH_FIELD_CHARS
            ));
        }
        let max_hops = match args.get("max_hops") {
            None | Some(Value::Null) => 5u32,
            Some(v) => {
                let raw = v
                    .as_u64()
                    .ok_or_else(|| "max_hops 必须为非负整数".to_string())?;
                u32::try_from(raw).map_err(|_| format!("max_hops 超出范围: {}", raw))?
            }
        };
        let result = crate::graph::path(&self.db, src, dst, max_hops).map_err(|e| {
            tracing::warn!("graph_path failed: {}", e);
            e.to_string()
        })?;
        Ok(json!({
            "status": "ok",
            "found": result.found,
            "length": result.length,
            "path": result.path,
            // M3: found:false + truncated:true 表示搜索被展开帽截断，
            // **不是**"不存在路径"。不透出会让 agent 把截断当事实。
            "truncated": result.truncated
        }))
    }

    pub(super) fn graph_link_entity(&self, args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let from = args["from"].as_str().ok_or("missing from")?;
        let to = args["to"].as_str().ok_or("missing to")?;
        if from.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS
            || to.chars().count() > MAX_MCP_GRAPH_FIELD_CHARS
        {
            return Err(format!(
                "from/to 过长（最多 {} 字符）",
                MAX_MCP_GRAPH_FIELD_CHARS
            ));
        }
        crate::growth::security::scan_fields(&[("from", from), ("to", to)])?;
        // C3: link_entity 会删除 from 行——old_canonical 必须在合并**前**解析，
        // 否则无损 session 键（session_Sprint-1）在行删除后精确匹配落空、回退到
        // 规范化形（session_sprint-1），报出错误的 old_canonical。探测错误按
        // W11/R6 口径直接上抛。
        let old_canonical = crate::graph::query::resolve_entity_canonical(self.db.conn(), from)
            .map_err(|e| e.to_string())?;
        let rewired = crate::graph::link_entity(&self.db, from, to).map_err(|e| {
            tracing::warn!("graph_link_entity failed: {}", e);
            e.to_string()
        })?;
        Ok(json!({
            "status": "ok",
            "edges_rewired": rewired,
            "old_canonical": old_canonical,
            "old_original_input": from
        }))
    }

    pub(super) fn graph_prune_dangling(&self, _args: &Value) -> Result<Value, String> {
        self.check_graph_enabled()?;
        let pruned = crate::graph::prune_dangling_refs(&self.db).map_err(|e| {
            tracing::warn!("graph_prune_dangling failed: {}", e);
            e.to_string()
        })?;
        Ok(json!({
            "status": "ok",
            "relations_pruned": pruned
        }))
    }

    fn session_turn_ids(&self, session_id: &str) -> Result<Vec<i64>, String> {
        let mut stmt = self
            .db
            .conn()
            .prepare("SELECT id FROM turns WHERE session_id = ?1 ORDER BY seq")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([session_id], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())
    }

    /// 计算本次 save_session 后的 graph_pending 字段
    pub(super) fn compute_graph_pending(&self, session_id: &str) -> Result<Option<Value>, String> {
        let turn_ids = self.session_turn_ids(session_id)?;
        if turn_ids.is_empty() {
            return Ok(None);
        }
        let pending =
            crate::graph::pending_turn_ids(&self.db, &turn_ids).map_err(|e| e.to_string())?;
        if pending.is_empty() {
            return Ok(None);
        }
        Ok(Some(json!({
            "turn_ids": pending,
            "hint": "These turns have no graph assertions yet. Call graph_assert with extracted triples (src, rel, dst) and source_turn=<id> to enable relationship queries."
        })))
    }
}
