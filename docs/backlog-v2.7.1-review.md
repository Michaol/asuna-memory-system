# v2.7.1 复检遗留问题 Backlog

> 来源：2026-09 对 v2.7.1 的只读全面复检（`cargo test --locked` 386+4 全绿）。
> 小问题已在 `chore/v2.7.1-small-fixes` 处理；本文只记录**尚未处理**的项，按优先级排序。
> 打开某项动手前先重读对应 file:line（代码会漂移）。

## High

### H1. 发版链路不依赖测试 — **DONE**: release.yml 增加 test 门禁，release needs [test, build, model]
- 位置：`.github/workflows/release.yml` — `release` 仅 `needs: [build, model]`
- 问题：打 tag 即可发版，不跑 test/clippy/fmt，也不 `needs` CI
- 建议：tag 工作流强制 rust 测试 job 通过后再 release，或 `workflow_call` 复用 CI rust job

### H2. `docs/` 三份死文档误导 — **DONE**: ARCHIVED 横幅 + v2.7 订正（intents→intent 等）
- `docs/architecture.md`：仍写 384 维 e5、三层架构，零提及 L0–L5
- `docs/code_review_report.md`：「37 个测试」「int8[384]」与现状不符
- `docs/project-aegis-spec.md`：路径 `memory/intents/` vs 实现 `memory/intent/`
- 建议：加「历史存档，以 README 为准」横幅，或按 v2.7 重写 architecture；把关键断言纳入 `tests/doc_truth.rs`

### H3. bounded_memory 多写入口 — **DONE(主路径)**: insert_memory_row；l1/chain/pipeline 已接入
- 生产裸 INSERT：`memory/l1.rs`（commit_store）、`service/pipeline.rs`（write_scenarios）、`memory/chain.rs`（create_superseding）、`growth/bounded_memory.rs` 自体
- 全库 `INSERT INTO bounded_memory` 约 86 处（含测试）
- 问题：confidence 映射 / memory_type 默认 / supersedes 不变量分散，改一处易漏
- 建议：提取 crate-internal `insert_atom_row()`，强制容量 + MEMORY.md sync 不变量

## Medium

### M1. `graph.enabled=false` 仍关掉整条 pipeline
- 位置：`src/service/pipeline.rs` 早退门控
- 现状：行为未改但 README/for_ai 已如实标注
- 建议：门控只跳过 graph integration；L1 由 `pipeline.enable_extraction` 控制，L2/L3-L5 由各自开关控制

### M2. 无 MSRV 编译矩阵
- 位置：`.github/workflows/ci.yml` 仅装 `stable`
- 建议：增加 `toolchain: ["1.88", stable]` 或独立 msrv job，验证 `rust-version`

### M3. ONNX Runtime / install 供应链校验缺口
- `release.yml` / `Dockerfile`：curl ONNX 包无 SHA256
- `hermes-plugin/install.sh`：本次已钉 `requests==2.32.3`，但与 requirements.txt 需保持同步
- 建议：为 ORT 包加 checksum；CI 校验 install.sh 与 requirements 版本一致

### M4. `mcp/protocol.rs` 零测试
- 位置：`src/mcp/protocol.rs`
- 建议：请求/响应/错误码 / `notifications/*` 不回包 / 畸形 JSON

### M5. doctor 全链路无测试
- 位置：`src/main.rs` `cmd_doctor` 及 `doctor_print_*` / reconcile
- 建议：至少一条内存 DB 冒烟，覆盖输出片段与 `--fix` 不误伤

### M6. `filter_map(|r| r.ok())` 静默丢行
- 约 30 处（http/retrieval/bounded_memory/db/rebuild 等）
- 建议：单行解码失败 `tracing::warn` 或汇总后告警

### M7. 大文件拆分
- `transport/http.rs` ~3.7k（测试约 2k）
- `growth/bounded_memory.rs` ~2.2k
- `memory/l1.rs` ~2.0k
- `service/pipeline.rs` ~1.9k
- 建议：先迁 `#[cfg(test)]` 到独立测试文件/模块，主文件生产代码控制在 ~1k 内

### M8. `memory/mod.rs` allow 堆积
- `allow(dead_code)` ×8、`allow(unused_imports)` ×11
- 建议：按 LIVE / DORMANT 收敛 re-export，删无用出口；Skill 保持 DORMANT 标注即可

## Low

### L1. `fact/mod.rs` 再导出 `index::conversation`
- 命名归属错位，建议 conversation 稳定归属 index 后去掉 re-export 或改文档

### L2. hermes-plugin 无对真 gateway 的 e2e
- 现有 pytest 全 mock `requests`，缺一条本地 gateway 冒烟

### L3. 注释/文档漂移（本次已修一部分）
- 本次：ci.yml `1.82→1.88`、state.rs 去掉 P3 dead_code、graph/query.rs LIKE→instr、install.sh 钉 requests
- 仍可扫：`docs/plans/*` 历史计划与现状是否需标 ARCHIVE

### L4. 依赖边角
- `tokio` 本次已从 `full` 收窄为 `rt-multi-thread, macros, net, fs, sync, time`（若编译失败按需加 `io-util`）
- `ort = "=2.0.0-rc.10"` 仍 pin RC，建议在 Cargo.toml 旁注 pin 原因与 ORT 动态库版本对应关系

## 已确认不必再做的（上轮已修）

- L3–L5 接线、RetrievalEngine 单实现、SessionStore 双入口收敛
- 日常 CI、生产 unwrap/panic 隔离、Dockerfile MSRV、CORS/bind_host 语义
- Skill documented-dormant（刻意休眠，有前置条件说明）
