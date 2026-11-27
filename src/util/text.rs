/// [Deprecated] 简单的中文分词处理：在汉字之间插入空格，使 FTS5 (unicode61) 能够正确索引和匹配。
///
/// 已被 jieba FTS5 tokenizer 替代（v2.4.0）。保留用于 `tokenize_zh` UDF 向后兼容。
// v4-M: 陈旧的 allow(dead_code) 已删——tokenize_chinese 仍被 db.rs 的
// register_functions（tokenize_zh UDF）在生产路径调用。
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
    ('\u{4e00}'..='\u{9fff}').contains(&c)
        || ('\u{3400}'..='\u{4dbf}').contains(&c)
        || ('\u{20000}'..='\u{2a6df}').contains(&c)
}

/// Escape SQLite LIKE wildcards (%, _, \) using \ as ESCAPE character
pub fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' | '%' | '_' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// Maximum terms preserved when constructing an FTS5 MATCH expression.
const MAX_FTS_TERMS: usize = 32;

fn is_fts_term_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '#')
}

/// 进程内唯一共享的 `Jieba` 实例：索引侧内联 FTS5 `jieba` tokenizer 与查询侧 OR 回退
/// 共用同一份词典，彻底消除双份词典内存与重复初始化开销。
/// `jieba_rs::Jieba` 满足 `Send + Sync`，`cut()` 内部经 thread-local scratch 无锁并发执行。
pub fn shared_jieba() -> &'static jieba_rs::Jieba {
    static JIEBA: std::sync::OnceLock<jieba_rs::Jieba> = std::sync::OnceLock::new();
    JIEBA.get_or_init(jieba_rs::Jieba::new)
}

fn has_cjk(s: &str) -> bool {
    // F7: 与同文件 is_chinese_char 同口径（含扩展 A/B 区），否则扩展区汉字串
    // 不被切词，W2 的非邻接问题对它们依旧存在。
    s.chars().any(is_chinese_char)
}

fn quote_term(tok: &str) -> String {
    format!("\"{}\"", tok.replace('"', "\"\""))
}

fn extract_fts_terms(query: &str) -> Vec<String> {
    query
        .split(|c: char| !is_fts_term_char(c))
        .filter(|tok| !tok.is_empty() && tok.chars().any(char::is_alphanumeric))
        .take(MAX_FTS_TERMS)
        .map(quote_term)
        .collect()
}

/// W2: OR 回退用的词条——含 CJK 的串按 jieba 切词（与索引侧 tokenizer 同口径：
/// `shared_jieba().cut(..., true)`）。
///
/// **只用于 OR 回退，不用于 AND 主查询**：索引侧默认开启停用词过滤
/// （见 `src/index/jieba_tokenizer.rs` `enable_stopword`），而 AND 主查询若切词
/// 并把停用词全 AND 起来，任何含停用词的中文查询（如"数据库的性能"）都会因
/// 停用词在索引中不存在而恒返回 0 行——比原 phrase 行为更差。OR 回退则不受影响
/// （停用词项命中 0 行不影响其余项），且让中文多词查询终于能命中非邻接文档。
fn extract_fts_or_terms(query: &str) -> Vec<String> {
    let mut groups: Vec<Vec<String>> = Vec::new();
    for tok in query.split(|c: char| !is_fts_term_char(c)) {
        if tok.is_empty() || !tok.chars().any(char::is_alphanumeric) {
            continue;
        }
        let mut group = Vec::new();
        if has_cjk(tok) {
            for t in shared_jieba().cut(tok, true) {
                let w = t.word.trim();
                if w.is_empty() || !w.chars().any(char::is_alphanumeric) {
                    continue;
                }
                group.push(w.to_lowercase());
            }
        } else {
            group.push(tok.to_string());
        }
        if !group.is_empty() {
            groups.push(group);
        }
    }

    // W28: 按 token 组轮询并大小写不敏感去重，避免首个长中文 token 占满 32 词预算。
    let mut terms = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut round = 0usize;
    while terms.len() < MAX_FTS_TERMS {
        let mut progressed = false;
        for group in &groups {
            if let Some(raw) = group.get(round) {
                progressed = true;
                if seen.insert(raw.to_lowercase()) {
                    terms.push(quote_term(raw));
                    if terms.len() >= MAX_FTS_TERMS {
                        break;
                    }
                }
            }
        }
        if !progressed {
            break;
        }
        round += 1;
    }
    terms
}

/// Build a safe multi-term FTS5 MATCH expression using implicit `AND`.
///
/// Splits the raw query into clean alphanumeric terms and double-quotes each
/// term individually, neutralizing FTS5 syntax operators (`OR`, `NOT`, `NEAR`,
/// `*`, `^`, `:`, parens) while allowing non-adjacent multi-word matching via
/// the `jieba` tokenizer. Returns `None` when the input contains no searchable
/// alphanumeric terms.
pub fn build_safe_fts_query(query: &str) -> Option<String> {
    let terms = extract_fts_terms(query);
    if terms.is_empty() {
        return None;
    }
    Some(terms.join(" "))
}

/// Build a fallback FTS5 `OR` expression for multi-term natural-language
/// queries (`>= 2` terms). Returns `None` for 0- or 1-term queries.
pub fn build_safe_fts_or_query(query: &str) -> Option<String> {
    let terms = extract_fts_or_terms(query);
    if terms.len() < 2 {
        return None;
    }
    Some(terms.join(" OR "))
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
            || ('\u{ff00}'..='\u{ffef}').contains(&c)
        // 全角/半角形
        {
            heavy += 1;
        } else {
            light += 1;
        }
    }
    heavy + light.div_ceil(3)
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
