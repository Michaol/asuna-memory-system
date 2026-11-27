//! 发布文档终态真实性门禁（"文档撒谎" / 版本漂移回归防护）：
//! - breaking 清单覆盖 v3.0.0（1024 维锁定 + rebuild --full）与历史 v2.7.0 auth 启用反转；
//! - 安全叙述与实现对齐：HISTORY* 保留 v2.7.0 "no write-side scan" 历史实录，
//!   v3.0.0 README* 须如实陈述 L1–L5 + reconcile_fix 写侧扫描与 L0 软审计；
//! - README 引用的 doctor 标签（`图谱:`）与 FTS 分词器（`jieba`）与源码一致；
//! - Cargo.toml / Cargo.lock / Hermes 插件所有版本清单同源锁定（3.0.0）；
//! - active 发布文档零过时 ONNX / model-download / Skill Memory 残留声明。

use std::path::Path;

fn repo_file(name: &str) -> String {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读取 {}: {e}", p.display()))
}

/// 截取 `start`（含）到其后第一个 `end`（不含）之间的文本。
fn section(doc: &str, start: &str, end: &str) -> String {
    let s = doc
        .find(start)
        .unwrap_or_else(|| panic!("文档中找不到片段起点 {start:?}"));
    let after_start = s + start.len();
    let e = doc
        .get(after_start..)
        .and_then(|tail| tail.find(end))
        .map(|i| after_start + i)
        .unwrap_or(doc.len());
    doc.get(s..e).unwrap_or_default().to_string()
}

fn first_line_with(doc: &str, marker: &str) -> String {
    doc.lines()
        .find(|l| l.starts_with(marker))
        .unwrap_or_else(|| panic!("文档中找不到以 {marker:?} 开头的行"))
        .to_string()
}

/// breaking 清单必须包含 v3.0.0（1024 + rebuild --full）以及历史 v2.7.0 auth 启用反转与回退说明。
#[test]
fn breaking_lists_include_gateway_auth_enablement() {
    let history_en_raw = repo_file("HISTORY.md");
    let history_zh_raw = repo_file("HISTORY_ZH.md");

    let history_v27_en = section(
        &history_en_raw,
        "### Upgrading from v2.6.2 to v2.7.0",
        "**v2.7.0 Changelog",
    );
    let history_v27_zh = section(
        &history_zh_raw,
        "### 从 v2.6.2 升级到 v2.7.0",
        "**v2.7.0 变更摘要",
    );
    let history_v3_en = section(
        &history_en_raw,
        "### Upgrading from v2.7.3 to v3.0.0",
        "### Upgrading from v2.7.1 to v2.7.3",
    );
    let history_v3_zh = section(
        &history_zh_raw,
        "### 从 v2.7.3 升级到 v3.0.0",
        "### 从 v2.7.1 升级到 v2.7.3",
    );
    let readme_en = first_line_with(&repo_file("README.md"), "**Breaking (must-read)**");
    let readme_zh = first_line_with(&repo_file("README_ZH.md"), "**Breaking（必读）**");

    for (label, text) in [
        ("HISTORY.md(v2.7.0)", &history_v27_en),
        ("HISTORY_ZH.md(v2.7.0)", &history_v27_zh),
        ("README.md", &readme_en),
        ("README_ZH.md", &readme_zh),
    ] {
        assert!(
            text.contains("AMS_GATEWAY_API_KEY"),
            "{label}: breaking 清单缺 AMS_GATEWAY_API_KEY 隐含启用 auth 说明"
        );
        assert!(
            text.contains("AMS_GATEWAY_AUTH_ENABLED=false"),
            "{label}: breaking 清单缺 AMS_GATEWAY_AUTH_ENABLED=false 回退说明"
        );
    }

    for (label, text) in [
        ("HISTORY.md(v3.0.0)", &history_v3_en),
        ("HISTORY_ZH.md(v3.0.0)", &history_v3_zh),
        ("README.md", &readme_en),
        ("README_ZH.md", &readme_zh),
    ] {
        assert!(
            text.contains("1024") && text.contains("rebuild --full"),
            "{label}: v3.0.0 breaking 清单缺 1024 维锁定或 rebuild --full 指引"
        );
    }
}

/// 安全声明真实性：HISTORY* v2.7.0 段落保留历史实录；v3.0.0 README* 如实陈述 L1-L5 写侧扫描与 L0 软审计。
#[test]
fn security_scan_claim_is_scope_limited() {
    let history_en_v27 = section(
        &repo_file("HISTORY.md"),
        "### Upgrading from v2.6.2 to v2.7.0",
        "### Upgrading from v2.6.1",
    );
    let history_zh_v27 = section(
        &repo_file("HISTORY_ZH.md"),
        "### 从 v2.6.2 升级到 v2.7.0",
        "### 从 v2.6.1",
    );
    let readme_en = repo_file("README.md");
    let readme_zh = repo_file("README_ZH.md");

    for (label, text) in [
        ("HISTORY.md(v2.7.0)", &history_en_v27),
        ("README.md", &readme_en),
    ] {
        assert!(
            !text.contains("every automatic write path"),
            "{label}: 不得再使用不区分硬门/软审计的 'every automatic write path' 笼统措辞"
        );
    }
    for (label, text) in [
        ("HISTORY_ZH.md(v2.7.0)", &history_zh_v27),
        ("README_ZH.md", &readme_zh),
    ] {
        assert!(
            !text.contains("全部自动写入路径"),
            "{label}: 不得再使用笼统措辞 '全部自动写入路径'"
        );
    }

    // 历史 v2.7.0 段落保留当时真实局限
    assert!(history_en_v27.contains("no write-side scan"));
    assert!(history_zh_v27.contains("没有写侧扫描"));

    // v3.0.0 现状文档已补齐 L2-L5 + reconcile_fix 写侧扫描，不得再残留 "no write-side scan"
    assert!(
        !readme_en.contains("no write-side scan"),
        "README.md 仍残留 v2.7.0 过时的 'no write-side scan' 声明"
    );
    assert!(
        !readme_zh.contains("没有写侧扫描"),
        "README_ZH.md 仍残留 v2.7.0 过时的 '没有写侧扫描' 声明"
    );
    for (label, text) in [("README.md", &readme_en), ("README_ZH.md", &readme_zh)] {
        for needle in [
            "L2",
            "L3",
            "L4",
            "L5",
            "reconcile_fix",
            "/capture",
            "save_session",
        ] {
            assert!(
                text.contains(needle),
                "{label}: v3.0.0 安全说明缺关键覆盖项 {needle:?}"
            );
        }
    }
}

/// F4：README 不得给 doctor 输出造假字面值（二进制打印 `图谱:`）。
#[test]
fn readme_quotes_real_doctor_labels() {
    for name in ["README.md", "README_ZH.md"] {
        let doc = repo_file(name);
        assert!(
            doc.contains("图谱: ENABLED"),
            "{name}: doctor 描述必须引用二进制真实输出 `图谱:`"
        );
        assert!(
            !doc.contains("Graph: ENABLED"),
            "{name}: 二进制从不输出英文 `Graph:` 标签（src/main.rs doctor）"
        );
    }
}

/// F5：README 的 FTS 叙述必须与实现一致（jieba，非 unigram）。
#[test]
fn readme_fts_describes_jieba_tokenizer() {
    let schema = repo_file("src/index/schema.rs");
    assert!(
        schema.contains("tokenize='jieba'") || schema.contains("tokenize = 'jieba'"),
        "schema.rs 的 FTS tokenize 断言失效"
    );
    for name in ["README.md", "README_ZH.md"] {
        let doc = repo_file(name);
        assert!(
            doc.contains("jieba"),
            "{name}: FTS 描述缺 jieba（与 schema.rs 矛盾）"
        );
        assert!(
            !doc.contains("unigram"),
            "{name}: 仍残留 unigram 描述——v2.4.0 起已被 jieba 替代"
        );
    }
}

/// 全部 5 处版本清单必须与 CARGO_PKG_VERSION（3.0.0）严格一致。
#[test]
fn version_manifests_are_in_sync() {
    let ver = env!("CARGO_PKG_VERSION");
    assert_eq!(ver, "3.0.0", "crate version must be bumped to 3.0.0");

    let cargo_toml = repo_file("Cargo.toml");
    assert!(
        cargo_toml.contains(&format!("version = \"{ver}\"")),
        "Cargo.toml 缺少 version = \"{ver}\""
    );

    let cargo_lock = repo_file("Cargo.lock");
    let pkg_block = section(&cargo_lock, "name = \"asuna-memory\"", "dependencies = [");
    assert!(
        pkg_block.contains(&format!("version = \"{ver}\"")),
        "Cargo.lock asuna-memory 条目版本号与 {ver} 不一致"
    );

    let setup_py = repo_file("hermes-plugin/setup.py");
    assert!(
        setup_py.contains(&format!("version=\"{ver}\"")),
        "hermes-plugin/setup.py 版本号与 {ver} 不一致"
    );

    let init_py = repo_file("hermes-plugin/ams_memory/__init__.py");
    assert!(
        init_py.contains(&format!("__version__ = \"{ver}\"")),
        "hermes-plugin/ams_memory/__init__.py 版本号与 {ver} 不一致"
    );

    let plugin_yaml = repo_file("hermes-plugin/plugin.yaml");
    assert!(
        plugin_yaml.contains(&format!("version: {ver}")),
        "hermes-plugin/plugin.yaml 版本号与 {ver} 不一致"
    );
}

/// 现行功能/安装说明（Architecture 起全章 + for_ai.md）不得残留已删除的本地 ONNX / model-download / Skill Memory 声明。
#[test]
fn docs_have_no_stale_onnx_or_skill_claims() {
    let forbidden = [
        "model-download",
        "libonnxruntime",
        "onnxruntime.dll",
        "ORT_DYLIB_PATH",
        "ASUNA_DEV_ROOT",
        "embeddinggemma",
        "asuna-memory-windows-x64.exe.zip",
        "Skill Memory",
        "技能记忆",
    ];
    let readme_en_raw = repo_file("README.md");
    let readme_zh_raw = repo_file("README_ZH.md");
    let readme_en_body = section(&readme_en_raw, "### Architecture: Project Aegis", "\0");
    let readme_zh_body = section(&readme_zh_raw, "### 架构：Project Aegis", "\0");
    let for_ai = repo_file("for_ai.md");

    for (name, doc) in [
        ("README.md (active sections)", &readme_en_body),
        ("README_ZH.md (active sections)", &readme_zh_body),
        ("for_ai.md", &for_ai),
    ] {
        for needle in forbidden {
            assert!(
                !doc.contains(needle),
                "{name}: v3.0.0 现行文档仍残留已删除能力引用 {needle:?}"
            );
        }
    }
}

/// v4-M: D2-7 升至 4，A1/W4.3 升至 5，Phase 3（jieba-rs 0.11 FTS5 原子重建）再升至 6。
/// 活跃文档须声称 user_version = 6，不得残留 = 3 / = 4 / = 5 旧叙述；
/// db.rs 常量被钉住，未来再升版必须同步文档与本门禁。
#[test]
fn schema_version_claims_match_db() {
    let db = repo_file("src/index/db.rs");
    assert!(
        db.contains("CURRENT_SCHEMA_VERSION: i32 = 6"),
        "db.rs CURRENT_SCHEMA_VERSION 已变更——请同步 README*/HISTORY* 的 user_version 叙述与本门禁"
    );
    for name in ["README.md", "README_ZH.md", "HISTORY.md", "HISTORY_ZH.md"] {
        let doc = repo_file(name);
        for stale in ["user_version = 3", "user_version = 4", "user_version = 5"] {
            assert!(
                !doc.contains(stale),
                "{name}: user_version 叙述漂移（残留 {stale}，db.rs 现为 6）"
            );
        }
    }
}
