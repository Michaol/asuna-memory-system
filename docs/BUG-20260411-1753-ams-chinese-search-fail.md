# BUG-20260411-1753 - AMS 中文关键词搜索失效 (FTS5 unicode61 分词器限制)

## 1. 概要

在 AMS v1.0.1 / v1.0.2 中，**中文关键词搜索功能完全失效**。尽管 FTS5 索引已正确构建，数据已入库，但使用 `MATCH` 查询中文关键词时返回 0 条结果。

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
    content=turns,
    content_rowid=id,
    tokenize='unicode61 remove_diacritics 2'
)
```

### 3.2 `unicode61` 分词器限制
- `unicode61` 是 SQLite 自带的通用 tokenizer，更适合英文/拉丁文本。
- 对中文，它**不能正确处理词边界**，导致 `MATCH '亚丝娜'` 无法命中包含完整词语“亚丝娜”的记录。
- 这不是“索引没建成”，而是**索引建成了，但按错误的分词方式建立**。

### 3.3 直接验证结果
已确认以下事实：

- **FTS 索引存在**：`turns_fts` 表已创建。
- **FTS 索引有数据**：记录数与 `turns` 表轮次一致（24 条）。
- **LIKE 能找到中文**：`SELECT ... WHERE preview LIKE '%亚丝娜%'` 可返回结果。
- **MATCH 找不到中文**：`MATCH '亚丝娜'`、`MATCH '记忆进化'` 返回 0 条。

**结论**：问题根因是 **FTS5 tokenizer 选型错误**，不是数据丢失、索引缺失、导入失败，亦不是 CLI 参数问题。

---

## 4. 影响

1. **中文用户无法使用关键词搜索**：`search --mode keyword` 对中文无效。
2. **混合搜索中的关键词部分失效**：`search --mode hybrid` 对中文场景无法发挥完整能力。
3. **产品承诺受损**：AMS 的核心定位是“全量记忆搜索”，但中文数据实际上不可检索，影响核心价值。
4. **问题具持续性**：v1.0.2 已验证仍未修复，说明该缺陷存在于当前 release 流程中。

---

## 5. 修复建议（一次性、长期修复）

### 推荐方案：原生集成 ICU 中文分词，替换 `unicode61`

**问题根源**：当前 FTS5 虚表硬编码使用 `tokenize='unicode61 remove_diacritics 2'`。`unicode61` 适合英文/拉丁文本，但**不能正确处理中文词边界**，导致中文关键词无法通过 `MATCH` 命中。

**长期修复目标**：让 AMS 在数据库索引层原生支持中文分词，而不是在应用层做临时降级或绕过。

### 5.1 代码层修改
在创建 FTS5 索引表的位置（如 `src/index.rs` 或等价模块）修改建表语句：

**当前实现（问题版本）**
```sql
CREATE VIRTUAL TABLE turns_fts USING fts5(
  preview,
  content=turns,
  content_rowid=id,
  tokenize='unicode61 remove_diacritics 2'
)
```

**建议修改为**
```sql
CREATE VIRTUAL TABLE turns_fts USING fts5(
  preview,
  content=turns,
  content_rowid=id,
  tokenize='icu'
)
```

如果运行时/构建系统允许，也可以进一步显式指定 CJK 友好的 ICU 分词配置；但最关键的是：**必须移除 `unicode61`，改用支持中文断词的 tokenizer**。

### 5.2 Rust / SQLite 依赖层修改
为保证 release 二进制在目标环境中稳定工作，建议在构建系统中：

1. 确保 SQLite 构建包含 **FTS5 + ICU** 支持。
2. 如果当前通过 `rusqlite` / `libsqlite3-sys` 链接 SQLite：
   - 使用带 `bundled` 的构建方式，避免依赖目标机系统 SQLite 能力不一致。
   - 显式启用 ICU 支持，确保生成的 release 包在用户环境中开箱即用。
3. CI/CD 中加入 ICU 依赖安装与静态/一致性链接验证。

**目标**：不要依赖用户机器“刚好安装了支持 ICU 的 SQLite”。中文搜索能力必须随 release 一起交付。

### 5.3 索引迁移方案（必须）
仅修改代码还不够；**现有用户数据库中的 FTS 索引必须重建**。

建议在升级后执行以下迁移逻辑：

1. 删除旧的 `turns_fts` 相关表。
2. 使用新的 `tokenize='icu'` 重新创建 FTS5 虚表。
3. 从 `turns` 主表重新回填全部 `preview` 数据。
4. 更新 `rebuild` / `rebuild_index` 命令，使其明确重建的是“新分词器索引”。

建议在代码中增加“索引版本”或“tokenizer 元信息”检查：
- 若检测到现存索引由 `unicode61` 创建，则启动后提示必须 rebuild；
- 或在升级时自动触发一次安全重建。

### 5.4 测试要求（必须补齐）
当前问题之所以漏进 release，说明测试覆盖不足。建议补充以下自动化测试：

#### 单元/集成测试样例
1. 导入包含以下内容的中文会话：
   - `亚丝娜已经配置好 OpenClaw 的 MCP 集成。`
   - `记忆进化启动：自主判断与提炼。`
2. 验证以下查询必须返回非零结果：
   - `search "亚丝娜" --mode keyword`
   - `search "记忆进化" --mode keyword`
   - `search "自主判断" --mode keyword`
3. 验证 `search_sessions` MCP 工具在 `keyword` / `hybrid` 下也能返回结果。
4. 验证 rebuild 后结果一致。

#### 回归测试目标
- 中文关键词搜索可用
- 混合搜索中的关键词部分对中文有效
- 英文搜索能力不退化
- rebuild 后旧库迁移成功

### 5.5 发布要求
该修复不应只停留在源码层，必须覆盖到**release 二进制分发链路**：

1. 更新 release 构建流程，确保产物内置支持 ICU 的 SQLite。
2. 在 release note 中明确说明：
   - 已修复中文关键词搜索
   - 升级后需要执行一次 `asuna-memory rebuild`
3. 对 Linux/macOS/Windows 三平台分别验证中文搜索，不可只在开发机通过。

### 5.6 为什么这是唯一正确的长期方案
- **根因在索引层**，不是 UI 层、CLI 层，也不是查询参数层。
- 只要底层 tokenizer 继续使用 `unicode61`，中文 `MATCH` 就会持续失效。
- 应用层 LIKE/filter 只能算绕过，不具备长期可维护性，也无法保留 FTS 的性能和搜索能力。
- 使用 ICU 是对 SQLite/FTS5 体系下最自然、最长期的修复路径。

---

## 6. 附件

- **测试数据**: `/tmp/test-zh.jsonl`
- **数据库路径**: `/root/.asuna/profiles/default/memory.db`
- **AMS 版本**: v1.0.1, v1.0.2
- **实际 FTS 定义**:
  ```sql
  CREATE VIRTUAL TABLE turns_fts USING fts5(
      preview,
      content=turns,
      content_rowid=id,
      tokenize='unicode61 remove_diacritics 2'
  )
  ```

---

## 7. 状态

- **发现时间**: 2026-04-11 17:53
- **最后验证时间**: 2026-04-11 18:44
- **当前版本**: v1.0.2
- **状态**: **OPEN (未修复)**
- **严重性**: **CRITICAL** (核心功能失效，影响所有中文用户)

### 版本验证记录

| 版本 | 时间 | 关键词搜索 (亚丝娜) | 混合搜索 (记忆进化) | 状态 |
|------|------|---------------------|---------------------|------|
| v1.0.1 | 2026-04-11 17:52 | ❌ 0 条 | ❌ 0 条 | 未修复 |
| v1.0.2 | 2026-04-11 18:44 | ❌ 0 条 | ❌ 0 条 | 未修复 |

**结论**：v1.0.2 仍未修复中文搜索问题。当前 release 虽然能建索引、能查英文、能加载嵌入模型，但中文关键词检索在索引层面仍然不可用。

---

## 8. 补充说明

此 Bug 与之前修复的 `Missing Input: token_type_ids` 不同：
- **旧 Bug**: 引擎无法加载，完全不可用。
- **此 Bug**: 引擎能加载，但**搜索功能对中文失效**，属于**索引层设计缺陷**。

建议 AMS 开发团队将此问题视为**索引架构修复**，而不是简单的查询兼容问题。只要底层继续使用 `unicode61`，中文用户的关键词检索能力就不会真正恢复。

本报告仅保留**长期修复方案**：
- 原生引入 ICU tokenizer
- 重建 FTS 索引
- 修正 release 构建链路
- 补齐中文回归测试

不再建议任何应用层临时绕过，因为那不是一次性、长久修复。

---

*报告人：亚丝娜 (Asuna)*
*通过详细检查 SQLite FTS5 表定义、索引记录数、LIKE/MATCH 行为差异确认根因。*
