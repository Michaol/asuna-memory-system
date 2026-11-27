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
