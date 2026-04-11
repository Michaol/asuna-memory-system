# BUG-20260411-1636 - AMS 语义搜索未启用嵌入引擎（模型目录存在但 semantic search 不可用）

## 概要

在本机按 `for_ai.md` 的 **release 预编译二进制** 安装 `Asuna Memory System (asuna-memory 1.0.0)` 后，进行了 CLI + MCP 的全链路深度测试。

结果显示：
- 基础 CLI 功能正常
- MCP stdio 协议与工具调用正常
- JSONL 导入 / 导出 / 列表 / 重建索引 / 关键词搜索正常
- `memory_write / update / remove / read / user_profile / rebuild_index / save_session` 等 MCP 工具正常
- **但语义搜索 (`--mode semantic`) 不可用**

在 `doctor` 能识别模型目录存在的前提下，`search --mode semantic` 仍报：

```text
Error: 语义搜索需要嵌入引擎
```

这说明 AMS 当前并没有真正成功启用 embedding / vector engine。

---

## 测试环境

- 安装方式：GitHub Releases 预编译二进制（非本地编译）
- 二进制：`/root/.local/bin/asuna-memory`
- 版本：`asuna-memory 1.0.0`
- OS：Linux x64
- 数据目录：`/root/.asuna`
- Profile：`default`
- doctor 识别到的模型目录：

```text
/root/.rustrag/models/multilingual-e5-small
```

模型目录实际存在，且为软链接：

```text
/root/.rustrag/models/multilingual-e5-small -> /opt/rustrag/models/multilingual-e5-small
```

---

## 复现步骤

### 1. 确认 doctor 可识别模型目录

```bash
asuna-memory doctor
```

输出包含：

```text
模型目录: Some("/root/.rustrag/models/multilingual-e5-small")
```

### 2. 导入一条只适合语义命中的测试数据

导入内容：

```text
The crimson fruit keeps the physician away when eaten daily.
```

### 3. 分别执行 keyword / semantic / hybrid 搜索

```bash
asuna-memory search "apple health" --mode keyword --top-k 5
asuna-memory search "apple health" --mode semantic --top-k 5
asuna-memory search "apple health" --mode hybrid --top-k 5
```

---

## 实际结果

### CLI keyword

```text
搜索: "apple health" (mode=keyword)

共 0 条结果
```

### CLI semantic

```text
Error: 语义搜索需要嵌入引擎
```

### CLI hybrid

```text
搜索: "apple health" (mode=hybrid)

共 0 条结果
```

### MCP `search_sessions`

对 MCP stdio server 做同样探针后，结果与 CLI 一致：

#### MCP keyword

```json
{
  "count": 0,
  "results": [],
  "status": "ok"
}
```

#### MCP semantic

```text
错误: 语义搜索需要嵌入引擎
```

并且返回带有：

```json
"isError": true
```

#### MCP hybrid

```json
{
  "count": 0,
  "results": [],
  "status": "ok"
}
```

这说明问题不是 CLI 表层命令独有，而是 **AMS 内部供 CLI/MCP 共用的 semantic search 链路都失效了**。

---

## 期望结果

在 doctor 已识别模型目录存在的情况下：

1. `--mode semantic` 不应直接报“需要嵌入引擎”
2. 若模型可用，应正常初始化嵌入引擎并返回语义结果
3. 若初始化失败，doctor 或 search 应明确指出真正失败原因，例如：
   - ONNX Runtime 动态库未找到
   - 模型目录结构不符合预期
   - tokenizer / model 文件缺失
   - 运行时加载失败
4. `--mode hybrid` 不应静默退化到 0 结果而不给出语义层失效提示

---

## 影响范围

影响 AMS 的核心卖点之一：
- 语义检索不可用
- hybrid 搜索实际效果可能退化
- 用户会误以为模型已生效，因为 doctor 能看到模型目录

这会造成一种“看起来配好了，但其实没真正工作”的假成功状态。

---

## 深测结果摘要

本轮深度测试共覆盖 CLI + MCP 主流程。

### 通过项（核心）
- `--version`
- `doctor`
- `list-profiles`
- `import`
- `list-sessions`
- `export`
- `search --mode keyword`
- `rebuild`
- duplicate session overwrite
- MCP initialize
- MCP tools/list
- MCP `save_session`
- MCP `search_sessions`
- MCP `memory_write`
- MCP `memory_read`
- MCP `memory_update`
- MCP `memory_remove`
- MCP `memory_provenance`
- MCP `user_profile` read/write/remove
- MCP `rebuild_index`

### 失败项（真实产品问题）
- **vector / semantic search engine 未成功启用**

> 注：测试过程中还有 1 个“失败”来自测试脚本自身断言写得太苛刻，不属于 AMS 产品 bug，未计入本报告。

---

## 初步判断

更像是下面几类问题之一：

1. 模型目录仅被 doctor 检测到，但并未成功传入实际 embedding 初始化流程
2. ONNX Runtime 动态库虽随 release 提供，但运行时未被正确发现/加载
3. AMS 对模型目录结构有隐含要求，当前目录虽存在但内容不符合预期
4. semantic search 初始化失败时，错误只在内部吞掉，最终外部只看到“需要嵌入引擎”

### 本轮追加定位结果（2026-04-11 16:39）

继续做了更细的运行时验证，结果如下：

#### 1) 二进制内可见的模型/嵌入相关线索

对 release 二进制执行 `strings`，能看到如下关键字符串：

- `~/.rustrag/models/multilingual-e5-small`
- `~/.asuna/models/multilingual-e5-small`
- `model_O4.onnx`
- `embedding`
- `semantic`
- `vec_turns`

说明：
- AMS 二进制内部确实包含 embedding / semantic / vec 索引相关逻辑
- 它还内置了至少两个模型候选路径：`~/.rustrag/models/...` 与 `~/.asuna/models/...`
- 并且它期望的模型文件名里包含 `model_O4.onnx`

#### 2) 当前模型目录内容

当前实际模型目录：

```text
/root/.rustrag/models/multilingual-e5-small/
  config.json
  model_O4.onnx
  special_tokens_map.json
  tokenizer_config.json
  tokenizer.json
```

这说明：
- 路径存在
- 文件名也和二进制里暴露的 `model_O4.onnx` 对得上
- 不是“模型文件不存在”这一类低级问题

#### 3) ONNX Runtime 动态库存在

本机可见：

```text
/root/.openclaw/workspace/.tmp/asuna-memory-install/libonnxruntime.so
/opt/rustrag/libonnxruntime.so
/usr/local/lib/libonnxruntime.so
```

说明运行环境里并不缺 ORT 动态库文件本体。

#### 4) 手动补充 `LD_LIBRARY_PATH` 无效

显式注入：

```bash
LD_LIBRARY_PATH=/usr/local/lib:/opt/rustrag:/root/.openclaw/workspace/.tmp/asuna-memory-install:$LD_LIBRARY_PATH \
  asuna-memory search 'apple health' --mode semantic --top-k 3
```

结果仍然是：

```text
Error: 语义搜索需要嵌入引擎
```

这说明问题不像是一个简单的动态库搜索路径缺失。

#### 5) 手动补 `~/.asuna/models/multilingual-e5-small` 软链接也无效

我额外创建了：

```text
/root/.asuna/models/multilingual-e5-small -> /root/.rustrag/models/multilingual-e5-small
```

再次执行 semantic search，依旧报：

```text
Error: 语义搜索需要嵌入引擎
```

说明也不像是“只认 ~/.asuna/models、不认 ~/.rustrag/models”这么简单。

### 更新后的判断

从现有证据看，问题更像是以下两类中的一种：

1. **embedding engine 初始化逻辑本身有 bug**
   - doctor 只做路径检测
   - 真正进入 search 时，embedding backend 没有被构建成功
   - 但初始化失败原因没有向外暴露

2. **semantic 模式的启用判定/配置装载逻辑有 bug**
   - 即使模型与 ORT 都在，search 命令仍判定“没有嵌入引擎”
   - 可能是内部 `enabled` / `model_path` / `dimensions` / `batch_size` 等配置未正确装载默认值

因此，当前更倾向于：

> **不是模型文件缺失，也不是单纯 lib 路径缺失，而是 AMS 内部 embedding engine 的启用/初始化逻辑存在问题。**

---

## 建议

1. 给 `doctor` 增加 **embedding engine 真正初始化检查**，而不是只检查模型目录是否存在
2. 给 `search --mode semantic` 输出更明确的底层失败原因
3. 给 `search --mode hybrid` 在语义层失效时输出 warning，避免静默退化
4. 若 release 包依赖额外动态库搜索路径，应在 `for_ai.md` 中明确说明

---

## 附件

本次深测工作目录：

```text
/root/.openclaw/workspace/.tmp/ams-deep-test/
```

生成文件：
- `run_ams_deep_test.py`
- `summary.json`
- `logs.json`

---

## 状态

- 发现时间：2026-04-11 16:36 (Asia/Shanghai)
- 状态：**OPEN**
- 严重性：**MAJOR**（语义搜索主功能失效）
