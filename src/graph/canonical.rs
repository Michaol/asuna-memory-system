//! Entity identity normalization for the graph layer.
//!
//! canonical 化字符串：lowercase + trim + 把连续空白折叠为单个空格。
//! 这是实体身份的唯一逻辑：断言层与派生层（见 mod.rs 作者分层）共用，
//! server 不做 fuzzy 匹配或语义合并。

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
pub fn canonicalize(s: &str) -> String {
    s.trim()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[path = "canonical_tests.rs"]
mod tests;
