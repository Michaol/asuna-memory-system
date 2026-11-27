use super::*;

#[test]
fn test_tokenize_chinese() {
    assert_eq!(tokenize_chinese("亚丝娜"), "亚 丝 娜");
    assert_eq!(tokenize_chinese("Hello亚丝娜"), "Hello 亚 丝 娜");
    assert_eq!(tokenize_chinese("亚丝娜is back"), "亚 丝 娜 is back");
    assert_eq!(tokenize_chinese("你好 世界"), "你 好 世 界");
}

#[test]
fn test_escape_like() {
    assert_eq!(escape_like("plain text"), "plain text");
    assert_eq!(escape_like("50%"), "50\\%");
    assert_eq!(escape_like("a_b"), "a\\_b");
    assert_eq!(escape_like("back\\slash"), "back\\\\slash");
    assert_eq!(escape_like("%_\\"), "\\%\\_\\\\");
    // CJK passes through untouched
    assert_eq!(escape_like("亚丝娜%"), "亚丝娜\\%");
}

#[test]
fn test_estimate_tokens() {
    // Empty
    assert_eq!(estimate_tokens(""), 0);
    // Pure CJK: 1 token per char
    assert_eq!(estimate_tokens("你好世界"), 4);
    // CJK punctuation and fullwidth forms count as 1 each
    assert_eq!(estimate_tokens("你好，世界！"), 6);
    // Kana and Hangul count as 1 each (review: undercount is the unsafe direction)
    assert_eq!(estimate_tokens("こんにちは"), 5);
    assert_eq!(estimate_tokens("안녕하세요"), 5);
    // Pure ASCII: ~3 chars per token, rounded up
    assert_eq!(estimate_tokens("abc"), 1);
    assert_eq!(estimate_tokens("abcd"), 2);
    // Mixed: 2 CJK + 6 ASCII -> 2 + ceil(6/3) = 4
    assert_eq!(estimate_tokens("你好abcdef"), 4);
    // Monotonic: longer text never estimates fewer tokens
    assert!(estimate_tokens("你好世界abc") >= estimate_tokens("你好世界"));
}

#[test]
fn test_build_safe_fts_query_and_or_variants() {
    assert_eq!(
        build_safe_fts_query("Rust 内存 安全").as_deref(),
        Some("\"Rust\" \"内存\" \"安全\"")
    );
    // AND 主查询保持整串 phrase（与索引侧停用词过滤口径无法对齐，切词后 AND
    // 会让含停用词的中文查询恒 0 行）；非邻接匹配由下面的 OR 回退承担。
    assert_eq!(
        build_safe_fts_query("数据库备份").as_deref(),
        Some("\"数据库备份\"")
    );
    assert_eq!(
        build_safe_fts_query("foo:bar OR (baz* NOT ^qux)").as_deref(),
        Some("\"foo\" \"bar\" \"OR\" \"baz\" \"NOT\" \"qux\"")
    );
    assert_eq!(build_safe_fts_query("   !!! --- ...   "), None);
    // W2: 中文串切词后 >=2 词条，OR 回退对中文不再是结构性不可达
    assert_eq!(
        build_safe_fts_or_query("单关键词").as_deref(),
        Some("\"单\" OR \"关键词\"")
    );
    assert_eq!(
        build_safe_fts_or_query("Rust 内存 安全").as_deref(),
        Some("\"Rust\" OR \"内存\" OR \"安全\"")
    );
}

/// W28: OR 词条按 token 组轮询——首个超长中文 token（jieba 切出 >32 词）
/// 不得独占 32 词预算，后到的拉丁词 "Rust" 仍须保留（旧的平铺顺序取词
/// 会让首个长 CJK token 吃满预算、丢掉后面的英文词）。
#[test]
fn test_extract_fts_or_terms_round_robin_keeps_later_latin_term() {
    let zh = "我们今天一起讨论这个系统的问题因为他说她想要知道为什么那个方法可以解决目前出现的错误所以大家开始研究新的方案并且希望尽快完成所有工作让项目变得更加稳定可靠";
    let words: Vec<&str> = shared_jieba()
        .cut(zh, true)
        .iter()
        .map(|t| t.word.trim())
        .filter(|w| !w.is_empty())
        .collect();
    assert!(
        words.len() > MAX_FTS_TERMS,
        "测试前提：该中文串切出 >{} 词，got {}",
        MAX_FTS_TERMS,
        words.len()
    );
    let query = format!("{zh} Rust");
    let terms = extract_fts_or_terms(&query);
    assert!(terms.len() <= MAX_FTS_TERMS, "terms: {terms:?}");
    assert!(terms.contains(&"\"Rust\"".to_string()), "terms: {terms:?}");
}

/// W28: 词条大小写不敏感去重——"Rust"/"rust"/"RUST" 只计首次形态，重复
/// 中文词同理。
#[test]
fn test_extract_fts_or_terms_dedup_case_insensitive() {
    assert_eq!(
        extract_fts_or_terms("Rust rust RUST 数据库 数据库"),
        vec!["\"Rust\"".to_string(), "\"数据库\"".to_string()]
    );
}

/// M6a/W28: is_chinese_char 覆盖基本区末尾 U+9FFF（F7 同口径边界）。
#[test]
fn test_is_chinese_char_upper_boundary() {
    assert!(is_chinese_char('\u{9FFF}'));
    assert!(!is_chinese_char('\u{A000}'));
}
