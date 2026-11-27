use super::{L3_PERSONA_MAX_CHARS, RECALL_BANNER};
use crate::util::text::estimate_tokens;

const L3_BUDGET_PROTECT_MIN_BUDGET: usize = 200;
const L3_BUDGET_SHARE_NUM: usize = 2;
const L3_BUDGET_SHARE_DEN: usize = 5;

fn truncate_to_token_cap(text: &str, max_tokens: usize) -> String {
    if estimate_tokens(text) <= max_tokens {
        return text.to_string();
    }
    if max_tokens <= 1 {
        return "…".to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        let mut candidate: String = chars[..mid].iter().collect();
        candidate.push('…');
        if estimate_tokens(&candidate) <= max_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let mut out: String = chars[..lo].iter().collect();
    out.push('…');
    out
}

pub(super) fn cap_persona_content(content: String, budget: Option<usize>) -> String {
    let mut out = if content.chars().count() > L3_PERSONA_MAX_CHARS {
        let mut s: String = content.chars().take(L3_PERSONA_MAX_CHARS - 1).collect();
        s.push('…');
        s
    } else {
        content
    };
    if let Some(b) = budget.filter(|&b| b >= L3_BUDGET_PROTECT_MIN_BUDGET) {
        // W2.1: saturating_mul 防 b∈[2^63,2^64) 时 b*2 回绕（release 静默截到 1 token /
        // debug panic）；handler 侧另有 max_tokens 上界帽，此处为纵深防御。
        let max_tokens = (b.saturating_mul(L3_BUDGET_SHARE_NUM) / L3_BUDGET_SHARE_DEN).max(1);
        out = truncate_to_token_cap(&out, max_tokens);
    }
    out
}

/// v2.6 token budget: greedy prefix cut in layer order. The first item that
/// does not fit is dropped whole (never truncated); returns whether anything
/// was dropped. (Hindsight _filter_by_token_budget parity.)
pub(super) fn apply_token_budget(memories: &mut Vec<serde_json::Value>, budget: usize) -> bool {
    let mut used = 0usize;
    let mut keep = memories.len();
    for (i, m) in memories.iter().enumerate() {
        let est = m
            .get("content")
            .and_then(serde_json::Value::as_str)
            .map(estimate_tokens)
            .unwrap_or(0);
        if used + est > budget {
            keep = i;
            break;
        }
        used += est;
    }
    let truncated = keep < memories.len();
    memories.truncate(keep);
    truncated
}

/// Rebuild the context string from surviving memories. L0 turns are excluded
/// (same as v2.5.3). The untrusted-data banner is always the first line.
pub(super) fn rebuild_context(memories: &[serde_json::Value]) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(memories.len() + 1);
    lines.push(RECALL_BANNER.to_string());
    lines.extend(memories.iter().filter_map(|m| {
        let layer = m.get("layer")?.as_str()?;
        let content = m.get("content")?.as_str()?;
        Some(match layer {
            "L3" => format!("[Persona] {}", content),
            // S14c: layer labels for the consolidation documents; the item
            // content already leads with the document title, so the type
            // field distinguishes siblings here without a longer label.
            "L4" => format!("[MentalModel] {}", content),
            "L5" => format!("[Intent] {}", content),
            "L2" => format!("[Scenario] {}", content),
            "L1" => format!(
                "[{}] {}",
                m.get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("atom"),
                content
            ),
            _ => return None,
        })
    }));
    lines.join("\n")
}
