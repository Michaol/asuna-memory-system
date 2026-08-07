# Asuna Memory System — 历史版本变更日志

本文档包含当前版本之前所有版本的升级指南和变更日志。

最新版本请参阅 [README_ZH.md](README_ZH.md)。

---

### 从 v2.6.1 升级到 v2.6.2

v2.6.2 修 v2.6.1 L2 场景聚合特性在带 LLM 的 agent 实测中发现的问题。零新依赖，二进制体积不变，无需数据迁移。

升级步骤：替换二进制。无需改配置——DashScope 用户的 `batch_size` 会自动钳到 provider 的 10 条上限。

**v2.6.2 变更摘要：**

🔴 **修复：scenario 行误报 MEMORY.md 发散**

- scenario 行（`memory_type='scenario'`，L2 聚合写入）存在自己的 `scenarios/` 目录，不进 MEMORY.md，但 `reconcile_check` / `rebuild_md_from_db` / `sync_atoms_to_md` 对比/重建时遍历了所有 `target='memory'` 行 → `doctor` 报 `DIVERGED (.md=N, db=N+1)`，`--fix` 还会把 scenario 摘要塞进 MEMORY.md。
- 修复：三处排除 `memory_type='scenario'`（`COALESCE(memory_type,'manual') != 'scenario'`）；对 `user` target 是 no-op。`doctor --split-entries` 也不再切碎含零散 `§` 的 scenario 摘要。
- 测试：`test_reconcile_excludes_scenario_rows` + sync 路径覆盖。

🔴 **修复：scenario 字符不再计入 MEMORY.md 容量预算**

- `sync_atoms_to_md` 把 scenario 字符计入受保护的 `manual_chars`，尽管它们从不进 MEMORY.md。`max_scenarios=50` + ~200-400 字符摘要时，scenario 字符（~10-20k）可超过 2200 预算，每次 sync 驱逐所有 atom。
- 修复：scenario 从预算中排除（`NOT IN ('atom','scenario')`）。
- 测试：`test_scenario_chars_dont_evict_atoms`。

🔴 **修复：L2 嵌入批次不再超 DashScope 10 条/请求上限**

- `reembed_for_clustering` 一次把本会话全部已存 atom 传给 `embed_documents`；DashScope 超过 10 条返回 HTTP 400，L2 聚合被静默杀死。
- 修复：`embed_documents` 按 `batch_size` 分块（保序）；`EmbeddingConfig::resolve_env` 在格式为 DashScope 时钳 `batch_size`≤10（同时修 embed 分块 + rebuild/DB 回填分块——默认 `batch_size=32` 的用户也受益）。
- 测试：`test_dashscope_batch_size_clamped_to_provider_cap`。

🔵 **代码质量**

- 217 个测试通过（新增 5 个）。SonarCloud 0 issues，质量门 OK。

---

### 从 v2.6.0 升级到 v2.6.1

v2.6.1 修两个 v2.6.0 后发现的问题：`doctor --fix` 无法清理残缺 § 分隔符行的 bug（由 Hermes agent 运维者反馈），以及沉睡已久的 L2 场景聚合层（代码存在但从未接入 pipeline）。零新依赖，二进制体积不变，无需数据迁移。

升级步骤：替换二进制。L2 场景聚合是**可选**——在 config.json 设 `scenarios.enabled = true` 开启（需要 LLM + embedder，默认关）。

**行为变化**：开启 `scenarios.enabled` 后，会话后管线会把本会话 atom 聚类 + LLM 摘要写成 `memory_type='scenario'` 行，`/recall` L2 会读到。scenario 行不参与 atom 容量淘汰（作为未来工作跟踪）。

**v2.6.1 变更摘要：**

🔴 **修复：`doctor --fix` 无法清理残缺 § 行（split_multi_entry_rows）**

- **根因**：`split_multi_entry_rows` 用 `content LIKE '%\n§\n%'`（仅完整分隔符）检测坏行、用 `split("\n§\n")` 拆分。含残缺分隔符的行——尾部 `\n§`（如 `"...。\n§`）或头部 `§\n`（如 `"§\nAMS..."`）——永远不匹配，`doctor --fix` 报 0 坏行成功，脏行留着让 MEMORY.md 永久分歧。
- **修复**：
  - 检测扩到三种分隔形态：完整 `\n§\n`、尾部 `\n§`（content 末尾）、头部 `§\n`（content 开头）。合法 mid-content §（如 `"see §5 of the statute"`）不边界邻接，不会被误标记。
  - 新增 `normalize_separators`：§ 当且仅当两侧都边界邻接（start 或前接 `\n`）且（end 或后接 `\n`）才视为分隔符，规范化为 `\n§\n`（复用已有换行、只在字符串边界缺失处补）。合法 mid-content § 原样保留。
  - 删除守卫：原行仅当 split 真分解或清理了（`sub_entries.len() > 1` 或单 sub 与原 trim 不同）才 deref+delete；no-op split（如误标记的合法 mid-§ 行）不删——防数据丢失。
- 测试：`test_normalize_separators`（6 形态）、`test_split_truncated_separators`（真 DB 上种入尾/头/全/mid-§，断言清理 + mid-§ 保留 + 无残留残缺行）。

🟢 **特性：L2 场景聚合接入 pipeline**

- `ScenarioAggregator`（`src/memory/scenario.rs`）有完整的聚类 + LLM 摘要逻辑但**零调用者**；recall L2 读 `bounded_memory WHERE memory_type='scenario'` 但该表 0 行（死层）。
- 现经 `src/transport/pipeline.rs` 的 `run_l2_aggregation` 接入：L1 抽取 + 图集成后，若 `config.scenarios.enabled`，重取本会话已存 atom 的 (id, content)、重嵌入（滤零向量）、按 cosine > threshold 聚类、对每个 ≥ min_cluster_size 的簇 LLM 生成摘要、写成 `memory_type='scenario'` 行（`/recall` L2 可读）+ `memory/scenarios/` 下 .md 文件。
- 锁纪律同 `run_pipeline`：DB 锁内读 → 释放做 embed+LLM → 重取 DB 锁写。best-effort：失败仅 log 不阻断。
- 配置：`scenarios: { enabled: false, similarity_threshold: 0.8, min_cluster_size: 2, max_scenarios: 50 }`（opt-in，`#[serde(default)]`）。
- scenario 行绕过 atom 容量预算（atom 淘汰只针对 `memory_type='atom'`），故 L2 写入自带上限：超过 `max_scenarios` 的最老 `memory_type='scenario'` 行被淘汰，且摘要内容去重（防跨会话近重复淹没 L2 recall）。会重嵌入本会话 atom 来聚类——默认本地 ONNX 下是廉价 CPU，但 HTTP API（OpenAI/DashScope）后端会粗略翻倍每会话嵌入成本；`store_atoms` 返回 embedding 以避免重嵌入的重构列为 future work。
- 测试：`test_recall_l2_surfaces_scenario_rows`（scenario 行 → recall L2 返回）、`test_scenario_cap_evicts_oldest`（cap-淘汰 SQL）。

🔵 **代码质量**

- 214 个测试通过（新增 4 个），1 个 ignored（基准）。Clippy：1 个重构副产物警告已修（split 守卫 `map_or` → `is_none_or`）；6 个存量警告在未触碰文件中不变。

---

### 从 v2.5.3 升级到 v2.6.0

v2.6.0 是首个"轻量红利包"版本——检索可用性特性 + 治理地基，设计参考了 Hindsight 记忆引擎的源码级研究。零新依赖，二进制维持 ~16MB，无需手动迁移（旧库首次启动自动升级，见下）。

升级步骤：替换二进制。旧库首次启动自动获得 `edited_at` 列（无注释 ALTER，容忍重复列）与 `memory_history` 表（`CREATE IF NOT EXISTS`）。升级后运行 `asuna-memory doctor` 确认健康。

**注意行为变化**：v2.5.3 对 `/recall` 响应**不做**任何 token 预算；v2.6.0 起执行 `recall.token_budget`（默认 2000，可用请求参数 `max_tokens` 覆盖）。超大响应将返回更少的 memories 并带 `truncated: true`。依赖无上限 recall 载荷的客户端请显式调高 `max_tokens`。

**v2.6.0 变更摘要：**

🟢 **特性：`/recall` 响应 token 预算**

- 按层序（L3→L2→L1→L0）greedy prefix cut：第一个超出剩余预算的 memory 整条丢弃——不截断半条、不回填（对齐 Hindsight `_filter_by_token_budget`）。默认取配置 `recall.token_budget`（2000），请求可用 `max_tokens` 覆盖，响应带 `truncated` 标志
- token 估算为轻量字符法（CJK/全角/假名/谚文 ≈ 1 token/字，其余 ≈ 3 字符/token）——不引入外部分词器依赖。预算只计 memory 正文，JSON 框架不计（文档化的近似）
- 无丢弃时 `context` 与 v2.5.3 逐字节一致

🟢 **特性：`/recall` 显式时间范围过滤**

- `after` / `before`（RFC3339）与 `last_days`，语义与 `/search`（v2.5.1）完全一致：畸形值返回 400 而非静默放宽窗口；`last_days` clamp 到 [0, 36500] 并覆盖 `after`
- 谓词进 LIMIT 之前（无 post-filter 欠返）：L1 过滤 `bounded_memory.created_at`（recorded-at 语义，与 `timestamp_ms` 平行——有意选择：`update()` 只 bump `updated_at`）；L0 过滤 `turns.timestamp_ms`（走 `idx_turns_ts`）
- L3 persona 与 L2 scenarios 设计上不过滤（常青层）。L1 条目附加 `created_at`（epoch ms）——L1 prepare 语句形状有变（多一个投影），其余行为不变
- 对照说明：Hindsight 的 recall 没有显式时间参数（只有锚点 + 自然语言解析）——此为 AMS 自有面，非借鉴项

🟢 **特性：分数透明**

- `/search` 结果附加 `scores: {semantic, keyword}`——各来源分量精确加和为 `score`（hybrid 为 RRF 贡献；keyword/semantic 模式为单一分量）。排序可检查（对齐 Hindsight `RecallScores`），并采纳其经验：不设绝对分数阈值（分数未校准）
- `/recall` L1 条目附加 `ordered_by: "confidence+recency"`——L1 无数值分数，暴露排序依据

🔵 **治理：精确文本守卫 + `duplicate_skip` 审计**

- 入库路径在 embed/admission 之前：trim 后内容与任一现有 `bounded_memory` 行精确匹配即跳过，并写审计（`action='duplicate_skip'`）。此前此类复述被静默丢弃（无 embedder 时根本不拦截）——现在经 `audit_log` 可检查
- 作用域有意覆盖全部 target：与 persona 行或 manual 条目相同的 atom 同样跳过（防 MEMORY.md 双条目、防陈旧内容再 supersede）。守卫集在循环内更新，批内逐字重复同样被拦。审计失败仅 warn 不中断批次（对齐驱逐先例）。注：`audit_log` 尚无保留策略——`duplicate_skip` 行按 O(稳定事实数 × 会话数) 增长，清理策略作为 v2.6.x 后续项跟踪。

🔵 **治理：`edited_at` 用户手改保护标记**

- 新增可空列 `bounded_memory.edited_at`。由 `memory_update` 与 `doctor --fix` 回插 .md-only 条目时打点；`doctor --split-entries` 拆分子行继承（修复审查发现的丢标记漏洞）。契约：未来任何自动改写机制必须跳过 `edited_at` 非空的行。程序化写入（atom 抽取、`memory_write`）保持 NULL
- 迁移采用无注释的 `MIGRATION_P8_ALTER_SQL` 风格，并配模拟 v2.5.3 库的升级回归测试（防已知的"注释前缀 ALTER 被运行器跳过"行为）

🔵 **地基：`memory_history` 快照表**

- `(source_table, source_id, content_snapshot, changed_by, changed_at)` + `(source_table, source_id)` 索引。v2.6.0 无写入方（inert）；为未来整合引擎预留改写安全网，`rebuild --full` 不清空。`source_id` 有意不设外键（源行会被驱逐，软引用先例）

🔵 **质量：检索回归基准测试**

- `src/fact/bench_test.rs`：中文 fixture 语料（4 主题 × 10 条 + 共享短语的干扰项）、6 个 golden 查询；Success@5 / Recall@5 / MRR + p50/p95 延迟。`#[ignore]` 门控，常规套件跑 smoke 变体
- 基线在真实 v2.5.3 worktree 上测得：**Success@5=1.000，MRR=0.833，p50≈0.22ms**（v2.6.0 复测一致）。未来任何检索改动必须重跑；reranker（v2.6 可选外设）的发布以本基准实测胜出为条件

🔵 **加固（SonarCloud 清理 → 质量门绿灯）**

- 14 个认知复杂度重构（rust:S3776）：每个被标记的函数提取为私有 helper，行为保持（210/210 测试）。`recall()`/`search()` 经 `parse_time_window` / `recall_persona` / `recall_atoms` / `search_multi_hop` / `search_results_to_json` 等 helper 瘦身
- Dockerfile：非 root 运行时用户 `asuna`，卷移至 `/home/asuna/.asuna`；pip `--only-binary :all:` + 锁定版本；apt `--no-install-recommends` + 排序；curl `--proto '=https'`；`cargo build --locked`
- release.yml：6 个 action 全部钉定完整 commit SHA（checkout / rust-toolchain / upload-artifact / cache / download-artifact / gh-release）；curl 强制 HTTPS；pip 锁定版本 + `--only-binary`；`cargo build --locked`
- install.sh：错误输出走 stderr、`[` → `[[`、pip `--only-binary`
- SonarCloud：**0 issues**（bugs / vulnerabilities / code_smells / security_hotspots 全为 0）；质量门 OK（reliability / security / maintainability 全 A，重复率 0%，热点审查 100%）

🔵 **代码质量**

- 210 个测试通过（新增 15 个），1 个 ignored（基准测试）。Clippy：4 个重构副产物警告已修（type alias `HttpError`/`TurnRecord` + 提取边界 helper 的带理由 `#[allow]`）；6 个存量警告在未触碰文件中，按外科手术原则保留
- 流程：7 个条目逐项经对抗式代码审查后才进入下一项，findings 当步修复（含 1 个 MAJOR：拆分子行继承 `edited_at`）。Windows（GNU 工具链）与 Linux（WSL Ubuntu 24.04）双平台验证

---

### 从 v2.5.2 升级到 v2.5.3

v2.5.3 修复自动提取 atom 的驱逐在目标行被较新 atom 的 `supersedes_id` 引用时报 `FOREIGN KEY constraint failed` 的问题——该失败使提取管线永远无法重建 `MEMORY.md`，且完全静默。

升级步骤：替换二进制文件。若 `MEMORY.md` 已与 DB 分叉，运行一次 `asuna-memory doctor --fix` 重新同步。无需数据迁移。

**v2.5.3 变更摘要：**

🔴 **修复：`supersedes_id` 外键阻塞 atom 驱逐 → MEMORY.md 永不同步**

- **根因**：四个各自合理的设计相互碰撞。`supersedes_id` 是 `bounded_memory(id)` 上的自引用外键且无 `ON DELETE` 策略；每个连接都设置 `PRAGMA foreign_keys = ON`；冲突检测总是让**较新**的 atom 引用**较旧**的；而容量驱逐恰好最老优先删除。一旦某个被 supersedes 的 atom 需要被驱逐，`DELETE` 即报 FK 冲突，`sync_atoms_to_md()` 在重建 `.md` 之前返回，`store_atoms()` 又把错误降级为 warn —— atoms 持续入库而 `MEMORY.md` 静默滞后。卡死是永久性的：同一被引用行挡住之后每次 sync，`doctor --fix`（无损合并、不驱逐）只能修表象，下次管线运行又复现
- **修复**：
  - 驱逐前先解引用（`UPDATE bounded_memory SET supersedes_id = NULL WHERE supersedes_id = ?`）再删除，被 supersedes 的 atom 可正常驱逐，幸存者不受影响（`get_chain` 在被驱逐处自然终止）
  - 整个驱逐过程（解引用 + 删除 + 审计）在单个事务内执行——中途失败不再留下"部分删除已提交但 `.md` 未重建"的分叉
  - 另外两处删除路径的同类 FK 隐患一并修复：`remove()`（条目删除，解引用 + 删除同事务）与 `split_multi_entry_rows()`（坏行删除——拆分循环现为事务化；子条目重插时 `supersedes_id` 经标量子查询解析，引用同批已删行时退化为 NULL 而非 FK 违例）
  - 驱逐时 `vec_bounded_memory` 反索引失败现在会记录日志（此前被 `let _ =` 吞掉）
  - `store_atoms()` 的 sync 失败改为 `error` 级日志并附 `doctor --fix` 提示（此前只是安静的 warn）
- **运维提示**：若通过包装脚本启动 gateway，切勿把输出接到无人读取的管道（管道缓冲写满会阻塞进程）；应重定向到日志文件

🔵 **可靠性：嵌入 API 重试**

- `embed_batch()` 遇网络错误（连接重置/拒绝/超时）自动重试最多 3 次、指数退避（1s/2s/4s）——修复 DashScope 密集顺序嵌入时连接重置导致的提取停滞。API 校验类错误不重试。

🔵 **代码质量**

- 195 个测试通过（2 个新增：驱逐被 supersedes 的 atom 后幸存者 `supersedes_id` 置空且 `.md` 重建一致；`remove()` 删除被引用条目不再触发 FK）。无新增 clippy 警告。

---

### 从 v2.5.1 升级到 v2.5.2

v2.5.2 修复有界记忆完整性 bug：单个 `bounded_memory` DB 行可能包含多个 `§` 分隔的逻辑条目，导致 `.md` 与 DB 条目数不一致（如 `.md=83, db=64`），并使 `doctor` 误报差异。

升级步骤：替换二进制文件后运行 `asuna-memory doctor --split-entries`，拆分现存的多条目行（保留元数据、跳过已存在的重复、重建 `.md`）。`doctor --fix` 现在也会在合并前先自动执行拆分。

**v2.5.2 变更摘要：**

🔴 **修复：`bounded_memory` 行内包含多个 `§` 分隔条目**

- **根因**：`BoundedMemory::write()` 与 `update()` 不拒绝含 `\n§\n`（条目分隔符）的 content。LLM 生成的写入若包含分隔符，或手动 DB 编辑，会让一行 content 持有多个逻辑条目。`.md`（按 `\n§\n` 拼接再按 `\n§\n` 切分）把它们视为 N 条，而 DB 行数只计 1 行 —— 导致 `doctor` 报 `.md≠db`，`reconcile_fix` 在这种数据上还可能产生重复而非修复差异
- **修复**：
  - `write()` 与 `update()` 现在拒绝含条目分隔符的 content / new_text，并给出明确错误提示指向"一次只写一条"
  - 新增 `split_multi_entry_rows(target)` 方法：将每条坏行拆成"每子条目一行"，保留所有元数据（`created_at`/`updated_at`/`source_session`/`confidence`/`memory_type`/`supersedes_id`/`source_turn_ids`/`confidence_score`），跳过 DB 中已存在的子条目（精确字符串匹配），删除原坏行，并重建目标 `.md`
  - 新增 CLI 参数 `asuna-memory doctor --split-entries`，独立暴露拆分操作（幂等，DB 干净时为 no-op）
  - `reconcile_fix` 现在会先跑拆分，因此含多条目行时 `doctor --fix` 不再产生重复
- **数据安全**：不丢失内容；重复项被跳过而非重新插入；操作幂等（干净 DB 上重跑报 `0 bad rows`）

🔵 **代码质量**

- 193 个测试通过（4 个新增：write 拒绝分隔符、update 拒绝分隔符、split 保留元数据且跳过重复、split 幂等性）；无新增 clippy 警告。

---

### 从 v2.5.0 升级到 v2.5.1

v2.5.1 修复 REST `/search` 端点和 CLI `search` 命令忽略 `role`（及时间）过滤参数的问题。

升级步骤：替换二进制文件，无需数据迁移。

**v2.5.1 变更摘要：**

🔴 **修复：`/search` 与 CLI `search` 忽略 `role` / 时间过滤**

- **根因**：REST 的 `SearchRequest` 结构体没有 `role`/`after`/`before`/`last_days` 字段（serde 静默丢弃），且 `/search` handler 与 CLI `cmd_search` 在构造 `SearchParams` 时把 `role: None, after_ms: None, before_ms: None` 写死。形如 `{"query":"x","role":"assistant"}` 的请求会返回所有角色的 turn。（MCP `search_sessions` 工具本就正确透传，仅 REST 和 CLI 入口受影响。）
- **修复**：`SearchRequest` 现接受 `role`、`after`、`before`、`last_days`；CLI `search` 命令新增 `--role`、`--after`、`--before`、`--last-days`。两个入口都将其传入 `SearchParams`，与 MCP 工具对齐。畸形时间戳报错（REST 返回 400）而非静默忽略；`last_days` 钳制到 `[0, 36500]`。
- **`/recall` 不变**：它返回分层 persona/scenario/atom 记忆（而非对话 turn），`role` 过滤不适用。

🔵 **代码质量**

- 189 个测试通过（新增：`SearchRequest` 反序列化接受 role/时间字段）；无新增 clippy 警告。

---

### 从 v2.4.1 升级到 v2.5.0

v2.5.0 是一次**安全 + 正确性加固**发布。向量检索切换为**余弦距离**，嵌入维度不匹配从静默失败改为显式报错，并修复了一次完整代码审查发现的约 40 个问题。

**⚠️ 升级步骤：**

1. 替换二进制文件。
2. 重启服务 —— `vec_turns` / `vec_bounded_memory` 自动迁移为余弦度量（表会被 drop 后重建）。
3. **运行 `asuna-memory rebuild`** 重新嵌入 turn 向量。在此完成前，历史 turn 的语义/混合搜索降级为仅关键词；`vec_bounded_memory`（atom 向量）会在启动时自动回填。
4. **本地 ONNX 用户**：确保 `embedding.dimensions` 与模型一致（EmbeddingGemma = 768）。维度不匹配现在会报错，而不再静默把向量索引留空。
5. **安全**：若你之前关闭 auth 且依赖开放 CORS，请显式设置 `gateway.cors_origins` 或启用 `gateway.auth_enabled` —— auth 关闭时 CORS 不再默认放行任意来源。

**v2.5.0 变更日志：**

🟢 **余弦向量检索**

- `vec0` 表现在以 `distance_metric=cosine` 创建（此前默认 L2）。语义分数现在是真正的余弦相似度；纯 `--mode semantic` 不再返回离谱的大负数分数。启动时自动迁移检测缺失的度量并重建表。
- CLI `--mode vector` 与 `--mode fts` 现在正确映射到 Semantic / Keyword（此前都落到 Hybrid）。

🔴 **关键修复：嵌入维度校验**

- `LazyEmbedder` 对每个嵌入的长度与 `config.embedding.dimensions` 做校验。本地 ONNX 模型原生维度（如 768）与配置维度（默认 1024）不一致时现在显式报错，而不再产生被 `vec0` 静默拒绝的向量 —— 后者此前会导致向量索引为空且无任何提示。
- `rebuild` 现在统计并在错误报告（`stats.errors`）中暴露向量嵌入/插入失败，而不再在索引部分/全部为空时报告"成功"。

🔵 **安全**

- auth 关闭时网关 CORS 不再默认 `allow_origin(Any)`，改为限制 localhost 来源（`http(s)://localhost / 127.0.0.1 / [::1]`，任意端口），阻断公网站点跨域读取本地记忆库。显式 `cors_origins` 与 auth 开启路径不变。
- `sql` 子命令通过首 token 白名单（`SELECT/PRAGMA/EXPLAIN/WITH`）+ 引擎级 `PRAGMA query_only=ON` 强制只读，封堵 `REPLACE` / 可写 `PRAGMA` / `VACUUM` 绕过。
- 扩充凭据扫描模式（OpenAI `sk-proj-…`、Google `AIza…`、`github_pat_…`、`Bearer` token）。
- MCP stdio 服务器用 `catch_unwind` 隔离工具 panic，单个畸形请求不会击垮服务器。

🔵 **并发**

- 会话后管线在 LLM 抽取期间释放全局 DB 锁；`/capture` 在加锁前预计算嵌入；`store_atoms` 把网络 I/O（准入/嵌入）移出写事务。慢速 LLM/嵌入调用不再冻结整个网关或撑大 WAL。

🔵 **正确性**

- 被取代的 atom 从 `vec_bounded_memory` 删除索引，矛盾事实不再与替代版本同时出现在语义检索中。
- 批内去重：单次抽取批次内的重复 atom 现在能被检出（插入时实时更新 existing 集合）。
- role/time 搜索过滤不再低于 `top_k` 少返回（先多取再截断）。
- 图 `neighbors()` 每个实体只返回一次（取最近距离），修复多路径可达节点的重复。
- DashScope 查询使用 `text_type=query`（此前一律 `document`），提升该后端的检索相关性。
- `recall()` 的 L2/L1 token 预算按层计算，而非累计总量（此前低层被饿死）。
- `/capture` 通过 `vec_int8()` 以 INT8 存储 turn 向量 —— 此前写入的是 f32 原始字节、被 `vec0` 拒绝，导致网关写入的 turn 从未被向量索引。
- 有界记忆淘汰按总容量（而非仅 atom 预算）强制收敛、并写入审计日志；`reconcile_fix` 不再把 atom 重标为永不淘汰的 `manual`。
- `/stats` 查询失败返回 500 而非误导性的 0；FTS5 关键词查询做转义（标点不再触发语法错误）；畸形 `time_range` 与溢出/负值 `last_days` 做校验；修补若干 panic（仅含头部的记忆文件、早于 epoch 的文件 mtime）；chain/graph 查询中静默吞错的 `.ok()` 改为显式 no-rows 处理。

🔵 **质量**

- 188 个测试通过（6 个新增回归测试）；无新增 clippy 警告。

---

### 从 v2.4.0 升级到 v2.4.1

v2.4.1 修复 jieba 迁移后 `bounded_memory_fts` FTS 索引为空的问题。

升级步骤：

1. 替换二进制文件
2. 重启服务 — `bounded_memory_fts` 将使用 FTS5 `'rebuild'` 命令从 `bounded_memory` 源表自动重建
3. 运行 `asuna-memory doctor` 验证

**v2.4.1 变更摘要：**

🔴 **Critical 修复：`bounded_memory_fts` 迁移后为空**

- **根因**：`SELECT COUNT(*)` 在 external-content FTS5 表（`content='bounded_memory'`）上会委托到源表，返回源表行数（如 31）而非 FTS 索引行数（0）。backfill 函数用此 COUNT 判断是否跳过，导致始终跳过 — jieba 迁移后 FTS 索引永久为空
- **修复**：用 FTS5 内置 `'rebuild'` 命令替代不可靠的 COUNT 检查：`INSERT INTO bounded_memory_fts(bounded_memory_fts) VALUES('rebuild')`。此命令完全由 FTS5 引擎处理 — 删除所有索引条目并从内容表重新索引。幂等、快速、保证正确
- **影响**：HTTP `/recall` L1 FTS 搜索和查询 `bounded_memory_fts` 的外部工具现在返回正确结果

🔵 **代码质量**

- `test_bounded_memory_fts_backfill`：模拟 jieba 迁移清空 FTS 表，验证重启后 rebuild 恢复搜索
- 182/182 测试通过

---

### 从 v2.3.1 升级到 v2.4.0

v2.4.0 将 FTS5 分词器从 `unicode61` + 自定义 UDF（`tokenize_zh`）替换为 **jieba 原生 FTS5 分词器**，实现词级中文分词并消除外部工具的 `no such function: tokenize_zh` 报错。

升级步骤：

1. 替换二进制文件
2. 无需配置变更
3. 重启服务 — 自动迁移检测旧 `unicode61` 分词器并使用 jieba 重建 FTS 表
4. 运行 `asuna-memory doctor` 验证

**v2.4.0 变更摘要：**

🟢 **新增：Jieba 原生 FTS5 分词器**

- **词级中文分词**：用 jieba 词典分词替代字级 unigram（`unicode61` + `tokenize_zh` UDF）。"北京大学" 现在分词为 "北京 大学"（2 个词）而非 "北 京 大 学"（4 个字），搜索精度大幅提升
- **外部工具兼容**：FTS 触发器不再依赖 `tokenize_zh` UDF。外部工具（Python sqlite3、sqlite3 CLI 等）现在可以直接对 `turns` 和 `bounded_memory` 表执行 INSERT/UPDATE/DELETE，不再报 `no such function: tokenize_zh` 错误
- **自动迁移**：`init_schema()` 检测现有数据库中的旧 `unicode61` 分词器，自动删除/重建 FTS 表 + 触发器为 jieba。数据保留并重新索引
- **代码简化**：移除搜索查询、FTS backfill、rebuild 和删除操作中的所有 `tokenize_chinese()` 预处理。jieba 分词器在 FTS5 引擎内部处理分词

🔵 **代码质量**

- `rusqlite` 从 0.32 升级到 0.39（bundled SQLite 3.51.3）
- 添加 `sqlite-jieba-tokenizer 0.6` 作为 FTS5 分词器提供方
- `tokenize_zh` UDF 保留向后兼容（`asuna-memory sql` 可用）但标记为 `[Deprecated]`
- 新增 3 个测试：jieba 中文词搜索、英文搜索、unicode61→jieba 迁移
- 181/181 测试通过

---

### 从 v2.3.0 升级到 v2.3.1

v2.3.1 修复了 `reconcile_fix`（`doctor --fix` 使用）从有损覆盖 `.md` 改为无损合并两端数据。

升级步骤：

1. 替换二进制文件
2. 无需配置变更
3. 运行 `asuna-memory doctor` — 如有 DIVERGED 警告，`doctor --fix` 现在会合并而非覆盖

**v2.3.1 变更摘要：**

🔴 **Critical 修复：`reconcile_fix` 无损合并**

- **根因**：`reconcile_fix()` 用 SQLite 数据完全覆盖 `.md`。手动添加到 `.md` 的条目（尚未入库）在 `doctor --fix` 时静默丢失
- **修复**：重写为三步无损合并：(1) `.md` 独有条目 → INSERT 到 SQLite，(2) SQLite 独有条目 → 追加到 `.md`，(3) 两端都有 → 不变
- **Bug 修复**：修正 `datetime('now')`（TEXT）为 `time::now_unix_ms()`（INTEGER）匹配 `created_at`/`updated_at` 列类型；修正 `confidence = 0.5`（REAL）为 `'medium'`（TEXT）匹配 schema 类型
- **`sync_atoms_to_md()` 解耦**：不再调用 `reconcile_fix()` — atom 容量驱逐后直接从 DB 写 `.md`。防止被驱逐的 atom 因仍在 `.md` 中被合并逻辑重新插入（旧流程的回归问题）
- **`doctor --fix` 输出**：从 `"rewrote .md from SQLite"` 更新为 `"merged .md and SQLite"`

🔵 **代码质量**

- `test_reconcile_fix_preserves_md_only`：验证 `.md` 独有条目在合并后保留（2 条 `.md` + 1 条 DB → 两端各 3 条）
- `test_sync_atoms_no_regression`：验证被驱逐的 atom 不会重新出现在 `.md` 中
- `test_reconcile_fix_restores_consistency`：更新为无损语义（"corrupted" 作为 `.md` 独有条目被保留）
- 178/178 测试通过

---

### 从 v2.2.3 升级到 v2.3.0

v2.3.0 新增可配置向量维度、第三方向量 API 支持（OpenAI 兼容 + DashScope 原生）和可配置批次大小。

升级步骤：

1. 替换二进制文件
2. 更新 `config.json` — 如使用 API 后端，添加 `embedding` 字段（参见 [配置文件](README_ZH.md#配置文件)）
3. 如从本地 ONNX 切换到 API（或更改维度）：`asuna-memory rebuild --full`
4. 运行 `asuna-memory doctor` 验证

**v2.3.0 变更摘要：**

🟢 **新增：第三方向量 API 支持**

- **双后端**：当 `api_url` + `api_model` 已设置时自动使用 API 后端；否则回退到本地 ONNX
- **OpenAI 兼容格式**：支持 OpenAI、Azure、Ollama、vLLM、LiteLLM、SiliconFlow 等
- **DashScope 原生格式**：支持阿里 `text-embedding-v3/v4`，原生请求/响应格式和 `text_index` 排序
- **自动检测**：`api_format` 从 URL 自动检测 DashScope；可显式覆盖
- **环境变量注入密钥**：`AMS_EMBEDDING_API_KEY` 环境变量作为 `config.json` 的替代

🟢 **新增：可配置向量维度**

- **动态 `vec0` DDL**：`init_schema()` 从 `config.embedding.dimensions` 生成向量表 DDL，不再硬编码 768
- **自动迁移**：启动时检测维度不匹配，自动删除/重建 `vec_turns` 和 `vec_bounded_memory` 表
- **默认 1024 维**：推荐用于 `text-embedding-v4` 等模型的最佳质量/成本平衡
- **维度无关代码**：所有 SQL 查询使用参数化 `vec_int8()`——无硬编码维度假设

🟡 **性能：可配置批次大小**

- `embedding.batch_size` 现在实际控制 API 批次大小（之前硬编码为 32）
- DashScope 用户应设置 `"batch_size": 10`（API 限制）
- `rebuild` 和 `backfill` 均从 embedder 配置读取批次大小

🔵 **代码质量**

- `Db::dimensions()` / `set_dimensions()` 运行时维度配置
- `ApiEmbedder` 模块支持 OpenAI + DashScope 格式和 L2 归一化
- `LazyEmbedder` 重构为 `Backend` enum（Onnx/Api）
- 176/176 测试通过

---

### 从 v2.2.2 升级到 v2.2.3

v2.2.3 修复了 `vec_bounded_memory` 在 schema 迁移（float32→int8）后为空的问题，该问题导致有界记忆的语义搜索静默退化为纯 FTS。

升级步骤：

1. 替换二进制文件
2. 重启 `ams-gateway.service` — 启动时 `maybe_backfill_bounded_memory_vec()` 自动检测缺失向量索引的 atom 并重新嵌入
3. 运行 `asuna-memory doctor` 验证 `vec_bounded_memory` 数量与 atom 数量一致

**v2.2.3 变更摘要：**

🔴 **Critical 修复：vec_bounded_memory 自动回填**

- **根因**：`init_schema()` 有 `bounded_memory_fts` 的 backfill 但没有 `vec_bounded_memory` 的。float32→int8 迁移（v2.2.1）drop 并重建表后，已有 atom 条目的向量永久丢失——有界记忆的语义搜索静默退化为 FTS
- **修复**：新增 `Db::maybe_backfill_bounded_memory_vec()` 方法，检查 `bounded_memory` 中 `memory_type='atom'` 的条目是否缺少 `vec_bounded_memory` 索引，批量嵌入（32/批，Document 前缀）并事务性插入
- **自动触发**：在 `run_gateway()`（HTTP 模式）和 `ToolHandler::new()`（MCP 模式）启动时调用——backfill 失败不阻断服务启动（错误以 warning 记录）
- **幂等性**：已有向量的 atom 通过检查 `vec_bounded_memory` 现有 rowid 跳过；完全回填后重复启动零开销

🔵 **代码质量**

- `maybe_backfill_bounded_memory_vec()` 与现有 `maybe_backfill_bounded_memory_fts()` 模式对称
- 两个调用点：`transport/http.rs` 和 `mcp/tools.rs`，均使用 `if let Err` 优雅降级
- 170/170 测试通过

---

### 从 v2.2.1 升级到 v2.2.2

v2.2.2 新增增量重建模式：当 JSONL 文件未变化时，rebuild 自动跳过 Phase 1（元数据 + FTS）并从上次中断处继续 Phase 2（向量嵌入）。这修复了中断后必须从头重建的问题。

升级步骤：

1. 替换二进制文件
2. 无需配置变更
3. 运行 `asuna-memory rebuild` — 如果上次 rebuild 被中断，会自动从最后完成的批次继续

**v2.2.2 变更摘要：**

🟢 **新增：增量重建模式**

- **自动检测恢复场景**：`rebuild` 命令现在检查 DB 是否已有与 JSONL 文件匹配的数据。如果数量一致，跳过 Phase 1（元数据 + FTS）直接进入 Phase 2（向量嵌入）
- **`--full` 标志**：使用 `asuna-memory rebuild --full` 强制完整重建（忽略现有数据）
- **默认增量模式**：当 JSONL 数量与 DB session 数量一致时，rebuild 自动跳过 Phase 1，只嵌入缺失的向量
- **清晰日志**：指示当前运行模式（"增量模式" vs "完整重建模式")

🟡 **性能：Rebuild 断点续传**

- **Phase 1 跳过**：增量模式下，跳过昂贵的 sessions/turns/FTS 删除+插入（4020 个 sessions 约 7 秒）
- **向量恢复**：Phase 2 检查 `vec_turns` 中已存在的 rowid，只嵌入缺失的 turns
- **批次进度**：从最后完成的 320 条批次继续，而非从头开始
- **安全机制**：如果 JSONL 数量与 DB 数量不一致，自动回退到完整重建

🔵 **代码质量**

- `should_do_incremental_rebuild()` 辅助函数检测恢复场景
- `rebuild_from_jsonl_with_callback()` 新增 `full_rebuild: bool` 参数
- 所有测试用例更新为使用 `full_rebuild: true` 以保证测试隔离性
- 修改文件零新增 clippy 警告
- 170/170 测试通过

---

### 从 v2.2.0 升级到 v2.2.1

v2.2.1 统一向量存储格式：`vec_bounded_memory` 改用 INT8 量化（与 `vec_turns` 一致），存储降低 4 倍。

升级步骤：

1. 替换二进制文件
2. 重启 `ams-gateway.service` — 现有 `vec_bounded_memory` 数据（float32）在首次启动时自动迁移为 int8（旧向量被丢弃，下次管线运行时重新嵌入）
3. 运行 `asuna-memory doctor` 验证

**v2.2.1 变更摘要：**

🟡 **性能：向量存储格式统一**

- **Schema 变更**：`vec_bounded_memory` 虚拟表从 `float32[768]` 改为 `int8[768]`，与 `vec_turns` 格式一致 — **存储降低 4×**（每条 atom 从 3072 字节降至 768 字节）
- **写入路径**：`L1Extractor::store_atoms()` 在 Unique 和 Conflict 两个分支均改用 `quantize_to_int8()` + `vec_int8()`
- **读取路径**：`load_existing_embeddings()` 将 int8 字节解码回 f32（`byte as i8 as f32 / 127.0`）
- **搜索路径**：`RetrievalEngine::search_atoms()` 通过 `quantize_to_int8()` + `vec_int8()` 量化查询向量用于距离比较
- **迁移**：`Db::init_schema()` 检测旧 float32 schema 并自动 drop/recreate 为 int8 格式；现有 atom 向量被丢弃（下次管线运行时重新嵌入）

🔵 **代码质量**

- `quantize_to_int8` 从 `embedder::onnx` 导入到 `memory::l1` 和 `memory::retrieval`
- 修改文件零新增 clippy 警告
- 170/170 测试通过

---

### 从 v2.1.1 升级到 v2.2.0

v2.2.0 新增 LLM 实体提取（自动构建图谱 `mentions` 关系）和成长层双写（自动提取的 atoms 同步到 MEMORY.md，带容量感知的 LRU 驱逐）。

升级步骤：

1. 替换二进制文件
2. 无需配置变更（新增 `memory.atom_capacity_ratio` 默认 0.3）
3. 重启 `ams-gateway.service` — 新会话将自动提取实体并同步 atoms 到 MEMORY.md

**v2.2.0 变更摘要：**

🟢 **新增：LLM 实体提取用于图谱构建**

- **Atom entities 字段**：`Atom` 结构体新增 `entities: Vec<String>`，由 LLM 在提取 content/atom_type/confidence 时一并提取
- **LLM prompt 更新**：`extract_from_turns()` 系统提示词现在指示 LLM 提取专有名词、技术术语、产品名、人名和组织名（每个 atom 最多 5 个，使用内容原始语言）
- **图谱 mentions 关系**：管线将提取的实体传给 `integrate_atom_with_graph()`，自动创建 `mentions` 关系（atom → entity）
- **实体名过滤**：少于 2 个字符的名称被过滤，减少噪声

🟢 **新增：成长层双写**

- **MEMORY.md 同步**：`L1Extractor::store_atoms()` 现在双写 atoms 到 `bounded_memory` 表（DB）和 `MEMORY.md`（文件），解决了 `doctor` 报告 DB 多出 3 条的 DB/.md 不一致问题
- **容量感知驱逐**：`BoundedMemory::sync_atoms_to_md()` 管理 atom 容量，采用 LRU 驱逐——当 atom 预算（默认占 MEMORY.md 容量的 30%）超出时，最旧的 `memory_type='atom'` 条目被优先驱逐
- **手动条目保护**：`memory_type='manual'` 条目永远不会被驱逐；只有自动提取的 atoms 是驱逐候选
- **可配置比例**：`memory.atom_capacity_ratio`（默认 0.3）控制 MEMORY.md 中分配给 atoms 的比例

🔵 **代码质量**

- `BoundedMemory` 新增 `with_atom_capacity_ratio()` 构建器和 `sync_atoms_to_md()` 方法
- `L1Extractor` 新增 `with_growth()` 构建器用于可选的成长层集成
- `MemoryConfig` 新增 `atom_capacity_ratio` 字段，使用 `#[serde(default)]` 保持向后兼容
- 修改文件零新增 clippy 警告
- 170/170 测试通过

---

### 从 v2.1.0 升级到 v2.1.1

v2.1.1 优化了 `rebuild` 命令，采用两阶段事务拆分、批量提交和向量嵌入断点续传。

升级步骤：

1. 替换二进制文件
2. 无需配置变更
3. 运行 `asuna-memory rebuild` 以受益于改进的性能和崩溃恢复能力

**v2.1.1 变更摘要：**

🟡 **性能：Rebuild 事务优化**

- **两阶段重建**：Phase 1（元数据 + FTS）在单个快速事务中运行（~21秒）；Phase 2（向量嵌入）在分批事务中运行（每批 1000 turns，~30秒/批）
- **崩溃恢复**：之前，2小时的重建在一个巨大事务中运行——在 99% 时崩溃会丢失所有工作。现在，崩溃最多丢失当前批次（~30秒的工作）
- **断点续传**：向量嵌入阶段检查 `vec_turns` 中已存在的 turn_id，跳过已索引的向量。崩溃后重新运行 `rebuild` 只嵌入剩余的 turns
- **进度可见性**：`rebuild_from_jsonl_with_callback` 接受进度回调；MCP `rebuild_status` 现在显示每批向量嵌入的实时进度
- **WAL 管理**：更小的事务减少 WAL 文件增长（之前 27,742 turns 会产生 18MB+）

🔵 **代码质量**

- `rebuild_from_jsonl` 重构为 `rebuild_metadata()`（Phase 1）和 `rebuild_vectors()`（Phase 2）
- 新增 `RebuildStats.vectors_skipped` 字段用于断点续传可见性
- 零新增 clippy 警告
- 170/170 测试通过

---

### 从 v2.0.4 升级到 v2.1.0

v2.1.0 新增自动图谱构建管线，在会话结束时自动提取 L1 原子并构建知识图谱实体/关系。

升级步骤：

1. 替换二进制文件
2. 设置 LLM API 凭证：`export AMS_LLM_BASE_URL=https://api.deepseek.com/v1` 和 `export AMS_LLM_API_KEY=sk-...`
3. 在 `config.json` 中启用管线：`"pipeline": { "enable_extraction": true, "every_n_turns": 5 }`
4. 重启 `ams-gateway.service`
5. 会话结束时（触发 `/session/end`）将自动创建图谱实体/关系

**v2.1.0 变更摘要：**

🟢 **新增：自动图谱构建管线**

- **会话后 L1 提取**：调用 `/session/end` 时，网关启动后台任务读取会话 turns，通过 LLM 提取原子事实（`L1Extractor`），存储带嵌入的 atoms，并集成到知识图谱（`integrate_atom_with_graph`）
- **配置驱动**：管线由 `config.json` 中的 `pipeline.enable_extraction`（默认: false）和 `graph.enabled`（默认: true）控制。短于 `pipeline.every_n_turns`（默认: 5）的会话被跳过
- **非阻塞**：管线在 `tokio::task::spawn_blocking` 中运行，避免 LLM 调用（~2-5s）期间饿死 HTTP 服务器
- **LLM 客户端**：`LlmClient` 现在实现 `Clone` 并新增 `from_config(&LlmConfig)` 构造器；从 `AMS_LLM_BASE_URL` / `AMS_LLM_API_KEY` / `AMS_LLM_MODEL` 环境变量或 `config.json` 的 `llm` 节读取
- **AppState 扩展**：HTTP 网关状态新增 `llm: Option<Arc<LlmClient>>`；LLM 未配置时（Lite 模式）管线优雅跳过
- **响应字段**：`/session/end` 现在返回 `"pipeline": "spawned"` 或 `"skipped (no LLM configured)"` 而非通用消息

🔵 **代码质量**

- 新增模块 `transport/pipeline.rs` — 隔离管线逻辑（~180 行）
- `transport/mod.rs` 更新为导出 `pipeline` 模块
- 修改文件零新增 clippy 警告
- 170/170 测试通过

---

### 从 v2.0.3 升级到 v2.0.4

v2.0.4 修复了 MCP serve 进程在找不到 `libonnxruntime.so` 时崩溃的问题，导致 `search_sessions` 返回 "Connection closed"。

升级步骤：

1. 替换二进制 **及** `libonnxruntime.so` 文件（两者均包含在 release 压缩包中）
2. 重启 `ams-gateway.service` — 二进制现在会自动从同目录、`~/.asuna/lib/` 或 `/usr/local/lib/` 发现 `libonnxruntime.so`
3. 运行 `asuna-memory doctor` — 若找到 `.so` 则显示 `嵌入引擎状态: OK`，否则输出明确的修复指引

**v2.0.4 变更摘要：**

🔴 **Critical 修复**

- **MCP serve 因 ONNX Runtime 缺失崩溃**：`ort` crate（`load-dynamic` feature）在 `libonnxruntime.so` 不可加载时直接 panic。新增 `init_ort_library_path()` 在任何 `ort` 调用之前自动从可执行文件目录、`~/.asuna/lib/` 或标准系统路径（`/usr/lib`、`/usr/local/lib`）发现动态库并设置 `ORT_DYLIB_PATH`。若库确实不存在，`ort_available()` 通过 `libloading` 安全探测并缓存失败状态，使系统优雅降级为关键词搜索而非进程崩溃。

🟡 **Docker 与安装**

- **Dockerfile**：运行时镜像从 Microsoft 官方 release 安装 ONNX Runtime（通过 `TARGETARCH` 自动选择 x64/aarch64），合并为单层 `RUN`
- **安装文档**：README 和 `for_ai.md` 新增 `sudo mv libonnxruntime.so* /usr/local/lib/` 步骤；补充自动发现行为说明
- **`doctor` 命令**：ORT 不可用时输出可操作的修复指引（`LD_LIBRARY_PATH`、`ORT_DYLIB_PATH`、标准路径建议）

🔵 **代码质量**

- `libloading` 升级为直接依赖（原已是 `ort` 的传递依赖）
- `OnceCell<bool>` 全局缓存 ORT 探测结果（首次检查后零开销）
- 实例级 `load_failed` 缓存避免重复查询全局缓存

---

### 从 v2.0.2 升级到 v2.0.3

v2.0.3 修复了未迁移数据库上 L1 FTS 检索失败、WAL 数据不可见，以及 `/capture` 网关端点的可靠性问题。

升级步骤：

1. 替换二进制文件
2. 重启 `ams-gateway.service`（触发 `wal_checkpoint(TRUNCATE)`，将 WAL 中积压的数据刷入主 DB 文件）
3. 运行 `asuna-memory rebuild` 补全之前通过 gateway 写入时缺失的向量嵌入
4. 运行 `asuna-memory doctor` 验证

**v2.0.3 变更摘要：**

🔴 **Critical 修复**

- **L1 FTS 列名不匹配**：SQL 引用了 `confidence_score`（REAL，P3 迁移列），但未迁移的数据库上该列可能不存在，导致 `"no such column: bm.confidence_score"` 错误。所有查询改为使用始终存在的 `confidence`（TEXT）列 + `CASE` 映射（`'high'`→1.0 / `'medium'`→0.5 / `'low'`→0.25）。影响 L1 FTS 召回、chain 查询、检索降级排序和批量搜索。
- **WAL 永不 checkpoint**：`Db::open()` 启动时执行 `PRAGMA wal_checkpoint(TRUNCATE)`，将长驻 gateway 进程堆积在 WAL 中的数据刷入主 DB 文件。修复了外部工具（`asuna-memory sql`、MCP serve 子进程）看到空表/旧表而数据实际在 WAL 里的问题。

🟡 **`/capture` 网关端点重构**

- **事务原子性恢复**：Session + turns INSERT 包裹在 `unchecked_transaction()` 中，防止操作中途失败导致半写入状态
- **向量嵌入生成**：`/capture` 现在通过 `embed_document()`（Document 任务前缀）为 turn 生成嵌入并写入 `vec_turns`，使 gateway 写入的 turn 可被 L0 向量搜索召回
- **Embedder 锁优化**：锁在循环外一次性获取，而非每 turn 重复获取/释放，降低并发请求的锁竞争
- **JSONL 归档**：Turn 现在追加到 JSONL 文件（使用 `OpenOptions::append`），与 `save_session` MCP 路径对齐，支持归档和 `rebuild`
- **错误可见性**：`vec_turns` 写入失败和 JSONL 写入失败现在输出 `tracing::debug` / `tracing::warn` 日志，不再静默丢弃
- **TOCTOU 安全的 JSONL 创建**：先 `open()` 后检查 `file.metadata().len()`，替代先 `path.exists()` 后 `open()` 的竞态模式

🔵 **代码质量**

- `confidence_text()` 从 `chain.rs` 提取到 `memory/mod.rs`，减少跨模块耦合
- `parse_timestamp()` 辅助函数消除了 capture 中重复 3 次的时间戳解析
- preview 长度使用 `config.conversation.preview_length` 替代硬编码 500

---

### 从 v1.3.0 升级到 v1.3.1

v1.3.1 修复了 v1.3.0 的 6 个已知 Bug，新增模型自动下载和多个 CLI 安全工具。

升级步骤：

1. 替换二进制文件
2. 运行 `asuna-memory doctor --fix` 修复 DB/.md 不一致（若有）
3. 运行 `asuna-memory model-download` 下载嵌入模型（若之前未手动放置）
4. 运行 `asuna-memory doctor` 验证全部 OK

**v1.3.1 变更摘要：**

🔴 **Critical 修复**

- **嵌入维度错误**：tarball 版 `asuna-memory` 输出 10 维而非 768 维。修复为优先选择 `sentence_embedding` 输出（2D pooled），回退到 `last_hidden_state` + masked mean pooling
- **rebuild_index 超时**：MCP 调用 `rebuild_index` 改为后台异步执行，新增 `rebuild_status` 查询进度，不再阻塞至超时
- **DB/.md 不同步**：成长层写操作改为 SQLite FIRST → .md LAST 顺序，新增 `reconcile_check` / `reconcile_fix` 和 `doctor --fix` 修复命令

🟡 **新增功能**

- **`model-download` CLI**：从 GitHub Release Assets 下载 EmbeddingGemma 模型（~300MB），替代手动放置
- **`delete-turn` CLI**：安全删除 turn（自动清理 FTS + 向量索引，解决外部工具 `tokenize_zh` UDF 缺失问题）
- **`sql` CLI**：只读 SQL 查询（进程内 UDF 可用）
- **`doctor --fix`**：自动修复 DB/.md 不一致
- **`rebuild_status` MCP 工具**：查询异步 rebuild 进度
- **`graph` 配置检测**：`doctor` 提示 config.json 缺少 `graph` 配置段

🔵 **质量改进**

- `memory_update` / `memory_remove` 现在正确传递 `session_id` 到审计日志
- 移除 3 个未使用依赖（`thiserror`、`reqwest`、`uuid`），净减 ~40 个传递依赖
- 移除死代码模块（`download.rs`、`id.rs`）和 6 个零调用方法
- `server.rs` 序列化失败不再 panic（回退到内部错误 JSON）
- 魔法数字集中化（`MS_PER_DAY`、`JSONRPC_VERSION`）

---

### 从 v1.2.1 升级到 v1.3.0

v1.3.0 在事实层和成长层之外新增**图谱记忆层**（第三层）。事实层和成长层一字不动；旧数据完全兼容。

升级步骤：

1. 替换二进制文件
2. 首次启动时 `init_schema` 自动建 `entities` + `relations` 两张表
3. 运行 `asuna-memory doctor`，预期看到：
   - `图谱: ENABLED (0 entities, 0 relations)`

无需 `rebuild`：图谱由 agent 累积，重建对图谱无意义。

**v1.3.0 变更摘要：**

🟢 **新增 · 图谱记忆层**

- 同 SQLite 数据库内新增 `entities` 和 `relations` 两张表，零新依赖
- 5 个 MCP 工具：`graph_assert` / `graph_neighbors` / `graph_path` / `graph_link_entity` / `graph_prune_dangling`
- canonical 归一化（lowercase + trim + 折空白），不做 fuzzy / 语义合并
- `save_session` 返回 `graph_pending` 软提示，列出尚未被图谱引用的 turn_id
- `doctor --verbose` 显示图谱覆盖率和悬空引用诊断
- 不调 LLM：图谱内容由 agent 自主断言，可选规则抽取也未引入

🟡 **API 表面**

- `Config.graph.enabled` / `Config.graph.remind_on_save`（serde default，老配置零迁移）
- `graph.enabled = false` 时所有图谱 MCP 工具返回 `"graph disabled in config"`
- 二进制体积不变（不引入新 crate）

---

### 从 v1.2.0 升级到 v1.2.1（强烈推荐）

v1.2.1 是一个**安全与质量加固**版本，修复了 1 个 Critical 级别的**路径穿越漏洞**和多个数据正确性问题。所有用户应当尽快升级。

```bash
# 1. 替换二进制文件

# 2. 重建索引（让向量召回质量充分受益于 query/document 前缀分离）
asuna-memory rebuild

# 3. 验证
asuna-memory doctor
# 预期新增字段：
#   版本: v1.2.1
#   外键约束: ON
```

**v1.2.1 变更摘要：**

**🔴 Critical 修复：**

- **路径穿越漏洞**：`memory_write` / `memory_update` / `memory_remove` 的 `target` 参数不再被信任直接拼路径，改为白名单 (`memory` / `user`) 严格校验，杜绝 `../../foo` 之类穿越攻击。

**🟠 Important 修复：**

- **EmbeddingGemma 前缀分离**：保存对话和重建索引时使用 `title: none | text:` (Document) 前缀；搜索查询使用 `task: search result | query:` (Query) 前缀。两者不再共用 query 前缀，召回质量显著提升（**升级后强烈建议 `rebuild`**）。
- **JSONL/SQLite 原子化**：`save_session` 改为 _DB 事务 → commit → 写 JSONL_ 顺序，且事务任意失败自动 `ROLLBACK`。彻底消除了"JSONL 已落盘但 DB 半写"的残骸状态。
- **LIKE 通配符注入**：`memory_update` / `memory_remove` 的 SQLite LIKE 子句改为 `ESCAPE '\\'` 模式，并对 `% _ \` 转义；同时改为**条目级（§ 分隔）**匹配，避免子串误改无关条目。
- **§ 分隔符鲁棒性**：连续删除多条相邻条目不再残留 `§§§`；删除最后一条只留 metadata header；删除首条不留前缀 `\n§\n`。
- **中文长内容 panic**：审计日志的内容截取从字节切片改为 `chars().take(N)`，多字节字符不再触发 panic。
- **外键约束**：默认开启 `PRAGMA foreign_keys = ON`，防止 `turns` 引用悬空 `session_id`。
- **save_session 严格校验**：`timestamp` / `role` / `content` 任一缺失立即报错；`role` 必须在 `user` / `assistant` / `tool_call` / `system` 内，不再静默吞错为 `user`。

**🟡 Minor 改进：**

- **ONNX 动态 padding**：tokenizer 不再恒填 2048，按 batch 内最长长度动态 pad，对 preview 短文本提速 5–20×。
- **凭据正则缓存**：安全扫描的 5 条凭据正则编译一次复用，扫描热路径不再每次重新编译。
- **模型下载完整性**：流式写到 `.partial` 临时文件，校验 `Content-Length` 后原子 rename，避免中断后残留半文件被误判为完成。
- **doctor 增强**：新增版本号 / 外键状态 / 嵌入向量维度展示。
- **配置字段接入**：`conversation.preview_length` / `search.default_top_k` / `search.search_mode` / `memory.security_scan` 现在真正生效。
- **e2e 测试入库**：6 个端到端测试从孤儿文件接入测试套件（覆盖 save→搜索、覆盖写入、删除残留、rebuild 一致性）。
- **死列清理**：`turns.embedding BLOB` 从 schema 移除（向量始终存在 `vec_turns` 虚表）。
- **未使用依赖**：移除 `indicatif`，新增 `once_cell` / `tempfile (dev)`。

> 注意：旧版数据库中的 `turns.embedding` 列会保留（SQLite IF NOT EXISTS 语义），不会回写也不会迁移，无害。

---

### 从 v1.1.4 升级到 v1.2.0

v1.2.0 是一个**可靠性与安全性加固**版本，修复了 2 个 Critical 级别的数据一致性问题和 8 个 Important 级别的功能缺陷。

```bash
# 1. 替换二进制文件

# 2. 重建索引以应用 char_count 修正（字节数 → 字符数）
asuna-memory rebuild

# 3. 验证
asuna-memory doctor
```

**v1.2.0 变更摘要：**

**🔴 Critical 修复：**

- **事务安全**：`save_session` 和 `rebuild` 的所有数据库写操作现在包裹在 `BEGIN IMMEDIATE ... COMMIT` 事务中，进程崩溃时不再导致数据库处于半写入不一致状态

**🟡 Important 修复：**

- **模型流式下载**：大模型文件不再完整加载到内存，改用流式 `io::copy` 写入磁盘，避免内存受限环境下 OOM
- **char_count 修正**：`turns.char_count` 字段从 UTF-8 字节数修正为 Unicode 字符数，中文内容不再虚高 3 倍
- **unsafe FFI 文档化**：sqlite-vec 扩展注册的 unsafe `transmute` 添加了完整的 SAFETY 注释和 ABI 兼容性说明
- **成长层 update() 修复**：仅在 body 上做替换，不再意外修改 metadata header；替换后自动更新时间戳；同步更新 SQLite `bounded_memory` 表
- **成长层 remove() 修复**：删除操作现在同步清理 SQLite `bounded_memory` 表
- **查询优化**：`list_entries()` 合并了对同一 session_id 的重复查询
- **MCP 错误处理文档化**：tools/call 的 `content + isError` 错误格式添加了 MCP 协议规范引用

**🟢 Minor 改进：**

- **空 turns 校验**：`save_session` 在解析前验证 turns 非空
- **时间戳安全**：`unix_ms_to_iso()` 对无效时间戳使用 epoch fallback
- **废弃 db_path 字段**：`config.json` 中的 `db_path` 字段标记为废弃，保持向后兼容

---

### 从 v1.1.3 升级到 v1.1.4

v1.1.4 修复了在某些情况下 `rebuild` 命令后向量索引回零的回归问题，并优化了重建性能。

```bash
# 1. 替换二进制文件

# 2. 重新执行重建以恢复可能回零的向量索引
asuna-memory rebuild
```

**v1.1.4 变更摘要：**

- **向量索引回归修复**：解决了 SQLite `vec0` 虚拟表在 `rebuild` 过程中由于读写游标并发冲突导致的静默写入失败。
- **重建性能优化**：合并了 FTS 和向量索引重建的查询路径，减少 50% 的数据库 IO，提升了大数据量下的重建速度。
- **错误诊断增强**：将原有的静默错误忽略改为 `warn!` 日志输出，提升了系统的可观测性。

---

### 从任意旧版本升级到 v1.1.3

v1.1.3 解决了遗留数据库由于早期 FTS 虚拟表结构而导致的 `Content in the virtual table is corrupt` 运行时损坏问题。

```bash
# 1. 替换二进制文件

# 2. 正常运行即可。如果有必要也可以运行校验：
asuna-memory doctor
```

**v1.1.3 变更摘要：**

- **自动 Schema 迁移**：对于基于旧版 (external-content) 建立的 sqlite 数据库，自动实施了至新型 contentless 结构的转换。
- **Trigger 更新保障**：针对老版本包含错误定义的同步触发器，新增 Drop And Re-create 检查逻辑，杜绝新代码跑出旧版行为，一劳永逸解决了在导入或者检索中随机抛出的虚表损坏 Panic。

---

### 从 v1.1.x 升级到 v1.1.2

v1.1.2 解决了在某些环境下 CLI 模式下中文搜索失效的问题。强烈建议所有用户升级。

```bash
# 1. 替换二进制文件

# 2. 强制重建索引（以应用增强的分词保障）
asuna-memory rebuild
```

**v1.1.2 变更摘要：**

- **FTS 稳定性增强**：将 Rebuild 阶段的分词逻辑从 SQL 层移回 Rust 层，确保在所有系统环境下分词 Token 的一致性。
- **搜索诊断输出**：CLI `search` 现在会显示分词后的结果，方便调试。

---

### 从 v1.0.x 升级到 v1.1.0

v1.1.0 修复了向量数据库未写入的问题。升级后需要重建索引以补全向量数据：

```bash
# 1. 替换二进制文件

# 2. 重建索引（会同时重建 FTS 和向量索引）
asuna-memory rebuild

# 3. 验证
asuna-memory doctor
# 预期输出包含：
#   索引统计: 10 会话, 24 轮对话, 24 个向量
```

**v1.1.0 变更摘要：**

- `rebuild` 现在会为每条 turn 生成 int8 向量并写入 `vec_turns` 表
- `save_session` / `import` 在嵌入模型可用时自动生成向量
- `doctor` 现在显示向量索引数量
- 所有写入路径（save / import / rebuild / MCP）共享统一的嵌入管道
