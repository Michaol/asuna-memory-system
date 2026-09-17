//! 发布文档终态真实性门禁（跨步审查 F2/F3/F4/F5 的回归防护，"文档撒谎"类）：
//! - F3：breaking 清单（EN/ZH 四处）必须含「AMS_GATEWAY_API_KEY 隐含启用
//!   auth」条目与 `AMS_GATEWAY_AUTH_ENABLED=false` 回退说明；
//! - F2：安全叙述不得再出现「every automatic write path / 全部自动写入路径」
//!   总括（L2-L5 生成写面无写侧扫描），且必须带显式范围说明；
//! - F4：README 引用的 doctor 字面输出必须与二进制一致（`图谱:`，非 `Graph:`）；
//! - F5：README 的 FTS 叙述必须与 `src/index/schema.rs` 的 `tokenize='jieba'`
//!   一致（unigram 自 v2.4.0 已被替代，见 `src/util/text.rs` 注释）。
//!
//! 历史条目（HISTORY.md v2.6.1 之前的段落，如 v1.3.0 的 `Graph: ENABLED` 实录）
//! 不在断言范围：切片只覆盖 v2.7.0 节 / README 现状描述。

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
    let e = doc[s + start.len()..]
        .find(end)
        .map(|i| s + start.len() + i)
        .unwrap_or(doc.len());
    doc[s..e].to_string()
}

fn first_line_with(doc: &str, marker: &str) -> String {
    doc.lines()
        .find(|l| l.starts_with(marker))
        .unwrap_or_else(|| panic!("文档中找不到以 {marker:?} 开头的行"))
        .to_string()
}

/// F3：auth 启用反转必须进 breaking 清单（四个发布物文档），含回退开关。
#[test]
fn breaking_lists_include_gateway_auth_enablement() {
    let history_en = section(
        &repo_file("HISTORY.md"),
        "**⚠️ Breaking changes:**",
        "**v2.7.0 Changelog",
    );
    let history_zh = section(
        &repo_file("HISTORY_ZH.md"),
        "**⚠️ Breaking changes：**",
        "**v2.7.0 变更摘要",
    );
    let readme_en = first_line_with(&repo_file("README.md"), "**Breaking (must-read)**");
    let readme_zh = first_line_with(&repo_file("README_ZH.md"), "**Breaking（必读）**");

    for (label, text) in [
        ("HISTORY.md", &history_en),
        ("HISTORY_ZH.md", &history_zh),
        ("README.md", &readme_en),
        ("README_ZH.md", &readme_zh),
    ] {
        assert!(
            text.contains("AMS_GATEWAY_API_KEY"),
            "{label}: breaking 清单缺「AMS_GATEWAY_API_KEY 隐含启用 auth」条目（升级可致全员 401，属最高影响面变更）"
        );
        assert!(
            text.contains("AMS_GATEWAY_AUTH_ENABLED=false"),
            "{label}: breaking 清单缺 AMS_GATEWAY_AUTH_ENABLED=false 回退说明"
        );
    }
}

/// F2：安全总括句必须如实限定范围（L2-L5 生成写面无写侧扫描）。
#[test]
fn security_scan_claim_is_scope_limited() {
    let history_en = section(
        &repo_file("HISTORY.md"),
        "**Upgrade steps:**",
        "### Upgrading from v2.6.1",
    );
    let history_zh = section(&repo_file("HISTORY_ZH.md"), "**升级步骤**", "### 从 v2.6.1");
    let readme_en = repo_file("README.md");
    let readme_zh = repo_file("README_ZH.md");

    for (label, text) in [("HISTORY.md", &history_en), ("README.md", &readme_en)] {
        assert!(
            !text.contains("every automatic write path"),
            "{label}: 「every automatic write path」为过度声明——L2 场景直插与 L3/L4/L5 文档写面无写侧扫描"
        );
    }
    for (label, text) in [("HISTORY_ZH.md", &history_zh), ("README_ZH.md", &readme_zh)] {
        assert!(
            !text.contains("全部自动写入路径"),
            "{label}: 「全部自动写入路径」为过度声明（同上）"
        );
    }
    // 范围说明必须显式列出未覆盖面（以 persona.md 为锚点）
    assert!(
        history_en.contains("no write-side scan") && readme_en.contains("no write-side scan"),
        "EN 文档缺 L2-L5 生成写面无写侧扫描的范围说明"
    );
    assert!(
        history_zh.contains("没有写侧扫描") && readme_zh.contains("没有写侧扫描"),
        "ZH 文档缺 L2-L5 生成写面无写侧扫描的范围说明"
    );
}

/// F4：README 不得给 doctor 输出造假字面值（二进制打印 `图谱:`，从未有 i18n）。
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

/// F5：README 的 FTS 叙述必须与实现一致（jieba，非 unigram）——与自家
/// for_ai.md 排障表（`no such tokenizer: jieba`）互斥陈述即文档撒谎。
#[test]
fn readme_fts_describes_jieba_tokenizer() {
    // 前提钉住：schema.rs 确实以 tokenize='jieba' 建表
    let schema = repo_file("src/index/schema.rs");
    assert!(
        schema.contains("tokenize='jieba'") || schema.contains("tokenize = 'jieba'"),
        "schema.rs 的 FTS tokenize 断言失效——若实现已再换分词器，请同步更新本测试与 README"
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
