//! Entity identity normalization for the graph layer.
//!
//! canonical 化字符串：lowercase + trim + 把连续空白折叠为单个空格。
//! 这是 v1.3.0 唯一的实体身份逻辑。
//! agent 是图谱内容的唯一作者；server 不做 fuzzy 匹配或语义合并。

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
}
