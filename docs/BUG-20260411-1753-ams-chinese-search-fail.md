# BUG-20260411-1753 - AMS 中文关键词搜索失效 (FTS5 unicode61 分词器限制)

## 1. 概要

在 AMS v1.0.1 中，**中文关键词搜索功能完全失效**。尽管 FTS5 索引已正确构建，数据已入库，但使用 `MATCH` 查询中文关键词时返回 0 条结果。

**影响范围**：所有使用中文的用户，无法通过 `search` 命令或 `search_sessions` MCP 工具检索中文对话内容。

---

## 2. 复现步骤

### 2.1 准备测试数据
```bash
# 导入一条包含中文的测试数据
cat > /tmp/test-zh.jsonl <<EOF
{"v":1,"type":"session_header","session_id":"test-zh-001","start_time":"2026-04-11T18:00:00+08:00","profile_id":"default","source":"bug-test","title":"中文搜索测试"}
{"ts":"2026-04-11T18:00:01+08:00","seq":1,"role":"assistant","content":"亚丝娜已经配置好 OpenClaw 的 MCP 集成，现在可以自动保存对话了。"}
EOF

asuna-memory import /tmp/test-zh.jsonl
```

### 2.2 执行搜索
```bash
# 尝试搜索 "亚丝娜"
asuna-memory search "亚丝娜" --mode keyword --top-k 5
```

### 2.3 预期结果
应该返回包含 "亚丝娜" 的那条记录。

### 2.4 实际结果
```text
搜索: "亚丝娜" (mode=keyword)
共 0 条结果
```

---

## 3. 根因分析

### 3.1 FTS5 表定义
通过检查 SQLite 数据库，确认 `turns_fts` 表的定义如下：

```sql
CREATE VIRTUAL TABLE turns_fts USING fts5(
  preview,
  content='turns',
  content_rowid='rowid',
  tokenize='unicode61'  -- 硬编码了 unicode61 分词器
)
```

### 3.2 `unicode61` 分词器限制
- `unicode61` 是 SQLite 自带的**简单分词器**，主要用于**英文**。
- 对中文，它**无法正确识别词边界**，通常将**每个汉字视为一个独立的词**，或者**无法建立有效的倒排索引**。
- 因此，`MATCH '亚丝娜'` 查询时，FTS5 无法匹配到任何记录。

### 3.3 验证
- **LIKE 查询有效**：`SELECT * FROM turns WHERE preview LIKE '%亚丝娜%';` 能正确返回结果。
- **FTS5 MATCH 查询无效**：`MATCH '亚丝娜'`、`MATCH '亚*'`、`MATCH '"亚丝娜"'` 均返回 0 条。
- **结论**：问题出在 **FTS5 分词器**，而非数据或索引本身。

---

## 4. 影响

1. **中文用户无法使用关键词搜索**：`search --mode keyword` 对中文无效。
2. **混合搜索退化为纯语义搜索**：`search --mode hybrid` 依赖 FTS5，若 FTS5 失效，则退化为纯语义搜索（而语义搜索对中文抽象概念的召回率本身就很低）。
3. **用户体验严重受损**：AMS 的核心功能之一是“全量记忆搜索”，中文搜索失效意味着**中文用户无法有效检索历史对话**。

---

## 5. 修复建议

### 方案 A：支持自定义分词器 (推荐)
- 在 AMS 配置中增加 `tokenize` 选项，允许用户指定分词器（如 `unicode61`, `porter`, 或自定义中文分词器）。
- 如果检测到中文内容，自动切换到支持中文的分词器（如 `jieba` 或 `icu`，如果 SQLite 支持）。

### 方案 B：硬编码中文分词支持
- 如果 AMS 主要面向中文用户，可以直接在代码中硬编码 `tokenize='icu'` 或 `tokenize='jieba'`（如果 SQLite 编译时支持）。
- 或者，在创建 FTS5 表时，根据语言自动选择分词器。

### 方案 C：绕过 FTS5，使用应用层搜索
- 如果 SQLite 无法支持中文分词，AMS 可以在应用层实现**简单的关键词匹配**（如 `LIKE` 查询），作为 FTS5 的 fallback。
- 虽然性能不如 FTS5，但能保证中文搜索可用。

### 方案 D：使用外部搜索引擎
- 对于大规模中文记忆检索，可以考虑集成 **Elasticsearch** 或 **Meilisearch**，它们对中文分词支持更好。
- 但这会增加系统复杂度，可能不适合轻量级场景。

---

## 6. 临时绕过方案 (用户侧)

在 AMS 修复之前，用户可以通过以下方式**手动检索中文内容**：

1. **使用 `list-sessions` + `export`**：
   ```bash
   asuna-memory list-sessions --last-days 30
   asuna-memory export <SESSION_ID>
   ```

2. **使用 Python 直接查询数据库**：
   ```python
   import sqlite3
   conn = sqlite3.connect("/root/.asuna/profiles/default/memory.db")
   cur = conn.cursor()
   cur.execute("SELECT session_id, preview FROM turns WHERE preview LIKE '%亚丝娜%';")
   for row in cur.fetchall():
       print(row)
   ```

3. **使用 `memory_read` + 应用层过滤**：
   - 调用 `memory_read --target memory` 读取所有记忆条目。
   - 在应用层用 Python 做关键词匹配。

---

## 7. 附件

- **测试数据**: `/tmp/test-zh.jsonl`
- **数据库路径**: `/root/.asuna/profiles/default/memory.db`
- **AMS 版本**: v1.0.1
- **SQLite 版本**: (需检查)

---

## 8. 状态

- **发现时间**: 2026-04-11 17:53
- **状态**: OPEN
- **严重性**: **CRITICAL** (核心功能失效，影响所有中文用户)

---

## 9. 补充说明

此 Bug 与之前修复的 `Missing Input: token_type_ids` 不同：
- **旧 Bug**: 引擎无法加载，完全不可用。
- **此 Bug**: 引擎能加载，但**搜索功能对中文失效**，属于**功能缺陷**。

建议 AMS 开发团队**优先修复**，或至少提供**明确的中文搜索限制说明**，避免用户误以为系统故障。

---

*报告人：亚丝娜 (Asuna)*
*通过详细检查 SQLite FTS5 表定义和查询行为确认根因。*
