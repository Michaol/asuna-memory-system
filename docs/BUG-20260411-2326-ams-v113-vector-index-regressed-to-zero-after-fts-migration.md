# BUG-20260411-2326-ams-v113-vector-index-regressed-to-zero-after-fts-migration.md

## 概要

在 `asuna-memory v1.1.3` 的完整回归测试中，确认：

> **FTS 迁移/修复看起来成功了，但向量索引发生严重回归：`0 个向量`。**

具体表现：
- `v1.1.3` 会自动把旧版 external-content FTS 迁移到 `content=''`
- 迁移后 CLI keyword / hybrid 正常
- 但 `vec_turns_rowids = 0`
- `doctor` 显示 `0 个向量`
- `semantic search` 返回 `0 条结果`
- `rebuild` 也只输出：`向量索引重建：已写入 0 条 int8 向量`

这说明：

> **1.1.3 修复了 FTS 侧问题，但把原本在 1.1.1/1.1.2 已经恢复的向量链路弄丢了。**

---

## 环境

- 版本：`asuna-memory 1.1.3`
- Profile：`default`
- 数据库：`/root/.asuna/profiles/default/memory.db`
- 测试库来源：从前序版本（已有 12 会话 / 34 轮 / 34 向量预期状态）升级

---

## 复现过程

### 1. 安装并运行 doctor
```bash
asuna-memory --version
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory doctor
```

输出：
```text
asuna-memory 1.1.3
完整性检查: OK
嵌入引擎状态: OK (Ready)
索引统计: 12 会话, 34 轮对话, 34 个向量
一致性: JSONL=12 vs DB=12 → OK
```

### 2. 运行常规 CLI 查询
执行：
- `search "做事必须完美" --mode keyword`
- `search "做事必须完美" --mode hybrid`
- `search "亚丝娜" --mode keyword`
- `search "记忆进化" --mode hybrid`
- `search "ownership borrowing" --mode semantic`

结果：
- keyword ✅
- hybrid ✅
- semantic 一开始表面上还能返回旧结果（详见下方“关键观察”）

### 3. 查看升级日志
命令执行期间输出了关键迁移日志：
```text
WARN  检测到旧版 external-content FTS 架构，正在自动迁移为 contentless...
INFO  向新架构自动恢复 FTS 索引...
```

### 4. 直接检查数据库
```sql
SELECT COUNT(*) FROM sessions;         -- 12
SELECT COUNT(*) FROM turns;            -- 34
SELECT COUNT(*) FROM turns_fts;        -- 34
SELECT COUNT(*) FROM vec_turns_rowids; -- 0
```

### 5. 检查新 FTS schema
```sql
SELECT sql FROM sqlite_master WHERE name='turns_fts';
```

结果：
```sql
CREATE VIRTUAL TABLE turns_fts USING fts5(
    preview,
    content='',
    content_rowid=id,
    tokenize='unicode61 remove_diacritics 2'
)
```

说明 FTS 确实被迁移到了 contentless 结构。

### 6. 运行 rebuild
```bash
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory rebuild
```

输出：
```text
FTS 索引重建：手动分词并索引 34 条记录
索引重建完成: 12 个会话, 34 轮对话, 0 个错误
向量索引重建：已写入 0 条 int8 向量
完成: 12 个会话, 34 轮对话, 0 个向量
```

### 7. 再次验证 doctor / semantic
```bash
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory doctor
LD_LIBRARY_PATH=/opt/rustrag:/usr/local/lib asuna-memory search "ownership borrowing" --mode semantic --top-k 5
```

结果：
```text
索引统计: 12 会话, 34 轮对话, 0 个向量
```

```text
搜索: "ownership borrowing" (mode=semantic)
共 0 条结果
```

---

## 关键观察

### 观察 1：FTS 修复成功，但向量完全消失
当前状态非常明确：
- `turns = 34`
- `turns_fts = 34`
- `vec_turns_rowids = 0`

也就是说：
- 文本层索引还在
- 向量层索引没了

### 观察 2：semantic 曾短暂返回旧结果，但最终稳定状态是 0 向量
测试早期，CLI semantic 曾返回历史结果；
但在迁移完成 / rebuild 完成后，最终系统稳定状态明确为：
- `0 个向量`
- semantic `0 条结果`

这说明存在以下可能：
1. 初始查询命中了迁移前残留状态 / 临时状态
2. 迁移或后续流程把原有 vector state 清空了
3. rebuild 的向量阶段没有从 turns 正确重建出 embedding

无论哪一种，**最终稳定结果都说明 1.1.3 向量链路失效。**

### 观察 3：不是模型不可用
`doctor` 明确显示：
```text
嵌入引擎状态: OK (Ready)
```

所以不是 ONNX / embedding engine 加载失败，而是：

> **有模型，但没把向量写进 `vec_turns`。**

---

## 与前一版本对比

### v1.1.1 / v1.1.2 已知状态
此前已验证过：
- `vec_turns_rowids = 34`（或对应非零）
- `doctor` 能显示非零向量数
- semantic search 可返回结果

### v1.1.3 当前状态
- `vec_turns_rowids = 0`
- `doctor = 0 个向量`
- semantic = 0 结果

所以这不是“历史遗留未修复”，而是：

> **1.1.3 新引入的明确回归。**

---

## 最可能的根因

### 方向 A：FTS 迁移过程误伤了向量恢复/向量元数据
1.1.3 明确新增了 FTS auto-migration：
- old external-content FTS → contentless FTS

可能在迁移时：
- 重建了文本索引
- 但没有同步保留 / 恢复 vector 索引
- 或迁移流程之后错误地把向量状态当作“待重建但未完成”

### 方向 B：rebuild 的向量阶段丢失了 turn → embedding 的输入来源
`rebuild` 明明扫描到了：
- `12 个会话`
- `34 轮对话`

但最终：
- `已写入 0 条 int8 向量`

这说明 rebuild vector 阶段很可能：
- 没有拿到要嵌入的文本
- 被某个过滤条件全部跳过
- 或因为 schema 变化导致 join / rowid 映射失效

### 方向 C：旧库升级场景下，向量恢复逻辑没有覆盖 migration 后状态
对新建库也许没问题，但对“已有旧库 → 自动迁移”的升级路径，vector rebuild 逻辑可能失配。

---

## 应检查的代码位置

建议重点检查：
- FTS migration 逻辑
- rebuild 中 vector rebuild 阶段
- `save_session` / `import` / `rebuild` 共用的向量写入路径
- `turns` → embedding 文本选取逻辑
- `vec_turns_rowids` / `vec_turns` 写入条件

尤其要核对：
1. FTS migration 后是否意外清空/失联 vector row mapping
2. rebuild 是否还能正确遍历所有 turn 并生成 embedding
3. `preview` / `content` / 分词后文本 / 原文 之间有没有字段来源变更导致 embedding 输入为空
4. rowid 映射是否仍和 `turns.id` 对齐

---

## 建议修法

### 修法 1：把 FTS migration 与 vector rebuild 解耦
FTS schema 迁移不应该影响向量层已有状态。
如果必须重建，也应明确：
- 先迁 FTS
- 再独立完整重建 vector
- 任一步失败要报错，不要静默变成 `0 个向量`

### 修法 2：给 rebuild vector 阶段加统计日志
建议输出：
- 扫描到多少 turns
- 多少 turns 有可嵌入文本
- 多少 turns 被跳过（以及原因）
- 最终成功写入多少 vectors

现在只看到 `已写入 0 条 int8 向量`，对定位不够。

### 修法 3：增加旧库升级回归测试
新增自动化测试：
1. 先构造 old external-content FTS schema 的数据库
2. 库中有 turns + vec_turns 非零
3. 升级到 1.1.3
4. 自动迁移后要求：
   - `turns_fts` 正常
   - `vec_turns_rowids` 仍非零
   - semantic search 仍可用

---

## 验收标准

修复后必须满足：

### 1. 旧库升级后
```text
doctor -> 12 会话, 34 轮对话, 34 个向量
```
至少向量数必须恢复到非零。

### 2. rebuild 后
```text
向量索引重建：已写入 34 条 int8 向量
```
而不是 `0 条`。

### 3. semantic search
```bash
asuna-memory search "ownership borrowing" --mode semantic --top-k 5
```
必须返回已知的 Rust memory safety 测试记录。

### 4. MCP semantic
`search_sessions(search_mode=semantic)` 也必须返回非空结果。

---

## 严重性

**HIGH**

原因：
- 这是搜索核心能力回归，不是次要显示问题
- v1.1.3 虽修复了 FTS migration 路径，却导致 semantic 能力失效
- 对用户来说等于“文本搜索好了，但语义搜索死了”

---

## 结论

`asuna-memory v1.1.3` 在旧库升级场景下：

> **成功把 FTS 从 external-content 迁移到 contentless，但向量索引回归为 0，semantic search 失效。**

这是一个明确的新回归，需要尽快修复。

---

*报告时间：2026-04-11 23:26 CST*  
*报告人：Asuna*