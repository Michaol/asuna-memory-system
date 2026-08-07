/// [Deprecated] 简单的中文分词处理：在汉字之间插入空格，使 FTS5 (unicode61) 能够正确索引和匹配。
///
/// 已被 jieba FTS5 tokenizer 替代（v2.4.0）。保留用于 `tokenize_zh` UDF 向后兼容。
#[allow(dead_code)]
#[allow(clippy::nonminimal_bool)]
pub fn tokenize_chinese(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let mut last_was_zh = false;

    for c in text.chars() {
        let is_zh = is_chinese_char(c);

        // 中文与中文之间、中文与非中文之间、非中文与中文之间都需补空格
        if (is_zh && last_was_zh)
            || (is_zh && !last_was_zh && !result.is_empty() && !result.ends_with(' '))
            || (!is_zh && last_was_zh && c != ' ')
        {
            result.push(' ');
        }

        result.push(c);
        last_was_zh = is_zh;
    }
    result
}

/// 判断是否为中文字符
fn is_chinese_char(c: char) -> bool {
    ('\u{4e00}'..='\u{9fa5}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{20000}'..='\u{2a6df}').contains(&c)
}

/// 轻量 token 估算（v2.6 预算控制用，不引入外部分词器依赖）。
///
/// CJK 表意文字 / CJK 标点 / 全角字符按每字 1 token 计，其余字符按约
/// 3 字符 1 token 计（向上取整）。与真实 tokenizer（如 cl100k）存在偏差，
/// 对预算控制场景可接受——预算本身是近似约束而非精确计量。
pub fn estimate_tokens(text: &str) -> usize {
    let mut heavy = 0usize; // CJK/全角：~1 token/字
    let mut light = 0usize; // 其余：~3 字符/token
    for c in text.chars() {
        if is_chinese_char(c)
            || ('\u{3000}'..='\u{303f}').contains(&c) // CJK 标点
            || ('\u{3040}'..='\u{30ff}').contains(&c) // 日文假名
            || ('\u{ac00}'..='\u{d7af}').contains(&c) // 谚文音节
            || ('\u{ff00}'..='\u{ffef}').contains(&c) // 全角/半角形
        {
            heavy += 1;
        } else {
            light += 1;
        }
    }
    heavy + light.div_ceil(3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_chinese() {
        assert_eq!(tokenize_chinese("亚丝娜"), "亚 丝 娜");
        assert_eq!(tokenize_chinese("Hello亚丝娜"), "Hello 亚 丝 娜");
        assert_eq!(tokenize_chinese("亚丝娜is back"), "亚 丝 娜 is back");
        assert_eq!(tokenize_chinese("你好 世界"), "你 好 世 界");
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
}
