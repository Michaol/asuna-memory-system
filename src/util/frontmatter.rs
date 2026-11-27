//! Zero-dependency YAML 1.2 frontmatter helpers for persona (`L3`) and
//! scenario (`L2`) Markdown mirrors.
//!
//! Replaces the archived `RUSTSEC-2024-0320` YAML dependency while keeping
//! the same strict multi-document rejection, quoting rules for colons/quotes/newlines,
//! and backward-compatible parsing for legacy YAML-written files.

/// Extract the frontmatter slice between an opening `---\n` / `---\r\n` line
/// and a standalone closing `---` line (tolerating optional leading UTF-8 BOM
/// and trailing horizontal whitespace on the opening fence).
pub fn extract_block(content: &str) -> Option<&str> {
    let trimmed = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let first_nl = trimmed.find('\n')?;
    let first_line = trimmed.get(..first_nl)?.trim_end_matches(['\r', ' ', '\t']);
    if first_line != "---" {
        return None;
    }
    let rest = trimmed.get(first_nl + 1..)?;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n', ' ', '\t']) == "---" {
            return rest.get(..offset);
        }
        offset += line.len();
    }
    None
}

/// Format `persona.md` frontmatter ending with a trailing newline.
/// M4: `supersedes_id` / `atom_ids` 是**只写**的溯源镜像键——供人工与外部工具检查
/// 演化链/聚类来源，程序自身不回读（生产读取面走 DB 的 supersedes_id /
/// source_turn_ids 列）。下方的 parse_optional_i64 / parse_i64_list 仅供往返测试
/// 校验写入形状，不是生产消费者。
pub fn format_persona(created_at: i64, updated_at: i64, supersedes_id: Option<i64>) -> String {
    let supersedes = supersedes_id.map_or_else(|| "null".to_string(), |id| id.to_string());
    format!("created_at: {created_at}\nupdated_at: {updated_at}\nsupersedes_id: {supersedes}\n")
}

/// Format L2 scenario mirror frontmatter ending with a trailing newline.
///
/// `title` is encoded as a JSON string literal (a strict subset of YAML 1.2
/// double-quoted scalars), keeping `": "`, `"# "`, quotes, and newlines valid.
pub fn format_scenario(title: &str, atom_ids: &[i64], created_at: i64, updated_at: i64) -> String {
    let quoted_title = serde_json::to_string(title).unwrap_or_else(|_| "\"\"".to_string());
    let ids = atom_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "title: {quoted_title}\natom_ids: [{ids}]\ncreated_at: {created_at}\nupdated_at: {updated_at}\n"
    )
}

/// Parse an integer scalar for top-level `key:` in a single-document frontmatter block.
pub fn parse_i64(frontmatter: &str, key: &str) -> Option<i64> {
    if has_multi_doc_marker(frontmatter) {
        return None;
    }
    let (_, raw) = find_top_level_key(frontmatter, key)?;
    strip_plain_comment(raw).parse::<i64>().ok()
}

/// Parse an optional integer scalar (`null` / `~` / empty → `Some(None)`).
#[cfg(test)]
pub fn parse_optional_i64(frontmatter: &str, key: &str) -> Option<Option<i64>> {
    if has_multi_doc_marker(frontmatter) {
        return None;
    }
    let (_, raw) = find_top_level_key(frontmatter, key)?;
    let val = strip_plain_comment(raw);
    if val.is_empty() || matches!(val, "null" | "Null" | "NULL" | "~") {
        return Some(None);
    }
    val.parse::<i64>().ok().map(Some)
}

/// Parse a string scalar for top-level `key:`, supporting double-quoted,
/// single-quoted, and safe plain YAML scalars.
pub fn parse_string(frontmatter: &str, key: &str) -> Option<String> {
    if has_multi_doc_marker(frontmatter) {
        return None;
    }
    let (_, raw) = find_top_level_key(frontmatter, key)?;
    decode_yaml_scalar(raw.trim())
}

/// Parse a list of `i64` values for top-level `key:` (flow `[1, 2]` or legacy
/// block `- 1` sequence).
#[cfg(test)]
pub fn parse_i64_list(frontmatter: &str, key: &str) -> Option<Vec<i64>> {
    if has_multi_doc_marker(frontmatter) {
        return None;
    }
    let (line_idx, raw) = find_top_level_key(frontmatter, key)?;
    let inline = strip_plain_comment(raw);
    if inline.starts_with('[') {
        return parse_flow_i64_list(inline);
    }
    if !inline.is_empty() {
        return None;
    }
    parse_block_i64_list(frontmatter, line_idx)
}

fn has_multi_doc_marker(frontmatter: &str) -> bool {
    frontmatter
        .lines()
        .any(|line| matches!(line.trim_end_matches([' ', '\t', '\r']), "---" | "..."))
}

fn find_top_level_key<'a>(frontmatter: &'a str, key: &str) -> Option<(usize, &'a str)> {
    for (idx, line) in frontmatter.lines().enumerate() {
        if line.starts_with([' ', '\t', '#']) {
            continue;
        }
        let Some(after_key) = line.strip_prefix(key) else {
            continue;
        };
        let Some(after_colon) = after_key.strip_prefix(':') else {
            continue;
        };
        if after_colon.is_empty() || after_colon.starts_with([' ', '\t']) {
            return Some((idx, after_colon));
        }
    }
    None
}

fn strip_plain_comment(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.starts_with('#') {
        return "";
    }
    if let Some(pos) = trimmed
        .as_bytes()
        .windows(2)
        .position(|w| w[1] == b'#' && matches!(w[0], b' ' | b'\t'))
    {
        return trimmed.get(..pos + 1).map_or(trimmed, str::trim_end);
    }
    trimmed
}

fn decode_yaml_scalar(raw: &str) -> Option<String> {
    if raw.starts_with('"') {
        return decode_double_quoted(raw);
    }
    if raw.starts_with('\'') {
        return decode_single_quoted(raw);
    }
    let plain = strip_plain_comment(raw);
    if plain.is_empty()
        || matches!(plain, "null" | "Null" | "NULL" | "~")
        || matches!(
            plain.as_bytes().first(),
            Some(b'|' | b'>' | b'[' | b'{' | b'&' | b'*' | b'!' | b'%' | b'@' | b'`')
        )
        || plain.contains(": ")
        || plain.contains(":\t")
        || plain.ends_with(':')
    {
        return None;
    }
    Some(plain.to_string())
}

fn is_valid_quoted_tail(raw_tail: &str) -> bool {
    raw_tail.trim().is_empty()
        || (raw_tail.starts_with([' ', '\t']) && raw_tail.trim_start().starts_with('#'))
}

fn decode_double_quoted(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = inner.char_indices();
    while let Some((idx, ch)) = chars.next() {
        if ch == '"' {
            let raw_tail = inner.get(idx + 1..)?;
            return is_valid_quoted_tail(raw_tail).then_some(out);
        }
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let (_, esc) = chars.next()?;
        match esc {
            '0' => out.push('\0'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            't' | '\t' => out.push('\t'),
            'n' => out.push('\n'),
            'v' => out.push('\x0B'),
            'f' => out.push('\x0C'),
            'r' => out.push('\r'),
            'e' => out.push('\x1B'),
            ' ' => out.push(' '),
            '"' => out.push('"'),
            '/' => out.push('/'),
            '\\' => out.push('\\'),
            'N' => out.push('\u{0085}'),
            '_' => out.push('\u{00A0}'),
            'L' => out.push('\u{2028}'),
            'P' => out.push('\u{2029}'),
            'x' | 'u' | 'U' => {
                let digits = match esc {
                    'x' => 2,
                    'u' => 4,
                    _ => 8,
                };
                let mut hex = String::with_capacity(digits);
                for _ in 0..digits {
                    let (_, h) = chars.next()?;
                    hex.push(h);
                }
                let code = u32::from_str_radix(&hex, 16).ok()?;
                let decoded = char::from_u32(code)?;
                out.push(decoded);
            }
            _ => return None,
        }
    }
    None
}

fn decode_single_quoted(raw: &str) -> Option<String> {
    let inner = raw.strip_prefix('\'')?;
    let mut out = String::new();
    let mut chars = inner.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch != '\'' {
            out.push(ch);
            continue;
        }
        if chars.peek().is_some_and(|&(_, next)| next == '\'') {
            chars.next();
            out.push('\'');
            continue;
        }
        let raw_tail = inner.get(idx + 1..)?;
        return is_valid_quoted_tail(raw_tail).then_some(out);
    }
    None
}

#[cfg(test)]
fn parse_flow_i64_list(inline: &str) -> Option<Vec<i64>> {
    let inner = inline.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|part| part.trim().parse::<i64>().ok())
        .collect()
}

#[cfg(test)]
fn parse_block_i64_list(frontmatter: &str, key_line_idx: usize) -> Option<Vec<i64>> {
    let mut out = Vec::new();
    for line in frontmatter.lines().skip(key_line_idx + 1) {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if !line.starts_with([' ', '\t']) && !t.starts_with('-') {
            break;
        }
        let item = t
            .strip_prefix("- ")
            .or_else(|| t.strip_prefix("-\t"))?
            .trim();
        out.push(strip_plain_comment(item).parse::<i64>().ok()?);
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
#[path = "frontmatter_tests.rs"]
mod tests;
