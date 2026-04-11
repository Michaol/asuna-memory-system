# 给 AMS 开发者的工程改进建议

## 背景

这份建议不是泛泛而谈，而是基于 2026-04-11 对 AMS 多轮实测后总结出来的工程改进方向。目标不是“少出 bug”这种空话，而是帮助 AMS 变成一个**链路闭环、状态透明、升级可靠**的系统。

这次暴露的问题主要有三类：
1. 修复只覆盖了部分路径，没有覆盖完整用户链路
2. 数据库 schema、运行时行为、CLI/MCP 表现之间存在不一致
3. 系统缺少足够的可观测性，用户必须查库才能确认真实状态

---

## 一、按“完整链路”开发，而不是按“单点修复”开发

一个功能是否修好，不取决于某个函数是否返回正确结果，而取决于用户完整链路是否闭环。

以 AMS 为例，一个“搜索功能修复完成”的定义，至少应覆盖：
- `save_session` 实时写入路径
- `import` 批量导入路径
- `rebuild` 索引重建路径
- CLI `search`
- MCP `search_sessions`
- `doctor` 状态可观测
- 老数据升级迁移路径

如果只修其中一条路径，系统在真实使用时仍会表现为“看起来修了，实际上没完全修”。

### 建议
- 为每个核心能力维护一份 **capability checklist**，发布前逐项勾选
- 每次修复都问：
  - 实时路径修了吗？
  - rebuild 路径修了吗？
  - CLI 和 MCP 都统一了吗？
  - 旧数据升级验证了吗？

---

## 二、实时写入与 rebuild 必须共用同一套核心逻辑

很多索引/搜索系统的 bug，根本原因都不是“算法不对”，而是：

> 实时写入走一套逻辑，rebuild 走另一套逻辑。

这次 AMS 的中文搜索问题就是典型例子：如果 rebuild 绕过了分词逻辑，那么线上旧数据永远修不干净。

### 正确做法
把以下步骤抽成统一函数或统一 pipeline：
- 文本标准化
- 中文分词 / tokenizer 处理
- FTS 写入
- embedding 生成
- int8 量化
- vec 索引写入

#### 伪代码建议
```rust
fn index_turn(turn: &Turn, mode: IndexMode, tx: &Transaction) -> Result<()> {
    let normalized = normalize_text(&turn.preview);
    let tokens = tokenize_for_search(&normalized)?;
    write_fts(tx, turn.id, &tokens)?;

    if vector_enabled() {
        let embedding = embed_text(&normalized)?;
        let q = quantize_to_int8(&embedding)?;
        write_vec(tx, turn.id, &q)?;
    }

    Ok(())
}
```

然后：
- `save_session` 调 `index_turn`
- `import` 调 `index_turn`
- `rebuild` 遍历历史 turn 也调 `index_turn`

这样才能保证三个入口行为一致。

---

## 三、schema 不是摆设，数据库里有什么就要兑现什么

如果数据库里存在：
- `turns.embedding`
- `vec_turns USING vec0(embedding int8[384])`

那么用户自然会认为：
> 系统已经支持向量持久化，并且至少部分数据已被向量化。

如果实际上：
- `turns.embedding` 全是 `NULL`
- `vec_turns_*` 全为空

那就是设计与实现不一致。

### 建议
二选一，不能含糊：

#### 方案 A：真的支持持久化向量库
那就必须：
- 在 `save_session` / `import` / `rebuild` 路径里实际写入向量
- 保证 `turn_id` 和 `vec` rowid 映射可追踪
- `doctor` 明确显示向量索引统计

#### 方案 B：当前版本不支持持久化向量库
那就必须：
- 删除未使用的 vec schema
- 或至少在 `doctor` / 文档 / release note 中明确说明“当前仅运行时 embedding，不持久化 vec db”

不要让产品处于一种“好像支持，但其实没写进去”的中间态。

---

## 四、doctor 必须说真话

一个成熟系统，不该让用户通过打开 SQLite 数据库来确认系统真实状态。

### 当前 doctor 的不足
虽然它会显示：
- 数据完整性
- 模型加载状态
- 会话/轮次统计

但它没有告诉用户：
- FTS 到底索引了多少条
- vec 到底索引了多少条
- tokenizer 是什么
- rebuild 是否重建了向量索引
- 当前 hybrid/semantic 是否依赖持久化 vec

### 建议输出增加这些信息
```text
FTS index: 24 / 24
Tokenizer: tokenize_zh
Vector index: 0 / 24
Embedding persistence: disabled
Rebuild covers: FTS only
Hybrid source: FTS + runtime embedding
```

用户一眼就知道系统真实状态，调试效率会高很多。

---

## 五、测试要按“用户路径”设计

“40 个单元测试通过”不等于“产品已经可用”。

### 必须补的测试类型

#### 1. 升级回归测试
- 安装旧版本
- 导入旧数据
- 升级到新版本
- 运行 rebuild
- 验证 CLI / MCP / FTS / semantic / hybrid

#### 2. 端到端测试
不要只测 tokenizer 或某个 SQL，而是测：
- 导入一段中文对话
- rebuild
- CLI keyword 能搜到
- CLI hybrid 能搜到
- MCP keyword 能搜到
- MCP hybrid 能搜到

#### 3. 数据一致性测试
验证以下数量是否一致：
- `turns`
- `turns_fts`
- `vec_turns` / vec rowids

如果不一致，测试应直接失败。

#### 4. 可观测性测试
- `doctor` 是否正确展示 tokenizer
- `doctor` 是否正确展示 vector count
- `rebuild` 日志是否包含 FTS / vector rebuild 信息

---

## 六、发布说明要精确，不要模糊

好的 release note 不是“修复若干问题”，而是明确告诉用户：
- 修了什么
- 没修什么
- 升级后需要做什么

### 建议格式
```markdown
## Fixed
- 修复中文搜索在 rebuild 路径中绕过分词的问题
- 修复 MCP search_sessions 未统一中文分词逻辑的问题

## Changed
- rebuild 现在会重新写入 FTS 中文分词索引

## Known limitations
- 当前版本仍未持久化 int8 向量库（如果属实）

## Upgrade steps
- 升级后必须运行：`asuna-memory rebuild`
```

这会让用户和开发都减少误判。

---

## 七、把 bug report 当作系统设计反馈，不要当作“找茬”

一个优秀开发者看到 bug report，应该优先问：
- 这是不是暴露了我没观察到的状态缺口？
- 这是不是说明 capability 定义不完整？
- 这是不是说明测试只覆盖了 happy path？
- 这是不是 schema / 文档 / 行为之间不一致？

而不是先想“怎么证明自己没错”。

真正好的工程团队，会把 bug report 变成：
- 新测试
- 新可观测性
- 新 release 要求
- 新的设计约束

---

## 八、开发团队的落地清单

### 立即要做
1. 统一实时写入 / import / rebuild 的索引逻辑
2. 明确 vector 持久化是否真的启用
3. 在 doctor 里显示 FTS / vector 的真实统计
4. 给 rebuild 增加清晰日志
5. 增加中文 + 升级路径回归测试

### 接下来要做
1. 建立 capability checklist
2. 建立 schema / runtime / docs 一致性检查
3. 建立 release note 模板
4. 建立最小可观测性标准

---

## 结语

出色的开发，不是“从不出 bug”。

而是：
- 出 bug 后能快速定位真实问题
- 修复时能覆盖完整链路
- 能把一次 bug 变成系统架构和工程流程的进化

如果 AMS 真想成为可靠的记忆系统，就必须从“能跑”进化到“链路闭环、状态透明、升级可信”。

---

*整理人：Asuna*  
*时间：2026-04-11*