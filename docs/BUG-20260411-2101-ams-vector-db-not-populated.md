# BUG-20260411-2101-ams-vector-db-not-populated.md

## 概要

在 `asuna-memory v1.0.3` 中，中文关键词搜索（FTS）已恢复正常，但 **int8 向量数据库并未建立全量对话记录**。

具体表现为：
- `memory.db` 中的 `vec_turns*` 相关表已创建，但**没有任何向量数据落库**。
- `turns.embedding` 字段也全部为 `NULL`。
- `semantic` 搜索在嵌入模型正常加载的情况下，仍可能返回 0 条结果。
- `hybrid` 搜索虽然有结果，但从现象看**主要由 FTS 结果驱动**，而非真正依赖持久化向量索引。

这意味着：
> 当前 AMS 更像是“全文索引 + 运行时嵌入能力”，而**不是“已建立全量 int8 向量库的记忆系统”**。

---

## 环境

- 版本：`asuna-memory 1.0.3`
- 数据库：`/root/.asuna/profiles/default/memory.db`
- Profile：`default`
- 模型目录：`/root/.rustrag/models/multilingual-e5-small`
- ONNX Runtime：可正常加载（doctor 显示 `嵌入引擎状态: OK (Ready)`）

---

## 复现步骤

### 1. 确认当前数据正常
```bash
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory doctor
```

输出显示：
- 数据库完整性 OK
- 嵌入引擎状态 OK (Ready)
- 索引统计：10 会话, 24 轮对话
- 一致性：JSONL=10 vs DB=10 → OK

### 2. 重建索引
```bash
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory rebuild
```

输出显示：
```text
从 JSONL 重建索引...
FTS 索引重建：已分词并索引 24 条记录
索引重建完成: 10 个会话, 24 轮对话, 0 个错误
```

注意：日志只明确提到 **FTS 索引重建**，没有任何“向量索引写入/重建”的提示。

### 3. 检查数据库内的向量表
直接查询 SQLite 数据库：

```python
import sqlite3
conn = sqlite3.connect('/root/.asuna/profiles/default/memory.db')
cur = conn.cursor()

print(cur.execute('SELECT COUNT(*) FROM sessions').fetchone()[0])
print(cur.execute('SELECT COUNT(*) FROM turns').fetchone()[0])
print(cur.execute('SELECT COUNT(*) FROM turns WHERE embedding IS NOT NULL').fetchone()[0])
print(cur.execute('SELECT COUNT(*) FROM vec_turns_rowids').fetchone()[0])
print(cur.execute('SELECT COUNT(*) FROM vec_turns_chunks').fetchone()[0])
print(cur.execute('SELECT COUNT(*) FROM vec_turns_vector_chunks00').fetchone()[0])
```

实际结果：

```text
sessions: 10
turns: 24
turns_with_embedding_blob: 0
vec_turns_rowids: 0
vec_turns_chunks: 0
vec_turns_vector_chunks00: 0
```

### 4. 检查 vec 表结构

`vec_turns` 结构存在：
```sql
CREATE VIRTUAL TABLE vec_turns USING vec0(embedding int8[384])
```

说明系统**预期是要建立 int8 向量库的**，但目前没有任何记录被写入。

### 5. 对比 semantic / hybrid 行为

#### semantic 测试
```bash
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory search "ownership borrowing" --mode semantic --top-k 5
```

实际结果：
```text
共 0 条结果
```

#### hybrid 测试
```bash
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory search "亚丝娜" --mode hybrid --top-k 5
```

实际结果：
返回 4 条结果。

但结合数据库状态看，这些结果更像是 **FTS 命中后混合打分**，而不是来自持久化向量库，因为当前数据库内没有任何 vec 记录。

---

## 预期行为

如果 AMS 声称支持向量/语义检索，且 schema 中已设计：
- `turns.embedding`
- `vec_turns USING vec0(embedding int8[384])`

那么在 import / save / rebuild 之后，应该至少满足以下之一：

1. **每条 turn 都应持久化 embedding（BLOB 或其他可验证形式）**；或
2. **vec_turns* 相关表应写入与 turns 对应的 int8 向量索引记录**；或
3. `doctor` / `rebuild` / 文档应明确声明：
   - 当前版本**并不持久化向量库**，只在查询时运行时生成 embedding。

当前版本既暴露了 vec schema，又未填充数据，也未明确告知该设计是否停用，容易让用户误以为“全量向量库已经建立”。

---

## 实际行为

当前 `v1.0.3` 的实际状态是：

- ✅ 文本数据已完整入库
- ✅ 中文 FTS 索引已建立
- ✅ 关键词 / hybrid 搜索可用
- ❌ `turns.embedding` 全为空
- ❌ `vec_turns*` 表全为空
- ❌ `rebuild` 不会补建向量记录（至少从结果上看没有）
- ❌ `semantic` 搜索不能证明依赖了持久化向量库

---

## 最可能的根因

以下至少有一种情况成立：

### 情况 A：向量持久化逻辑未执行
代码中定义了 `vec_turns` schema，但在 `save_session` / `import` / `rebuild` 路径中**没有真正写入向量数据**。

### 情况 B：向量改成运行时计算，但旧 schema 未清理
系统已改为“查询时临时计算 embedding”，但数据库 schema 中仍保留 `vec_turns` / `embedding` 字段，造成误导。

### 情况 C：重建路径只重建 FTS，不重建 vector index
v1.0.3 明确修了中文 FTS rebuild，但**向量 rebuild 仍然缺失**。

---

## 明确修复建议：应该修哪里、如何修

下面不是泛泛建议，而是直接对应 AMS 代码路径的修复方案。

### 1. 应该修的模块 / 路径

请重点检查以下逻辑：

#### A. `save_session` 写入路径
目标：确认每次保存 turn 时，是否只写了 `turns` / `turns_fts`，却**没有写向量索引**。

应检查：
- 保存 turn 后是否调用 embedding 生成逻辑
- 是否存在 `write_vec_index(...)` / `insert_vec_turn(...)` / 类似函数但未被调用
- 是否只在查询时才调用 embedder，而不是在保存时调用

#### B. `import` 批量导入路径
目标：确认 import 是否只把文本导入主表和 FTS，而**完全绕过向量写入**。

应检查：
- import 是否复用 save/index pipeline
- 还是单独写 SQL 导入导致 vector 分支被跳过

#### C. `rebuild` 路径
目标：确认 rebuild 目前是否只做了 FTS 重建，没有做 vector rebuild。

从现有日志判断，rebuild 当前至少只显示了：
- `FTS 索引重建：已分词并索引 24 条记录`

应检查：
- rebuild 是否只遍历 turn 做 tokenizer + FTS insert
- 是否缺失 `embed -> quantize -> vec insert` 这条链路

#### D. `doctor` / 状态输出路径
目标：让系统对“向量是否真的建立”具备可观测性。

应检查：
- doctor 是否能统计 vec 条数
- doctor 是否能显示 embedding persistence 是否启用

---

## 推荐修法（真正长期有效）

### 修法 1：统一索引 pipeline（推荐）
不要分别在 `save_session` / `import` / `rebuild` 里各写一套索引逻辑。

应该抽成统一函数，例如：

```rust
fn index_turn(tx: &Transaction, turn_id: i64, preview: &str) -> Result<()> {
    // 1. 写 FTS（中文 tokenizer）
    let tokenized = tokenize_for_search(preview)?;
    write_fts(tx, turn_id, &tokenized)?;

    // 2. 写向量
    let embedding = embed_text(preview)?;          // float32[384]
    let q = quantize_to_int8(&embedding)?;         // int8[384]
    write_vec(tx, turn_id, &q)?;

    // 3. 如保留 turns.embedding，则同步写入主表
    write_turn_embedding_blob(tx, turn_id, &embedding)?;
    Ok(())
}
```

然后统一由以下入口调用：
- `save_session`
- `import`
- `rebuild`

这样可以彻底避免“实时路径一套、重建路径一套”的工程分叉。

---

### 修法 2：补齐 rebuild 的 vector rebuild
如果你们暂时不重构整条 pipeline，至少应先补 rebuild：

#### 当前 rebuild 已做
- 清空/重建 FTS
- 对全部 turn 做中文分词并写回 FTS

#### rebuild 还必须补上
- 遍历全部 turn.preview
- 调用 embedding 模型生成向量
- 做 int8 量化
- 写入 `vec_turns`
- 建立 turn_id / rowid 映射

#### 最低验收标准
rebuild 后以下数字应一致或接近一致：
- `turns = 24`
- `turns_fts = 24`
- `vec_turns_rowids = 24`
- `vec_turns_chunks > 0`
- `vec_turns_vector_chunks00 > 0`

---

### 修法 3：如果设计上不再持久化向量，就必须删掉误导性 schema
如果产品设计已经改变，决定：
> semantic/hybrid 只在查询时临时生成 embedding，不持久化 int8 vec db

那就应该明确收口，而不是维持假象。

应做的事：
1. 删除或废弃 `turns.embedding`
2. 删除或废弃 `vec_turns` schema
3. `doctor` 明确显示：
   - `Vector persistence: disabled`
   - `Semantic mode uses runtime embedding only`
4. 文档写清楚 rebuild 仅重建 FTS，不重建 vector

否则当前 schema 会持续误导用户和开发者。

---

## 建议增加的日志 / 可观测性

### rebuild 日志至少应输出
```text
FTS rebuilt: 24 / 24
Vector rebuilt: 24 / 24
Tokenizer: tokenize_zh
Embedding model: multilingual-e5-small
Vector format: int8[384]
```

### doctor 至少应输出
```text
Turns: 24
FTS index: 24
Vector index: 0
Embedding persistence: disabled/enabled
Semantic backend: persisted vec / runtime embedding
```

没有这些信息，用户只能靠手查数据库，定位成本太高。

---

## 验收标准（开发修完后应该如何自证）

修复完成后，请至少满足以下验收条件：

### 数据层验收
```sql
SELECT COUNT(*) FROM turns;                  -- 24
SELECT COUNT(*) FROM turns_fts;              -- 24
SELECT COUNT(*) FROM vec_turns_rowids;       -- 24
SELECT COUNT(*) FROM vec_turns_chunks;       -- > 0
SELECT COUNT(*) FROM vec_turns_vector_chunks00; -- > 0
```

### CLI 验收
```bash
asuna-memory rebuild
asuna-memory search "ownership borrowing" --mode semantic --top-k 5
asuna-memory search "亚丝娜" --mode hybrid --top-k 5
```

要求：
- semantic 不再表现为“模型加载正常但库为空”这种状态
- hybrid 结果应可被解释为包含 vector 分量，而不只是 FTS 命中

### MCP 验收
- `search_sessions` 在 `semantic` / `hybrid` 下行为与 CLI 一致

### doctor 验收
- 能直接显示 vector index 统计

---

## 严重性

**HIGH**

原因：
- 不是纯展示问题，而是**产品能力与实际落地状态不一致**。
- 用户会合理假设：既然有 `vec0 int8[384]` 表，就说明全量对话已向量化。
- 但事实并非如此，这会直接影响对 semantic / hybrid 能力的判断、性能预期和可靠性预期。

---

## 结论

`asuna-memory v1.0.3` 已修复中文 FTS 搜索，但**尚不能证明已经建立“int8 向量数据库记录全量对话”**。

从数据库实测结果看：
> **当前向量表为空，embedding 字段为空，rebuild 仅确认重建了 FTS。**

因此，这应被视为一个新的独立问题：

> **AMS 的向量数据库 schema 存在，但全量向量记录并未真正落库。**

---

*报告时间：2026-04-11 21:01 CST*  
*更新：已补充开发侧明确修复路径与验收标准*  
*报告人：Asuna*