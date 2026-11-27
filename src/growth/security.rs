/// 安全扫描结果
pub struct ScanResult {
    issues: Vec<String>,
}

impl ScanResult {
    pub fn is_safe(&self) -> bool {
        self.issues.is_empty()
    }

    pub fn reason(&self) -> String {
        self.issues.join("; ")
    }
}

/// Prompt injection 模式（小写、规范空白形式）
const INJECTION_PATTERNS: &[&str] = &[
    "ignore previous instructions",
    "ignore all instructions",
    "disregard previous",
    "forget your instructions",
    "you are now",
    "new instruction:",
    "ignore above",
    "ignore the above",
    "do not follow",
    "忽略之前的指令",
    "无视之前",
    "忽略以上",
    "system prompt override",
];

/// 凭据格式正则
const CREDENTIAL_PATTERNS: &[&str] = &[
    r"sk-[a-zA-Z0-9_-]{20,}",                // OpenAI (含 sk-proj- 等新格式)
    r"gh[pousr]_[a-zA-Z0-9]{36,}",           // GitHub tokens (PAT/OAuth/App/Refresh)
    r"github_pat_[0-9a-zA-Z_]{22,}",         // GitHub fine-grained PAT
    r"(?:AKIA|ASIA)[A-Z0-9]{16}",            // AWS Access Key / STS Key ID
    r"AIza[0-9A-Za-z_-]{35}",                // Google API Key
    r"xox[bpsa]-[a-zA-Z0-9-]{10,}",          // Slack tokens
    r"glpat-[a-zA-Z0-9_-]{20,}",             // GitLab PAT
    r"hf_[a-zA-Z0-9]{20,}",                  // Hugging Face token
    r"(?i)bearer\s+[a-zA-Z0-9._~+/=-]{20,}", // Authorization: Bearer <token>
    r"(?i)-----BEGIN (?:[A-Z0-9 ]* )?PRIVATE KEY(?: BLOCK)?-----", // PEM / OPENSSH / ENCRYPTED / PGP private keys
];

/// 预编译并缓存凭据检测正则，避免每次 scan 都重新编译
fn credential_regexes() -> &'static [regex_lite::Regex] {
    static CACHE: std::sync::OnceLock<Vec<regex_lite::Regex>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        CREDENTIAL_PATTERNS
            .iter()
            .map(|p| regex_lite::Regex::new(p).expect("valid static credential regex"))
            .collect()
    })
}

/// 完整 PEM/PGP 私钥块（C1）：BEGIN 头 → 对应 END 尾，未闭合时一路吃到文本末尾。
/// 只替头行会把 Base64 私钥主体留在 audit_log.detail 里。检测仍用
/// CREDENTIAL_PATTERNS 的头行正则（命中即拒），这里只服务脱敏。
fn pem_block_regex() -> &'static regex_lite::Regex {
    static CACHE: std::sync::OnceLock<regex_lite::Regex> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        regex_lite::Regex::new(
            r"(?i)(?s)-----BEGIN (?:[A-Z0-9 ]* )?PRIVATE KEY(?: BLOCK)?-----.*?(?:-----END (?:[A-Z0-9 ]* )?PRIVATE KEY(?: BLOCK)?-----|\z)",
        )
        .expect("valid static PEM block regex")
    })
}

fn is_dangerous_invisible_or_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{200B}' // Zero Width Space
            | '\u{200C}' // Zero Width Non-Joiner
            | '\u{200D}' // Zero Width Joiner
            | '\u{FEFF}' // BOM / Zero Width No-Break Space
            | '\u{2060}' // Word Joiner
            | '\u{180E}' // Mongolian Vowel Separator
            | '\u{00AD}' // Soft Hyphen
            | '\u{034F}' // Combining Grapheme Joiner
            | '\u{061C}' // Arabic Letter Mark
            | '\u{200E}' // Left-to-Right Mark
            | '\u{200F}' // Right-to-Left Mark
            | '\u{202A}'..='\u{202E}' // BiDi embeddings & overrides
            | '\u{2061}'..='\u{2064}' // Invisible math operators
            | '\u{2066}'..='\u{2069}' // BiDi isolates
            | '\u{206A}'..='\u{206F}' // Deprecated format characters
            | '\u{FFF9}'..='\u{FFFB}' // Interlinear annotation anchors
            | '\u{E0000}'..='\u{E007F}' // Unicode Tags block
            | '\x00'..='\x08' // C0 controls (excluding \t \n \r)
            | '\x0B'
            | '\x0C'
            | '\x0E'..='\x1F'
            | '\x7F' // DEL
            | '\u{0080}'..='\u{009F}' // C1 controls
    )
}

/// W7/F3: 组合附加符与真正不可见的填充符（规范化时剥离，避免混淆变体绕过匹配）。
/// 覆盖主组合区/扩展区/补充区/符号组合区/半标记区，以及脚本专用组合符
/// （西里尔/希伯来/阿拉伯/天城文/泰文/藏文/缅甸文）、变体选择符与 Hangul 填充符。
fn is_combining_mark(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036F}' // Combining Diacritical Marks
            | '\u{0483}'..='\u{0489}' // Cyrillic combining
            | '\u{0591}'..='\u{05BD}' // Hebrew points
            | '\u{0610}'..='\u{061A}' // Arabic signs
            | '\u{064B}'..='\u{065F}' // Arabic harakat
            | '\u{0670}' // Arabic superscript alef
            | '\u{06D6}'..='\u{06DC}' // Arabic small marks
            | '\u{093C}' // Devanagari nukta
            | '\u{0E31}' // Thai mai han-akat
            | '\u{0E34}'..='\u{0E3A}' // Thai vowels/phinthu
            | '\u{0F71}'..='\u{0F84}' // Tibetan vowel signs
            | '\u{102B}'..='\u{103E}' // Myanmar vowels/medials
            | '\u{1AB0}'..='\u{1AFF}' // Combining Diacritical Marks Extended
            | '\u{1DC0}'..='\u{1DFF}' // Combining Diacritical Marks Supplement
            | '\u{20D0}'..='\u{20F0}' // Combining Marks for Symbols
            | '\u{302A}'..='\u{302F}' // CJK tone marks
            | '\u{A66F}' // Combining Cyrillic vzmet
            | '\u{FE00}'..='\u{FE0F}' // Variation Selectors
            | '\u{FE20}'..='\u{FE2F}' // Combining Half Marks
            | '\u{E0100}'..='\u{E01EF}' // Variation Selectors Supplement
            | '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}' // Hangul fillers
    )
}

/// F4: 凭据**检测与脱敏共用**的规范化管线——剥不可见/控制字符与组合符，再折全角。
/// 两条路径必须同口径：此前注入侧剥组合符而凭据侧不剥，`sk-` + U+0301 + key
/// 既不检测也不脱敏，原样落进 append-only 且无保留策略的 audit_log.detail。
fn strip_for_credentials(text: &str) -> String {
    text.chars()
        .filter(|&c| !is_dangerous_invisible_or_control(c) && !is_combining_mark(c))
        .map(fold_fullwidth)
        .map(|c| {
            if c.is_whitespace() && !matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn fold_fullwidth(ch: char) -> char {
    match ch {
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(ch as u32 - 0xFEE0).unwrap_or(ch),
        '\u{3000}' => ' ',
        _ => ch,
    }
}

fn push_collapsed(out: &mut String, prev_sep: &mut bool, ch: char, is_sep: bool) {
    if is_sep {
        if !*prev_sep {
            out.push(' ');
            *prev_sep = true;
        }
    } else {
        out.push(ch);
        *prev_sep = false;
    }
}

/// Strip dangerous invisible/control characters, fold fullwidth ASCII to ASCII,
/// lowercase, and produce:
/// - `norm_ws`: consecutive whitespace collapsed to a single ASCII space
/// - `norm_punct`: ASCII separator punctuation (`._-/*|,;~:()[]{}`) also folded to spaces
/// - `norm_compact`: whitespace AND punctuation removed entirely (W27a CJK form)
fn normalize_for_injection(text: &str) -> (String, String, String) {
    let mut norm_ws = String::with_capacity(text.len());
    let mut norm_punct = String::with_capacity(text.len());
    let mut norm_compact = String::with_capacity(text.len());
    let mut prev_ws = true;
    let mut prev_punct = true;

    for ch in text.chars() {
        if is_dangerous_invisible_or_control(ch) {
            continue;
        }
        for lower in fold_fullwidth(ch).to_lowercase() {
            // W7: 组合附加符必须剥掉——`to_lowercase('İ')` 展开为 'i' + U+0307，
            // 该组合点存活进规范化串会让 "İgnore previous instructions" 绕过注入
            // 硬门禁（输入侧直接写组合符同理）。注意只在**注入匹配**的规范化串里
            // 剥，不加进 is_dangerous_invisible_or_control：那个集合同时驱动
            // "检测到不可见/控制字符" 的拒绝判定，会误拒越南语/印地语等正常文本。
            if is_combining_mark(lower) {
                continue;
            }
            let is_ws = lower.is_whitespace();
            let is_punct = is_compact_separator(lower);
            push_collapsed(&mut norm_ws, &mut prev_ws, lower, is_ws);
            push_collapsed(&mut norm_punct, &mut prev_punct, lower, is_punct);
            if !is_compact_separator(lower) {
                norm_compact.push(lower);
            }
        }
    }

    (norm_ws, norm_punct, norm_compact)
}

/// W27a: 紧凑串要抹掉的分隔符——空白 + ASCII 标点 + CJK/全角/通用标点
/// （。、：；！？…—·《》 等）。全角 ：；！？， 已被 fold_fullwidth 折成
/// ASCII 标点，U+3002 。/ U+3001 、 不在折叠区，必须由 CJK 标点区覆盖。
fn is_compact_separator(ch: char) -> bool {
    ch.is_whitespace()
        || ch.is_ascii_punctuation()
        || matches!(
            ch,
            '\u{00B7}' // · middle dot
                | '\u{2010}'..='\u{2027}' // hyphens / dashes / quotes / †‡•…
                | '\u{2030}'..='\u{205E}' // ‰′″‹›※⁎ …
                | '\u{2E00}'..='\u{2E7F}' // supplemental punctuation
                | '\u{3000}'..='\u{303F}' // CJK symbols & punctuation (。、《》〜)
                | '\u{FE10}'..='\u{FE19}' // vertical forms
                | '\u{FE30}'..='\u{FE4F}' // CJK compatibility forms
                | '\u{FF61}'..='\u{FF64}' // halfwidth CJK punctuation
        )
}

/// CJK 注入模式（无空格）才用紧凑串匹配：英文模式含空格，紧凑串永不命中，
/// 且抹标点会让英文词粘连产生误报。
fn is_cjk_pattern(pattern: &str) -> bool {
    pattern
        .chars()
        .any(|c| ('\u{4E00}'..='\u{9FFF}').contains(&c))
}

/// Replace all detected credentials in `text` with `[REDACTED]` for safe
/// logging and audit persistence, stripping zero-width/invisible obfuscation first.
pub fn redact_credentials(text: &str) -> String {
    // 维度4: 先剥零宽/控制字符，再 fold_fullwidth——否则全角混淆凭据（ｓｋ－…）
    // 折不成 ASCII，凭据正则永不命中，原文落进 audit_log.detail。fold 只动
    // 全角 ASCII 区（FF01-FF5E）与表意空格，CJK 正文不受影响。
    let mut out: String = strip_for_credentials(text);
    // 先整块替换 PEM（含主体），再跑逐凭据正则——否则头行正则只吃掉头一行。
    let pem = pem_block_regex();
    if pem.is_match(&out) {
        out = pem.replace_all(&out, "[REDACTED]").into_owned();
    }
    for re in credential_regexes() {
        if re.is_match(&out) {
            out = re.replace_all(&out, "[REDACTED]").into_owned();
        }
    }
    out
}

/// 扫描内容安全。
///
/// C10 取舍（安全覆盖 > 性能，有意不做快速预筛早退）：本函数对大 payload
/// （offload 上界受网关 10MB 限流约束）做多遍规范化 + 正则。曾评估"不含任何
/// 触发字符则早退"的预筛，但可靠早退须排除全部触发类的并集——ASCII 字母数字
/// （注入+凭据）、CJK（中文注入）、全角 ASCII FF01-FF5E（fold）、所有
/// to_lowercase 会产出 ASCII 的字符（如 U+212A KELVIN→k、U+0130→i+组合点）、
/// 以及不可见/控制字符。真实对话文本几乎必含其一，预筛基本不触发，反而自身
/// 多一遍 O(n) 纯开销；字节级手写触发类预筛更易漏掉混淆变体（规范化存在的
/// 意义正是抓全角/零宽/标点分隔绕过）。故接受现状，不为性能削弱安全覆盖。
/// 既有缓解已压成本：三套规范化串在 normalize_for_injection 单遍产出；凭据
/// 正则 OnceLock 预编译缓存 + 字面前缀加速；10MB 上界兜底最坏开销。
pub fn scan_content(text: &str) -> ScanResult {
    let mut issues = Vec::new();
    let (norm_ws, norm_punct, norm_compact) = normalize_for_injection(text);

    // 1. Prompt injection 检测（在去混淆规范化文本上匹配）
    for pattern in INJECTION_PATTERNS {
        if norm_ws.contains(pattern)
            || norm_punct.contains(pattern)
            || (is_cjk_pattern(pattern) && norm_compact.contains(pattern))
        {
            issues.push(format!("疑似 prompt injection: '{}'", pattern));
        }
    }

    // 2. 凭据格式检测（剥离零宽字符 + fold_fullwidth 后使用预编译正则，防插字
    //    与全角混淆绕过；与 redact_credentials 同口径，检测/脱敏一致）
    let stripped_for_creds = strip_for_credentials(text);
    for (i, re) in credential_regexes().iter().enumerate() {
        if re.is_match(&stripped_for_creds) {
            issues.push(format!("疑似凭据泄露: 匹配 '{}'", CREDENTIAL_PATTERNS[i]));
        }
    }

    // 3. 不可见 / 控制 Unicode 字符检测（去重）
    let mut seen_chars = std::collections::BTreeSet::new();
    for ch in text.chars() {
        if is_dangerous_invisible_or_control(ch) && seen_chars.insert(ch as u32) {
            issues.push(format!(
                "检测到不可见/控制 Unicode 字符: U+{:04X}",
                ch as u32
            ));
        }
    }

    ScanResult { issues }
}

/// 对一组带标签的字段逐个跑 scan_content，返回第一个不安全字段的拒绝原因。
/// HTTP `/graph/assert` 与 MCP `graph_assert` 共用，保证双入口拒绝语义一致。
pub fn scan_fields(fields: &[(&str, &str)]) -> Result<(), String> {
    for (field, value) in fields {
        let scan = scan_content(value);
        if !scan.is_safe() {
            return Err(format!(
                "{} rejected by security scan: {}",
                field,
                scan.reason()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "security_tests.rs"]
mod tests;
