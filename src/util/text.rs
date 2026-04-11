/// 简单的中文分词处理：在汉字之间插入空格，使 FTS5 (unicode61) 能够正确索引和匹配
pub fn tokenize_chinese(text: &str) -> String {
    let mut result = String::with_capacity(text.len() * 2);
    let mut last_was_zh = false;

    for c in text.chars() {
        let is_zh = is_chinese_char(c);

        // 如果当前是中文，且上一个也是中文，中间补空格
        // 或者当前是中文，上一个是非中文（且非空格），也补空格
        // 或者当前是非中文，上一个也是中文，也补空格
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
    // 简单判断常用汉字区间
    (c >= '\u{4e00}' && c <= '\u{9fa5}')
        || (c >= '\u{3400}' && c <= '\u{4dbf}')
        || (c >= '\u{20000}' && c <= '\u{2a6df}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_chinese() {
        assert_eq!(tokenize_chinese("亚丝娜"), "亚 丝 娜");
        assert_eq!(tokenize_chinese("Hello亚丝娜"), "Hello 亚 丝 娜");
        assert_eq!(tokenize_chinese("亚丝娜is back"), "亚 丝 娜 is back");
        assert_eq!(tokenize_chinese("你好 世界"), "你 好 世界");
    }
}
