# ROOT CAUSE ANALYSIS: AMS Semantic Search Failure (Missing Input: token_type_ids)

## 1. 问题现状 (Current State)
- **修复前**：`search --mode semantic` 报错 `Error: 语义搜索需要嵌入引擎`。
- **修复后 (v1.0.0)**：引擎已加载，模型路径识别正确，但运行时报错：
  ```text
  [E:onnxruntime:, sequential_executor.cc:572 ExecuteKernel] Non-zero status code returned while running Gather node. 
  Name:'/embeddings/token_type_embeddings/Gather' 
  Status Message: ... Missing Input: token_type_ids
  ```
- **结论**：引擎已启动，但**推理输入张量构造与 ONNX 模型定义不匹配**。

## 2. 根本原因 (Root Cause)
### 2.1 模型输入定义不匹配
- **AMS 代码假设**：所有 BERT 类模型都需要 `input_ids`, `attention_mask`, **`token_type_ids`**。
- **实际模型行为**：`multilingual-e5-small` 的 ONNX 导出版本（`model_O4.onnx`）**可能未定义 `token_type_ids` 输入节点**，或者该节点是可选的。
- **冲突点**：ONNX Runtime 在运行时检测到模型定义中不存在 `token_type_ids` 输入，但 AMS 代码尝试传递该输入（或反之，模型期望该输入但 AMS 未传递，导致 `Gather` 节点失败）。

### 2.2 代码缺乏动态适配
- AMS 的 `src/embedder/onnx.rs` (或类似文件) 中，构造输入张量的逻辑是**静态的**：
  ```rust
  // 伪代码示例
  let inputs = vec![
      ("input_ids", input_ids_tensor),
      ("attention_mask", attention_mask_tensor),
      ("token_type_ids", token_type_ids_tensor), // 硬编码
  ];
  ```
- **问题**：没有检查模型实际需要的输入节点列表 (`session.get_inputs()`)，直接硬编码传递所有 BERT 标准输入。

## 3. 复现证据 (Evidence)
- **模型文件**：`/root/.rustrag/models/multilingual-e5-small/model_O4.onnx`
- **错误日志**：
  ```text
  Missing Input: token_type_ids
  Name: '/embeddings/token_type_embeddings/Gather'
  ```
- **环境**：
  - ONNX Runtime 版本：`libonnxruntime.so` (通过 `LD_LIBRARY_PATH` 加载)
  - 模型路径：`/root/.rustrag/models/multilingual-e5-small`

## 4. 修复方案建议 (Fix Recommendations)
### 方案 A：动态输入检测 (推荐)
修改 AMS 代码，在初始化时读取模型输入节点，只构造模型实际需要的输入：
```rust
let inputs = session.get_inputs();
let mut session_inputs = Vec::new();
for input in inputs {
    if input.name == "input_ids" {
        session_inputs.push(("input_ids", input_ids_tensor));
    } else if input.name == "attention_mask" {
        session_inputs.push(("attention_mask", attention_mask_tensor));
    } else if input.name == "token_type_ids" {
        session_inputs.push(("token_type_ids", token_type_ids_tensor));
    }
}
```

### 方案 B：强制包含 token_type_ids
如果模型确实需要该输入但未导出，重新导出 ONNX 模型时添加 `token_type_ids` 占位符（全 0 张量）。

### 方案 C：硬编码跳过 (针对 E5 模型)
如果确认所有 E5 模型都不需要 `token_type_ids`，在代码中显式跳过该字段：
```rust
// 仅当模型名称包含 "e5" 时跳过
if model_name.contains("e5") {
    // 不构造 token_type_ids
}
```

## 5. 所需信息 (For AI Agent)
- **代码位置**：请检查 `src/embedder/onnx.rs` 或 `src/main.rs` 中 `search` 命令的推理逻辑。
- **模型输入检查**：运行以下命令查看模型实际输入：
  ```bash
  python3 -c "import onnx; m=onnx.load('/root/.rustrag/models/multilingual-e5-small/model_O4.onnx'); print([i.name for i in m.graph.input])"
  ```
- **修复优先级**：高 (阻断语义搜索核心功能)。

## 6. 附件
- 错误日志：见上文
- 模型路径：`/root/.rustrag/models/multilingual-e5-small/model_O4.onnx`
- 测试命令：`asuna-memory search "apple health" --mode semantic --top-k 3`

---
**生成时间**: 2026-04-11 17:20
**状态**: 待修复 (Pending Fix)
