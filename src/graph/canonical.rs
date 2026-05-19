//! Entity identity normalization for the graph layer.
//!
//! canonical 化字符串：lowercase + trim + 把连续空白折叠为单个空格。
//! 这是 v1.3.0 唯一的实体身份逻辑。
//! agent 是图谱内容的唯一作者；server 不做 fuzzy 匹配或语义合并。

/// 规范化实体字符串作为图谱实体 ID。
///
/// 处理流程：`trim` → `to_lowercase` → 把连续空白折叠为单个空格。
///
/// 不做 Unicode 规范化（NFC/NFD）；遵循 Rust 标准库 `str::to_lowercase` 的行为：
/// - U+0130（土耳其大写带点 İ）会展开为 `i` + U+0307（组合点）
/// - 德语 `ß` **不会** 被展开为 `ss`
///
/// 调用方在**写入和查询时都必须**调用此函数，确保入库与查询使用同一形式。
///
/// 幂等：`canonicalize(canonicalize(s)) == canonicalize(s)`
// TODO(task-2.1): 移除 #[allow(dead_code)]，当 relations::assert 调用此函数时
#[allow(dead_code)]
pub fn canonicalize(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonicalize_basic_lowercase() {
        assert_eq!(canonicalize("Alice"), "alice");
        assert_eq!(canonicalize("ALICE SMITH"), "alice smith");
    }

    #[test]
    fn test_canonicalize_trims_and_folds_whitespace() {
        assert_eq!(canonicalize("  Alice  "), "alice");
        assert_eq!(canonicalize("Alice   Smith"), "alice smith");
        assert_eq!(canonicalize("Alice\tSmith"), "alice smith");
        assert_eq!(canonicalize("Alice\nSmith"), "alice smith");
    }

    #[test]
    fn test_canonicalize_chinese_passthrough() {
        // 中文不受 lowercase 影响；中文之间无空白时不补空白
        assert_eq!(canonicalize("亚丝娜"), "亚丝娜");
        assert_eq!(canonicalize("亚 丝 娜"), "亚 丝 娜");
        assert_eq!(canonicalize("亚丝娜  "), "亚丝娜");
    }

    #[test]
    fn test_canonicalize_empty() {
        assert_eq!(canonicalize(""), "");
        assert_eq!(canonicalize("   "), "");
        assert_eq!(canonicalize("\t\n"), "");
    }

    #[test]
    fn test_canonicalize_mixed() {
        assert_eq!(canonicalize("OpenAI Inc"), "openai inc");
        assert_eq!(canonicalize("Project / Asuna"), "project / asuna");
    }

    #[test]
    fn test_canonicalize_idempotent() {
        // 幂等性是 canonical 作为实体 ID 的核心不变量：
        // 同一输入多次 canonicalize 必须产出相同字符串
        for s in ["Alice", "  ALICE  Smith\t", "亚丝娜", "", "İ", "OpenAI Inc"] {
            let once = canonicalize(s);
            assert_eq!(canonicalize(&once), once, "not idempotent: {:?}", s);
        }
    }
}
