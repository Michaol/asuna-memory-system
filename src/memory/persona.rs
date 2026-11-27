//! L3 Persona: user profile generation from L2 scenarios
//!
//! S14b wiring: pipeline Phase 4b (the L3 step of `run_consolidation`,
//! `service/pipeline/consolidation.rs`)
//! regenerates this file when at least `persona.trigger_every_n` sessions
//! were touched since the last write (0 = disabled), feeding the newest
//! scenario rows to [`PersonaGenerator::generate`]. The output is a PURE
//! file surface: persona never writes a `bounded_memory` target='user' row
//! (the user face belongs to the manual-entry mechanism with its USER.md
//! reconcile + user_char_limit invariants — S14b design decision).
//!
//! Read surfaces: `/recall` L3 falls back DB user row → persona.md →
//! USER.md (`recall_persona`, `memory/retrieval.rs`); the `/persona`
//! endpoint serves USER.md → persona.md → DB (`transport/http/ops_handlers.rs`). Both
//! situate persona.md between the two manual heads; only the manual-head
//! order differs by design.
//!
//! S14a (U1) history: `load_persona` / `extract_section` were deleted —
//! the frontmatter `save_persona` writes never round-tripped through them
//! (3 keys vs 8 required fields, `supersedes_id: {:?}` emitted the string
//! "None") and they had zero callers. The frontmatter `updated_at` gained
//! a reader in S14b (`last_persona_ts`), best-effort, for the trigger.
//!
//! Persona contains:
//! - Preferences (likes/dislikes)
//! - Identity (role, background)
//! - Workflow patterns
//! - Technical stack
//! - Communication style

use crate::memory::llm::LlmClient;
use crate::memory::scenario::Scenario;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// L3 User persona
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    pub preferences: String,
    pub identity: String,
    pub workflow: String,
    pub tech_stack: String,
    pub communication_style: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub supersedes_id: Option<i64>,
}

/// L3 Persona generator
pub struct PersonaGenerator<'a> {
    llm: &'a LlmClient,
    memory_dir: PathBuf,
}

// YAML frontmatter for the human-readable `persona.md` mirror (S14a U1:
// encoded via `util::frontmatter` — never hand-concatenated — so `supersedes_id`
// emits a real `null` instead of the Debug string "None"). The only
// code-side reader is the pipeline trigger's best-effort `updated_at`
// parse (`last_persona_ts`, `service/pipeline/consolidation.rs`); the body stays
// for humans.

impl<'a> PersonaGenerator<'a> {
    pub fn new(llm: &'a LlmClient, memory_dir: &Path) -> Self {
        Self {
            llm,
            memory_dir: memory_dir.to_path_buf(),
        }
    }

    /// Generate persona from scenarios
    pub fn generate(&self, scenarios: &[Scenario]) -> anyhow::Result<Persona> {
        if scenarios.is_empty() {
            return Err(anyhow::anyhow!(
                "No scenarios available for persona generation"
            ));
        }

        let scenario_summaries: Vec<String> = scenarios
            .iter()
            .map(|s| format!("- {}: {}", s.title, s.summary))
            .collect();

        let system = format!(
            "You are a user profiling system. Given a list of scenario summaries inside <user_data>, create a comprehensive user persona. {}

Return JSON format:
{{
  \"preferences\": \"User likes/dislikes (2-3 sentences)\",
  \"identity\": \"User role and background (1-2 sentences)\",
  \"workflow\": \"Typical work patterns (2-3 sentences)\",
  \"tech_stack\": \"Technologies and tools used (1-2 sentences)\",
  \"communication_style\": \"How user communicates (1 sentence)\"
}}",
            crate::memory::UNTRUSTED_DATA_GUARD
        );

        let wrapped = crate::memory::wrap_untrusted_data(
            "user_data",
            &format!("Scenario summaries:\n{}", scenario_summaries.join("\n")),
        );

        let response: PersonaResponse = self.llm.chat_json(&system, &wrapped)?;

        let preferences = response.preferences.trim().to_string();
        let identity = response.identity.trim().to_string();
        let workflow = response.workflow.trim().to_string();
        let tech_stack = response.tech_stack.trim().to_string();
        let communication_style = response.communication_style.trim().to_string();
        if preferences.is_empty()
            && identity.is_empty()
            && workflow.is_empty()
            && tech_stack.is_empty()
            && communication_style.is_empty()
        {
            anyhow::bail!("LLM returned empty persona across all five sections");
        }

        let now = crate::util::time::now_unix_ms();

        // W13: 部分空白的响应不得销毁既有富内容——空白节回填上一版 persona.md 的
        // 对应节（无旧文件时保持空白）。全空白仍在上面 bail（W3 契约不变）。
        let prior = prior_persona_sections(&self.memory_dir.join("persona.md"));
        let fill = |value: String, section: &str| -> String {
            if value.is_empty() {
                prior.get(section).cloned().unwrap_or_default()
            } else {
                value
            }
        };
        let preferences = fill(preferences, "Preferences");
        let identity = fill(identity, "Identity");
        let workflow = fill(workflow, "Workflow");
        let tech_stack = fill(tech_stack, "Tech Stack");
        let communication_style = fill(communication_style, "Communication Style");

        Ok(Persona {
            preferences,
            identity,
            workflow,
            tech_stack,
            communication_style,
            // S14a 删掉 load_persona 后无从读回原始创建时间：每次生成都把
            // created_at 与 updated_at 一起戳成 now（两者恒等，不是笔误）。
            // 唯一读者是 pipeline 触发器的 best-effort updated_at 解析。
            created_at: now,
            updated_at: now,
            supersedes_id: None,
        })
    }

    /// Save persona to Markdown file. Since S14b this file IS a programmatic
    /// read surface (recall L3 fallback after the DB user row, /persona
    /// priority 2) — not merely a human-readable mirror; see module docs.
    pub fn save_persona(&self, persona: &Persona) -> anyhow::Result<PathBuf> {
        let path = self.memory_dir.join("persona.md");

        let frontmatter = crate::util::frontmatter::format_persona(
            persona.created_at,
            persona.updated_at,
            persona.supersedes_id,
        );

        // 不可 trim_end frontmatter：frontmatter 输出以 '\n' 结尾，粘上闭合
        // "---" 会破坏 frontmatter 结构（同 scenario.rs 的说明）。
        let content = format!(
            "---\n{}---\n\n# User Persona\n\n## Preferences\n{}\n\n## Identity\n{}\n\n## Workflow\n{}\n\n## Tech Stack\n{}\n\n## Communication Style\n{}\n",
            frontmatter,
            persona.preferences,
            persona.identity,
            persona.workflow,
            persona.tech_stack,
            persona.communication_style
        );

        crate::util::atomic_write(&path, content)?;
        Ok(path)
    }
}

/// W13: 读回既有 persona.md 的各节正文（`## <标题>` → 正文），供本次生成的空白节
/// 回填。文件缺失/不可读返回空表（首次生成，无内容可保护）。
fn prior_persona_sections(path: &std::path::Path) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return out;
    };
    // W16: 先把 CRLF / lone CR 归一成 LF——Windows 行尾下旧 split_once("\n---\n")
    // 命不中闭合 fence（frontmatter 漏进正文扫描），纯 \r 文件 lines() 根本不分行。
    let content = raw.replace("\r\n", "\n").replace('\r', "\n");
    // 跳过 frontmatter（首个闭合 `---` 行之后才是正文）
    // F6: 只剥**开头**的 frontmatter 块——按行找闭合 fence（归一后 CRLF 安全），
    // 不会命中文档中任意位置的 markdown 水平线（无 frontmatter 的手工编辑文件）。
    let body = strip_leading_frontmatter(&content);
    let mut current: Option<String> = None;
    let mut buf = String::new();
    for line in body.lines() {
        // F2: 只有五个已知节标题才算节边界——正文里出现 "## " 行（LLM 输出原样
        // 写入）此前会被当成新节，导致回填时只恢复半截内容、其余永久丢失。
        let header = line.strip_prefix("## ").map(str::trim);
        if let Some(h) = header.filter(|h| {
            matches!(
                *h,
                "Preferences" | "Identity" | "Workflow" | "Tech Stack" | "Communication Style"
            )
        }) {
            if let Some(key) = current.take() {
                out.insert(key, buf.trim().to_string());
            }
            current = Some(h.to_string());
            buf.clear();
        } else if current.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(key) = current {
        out.insert(key, buf.trim().to_string());
    }
    out
}

/// 剥离开头的 frontmatter 块：首行必须是 `---`，闭合 fence 按行匹配
/// （容忍行尾水平空白）。无闭合 fence 时整段视为正文（与旧 split_once
/// 落空回退同口径）。调用方须先归一行尾。
fn strip_leading_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\n', ' ', '\t']) == "---" {
            return &rest[offset + line.len()..];
        }
        offset += line.len();
    }
    content
}

/// W16: L4 整合输入用的 persona.md 镜像读回（`run_consolidation` 在 L4 due
/// 但本轮无新 persona 时调用）。节正文走 `prior_persona_sections`（CRLF 安全），
/// 时间戳 best-effort 取 frontmatter（L4 build_context 只消费四节正文，时间戳
/// 仅保持结构完整）。文件缺失/不可读/五节全空 → `None`。
pub(crate) fn load_persona_mirror(memory_dir: &Path) -> Option<Persona> {
    let path = memory_dir.join("persona.md");
    let sections = prior_persona_sections(&path);
    let get = |key: &str| sections.get(key).cloned().unwrap_or_default();
    let preferences = get("Preferences");
    let identity = get("Identity");
    let workflow = get("Workflow");
    let tech_stack = get("Tech Stack");
    let communication_style = get("Communication Style");
    if preferences.is_empty()
        && identity.is_empty()
        && workflow.is_empty()
        && tech_stack.is_empty()
        && communication_style.is_empty()
    {
        return None;
    }
    let (created_at, updated_at) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| {
            let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
            let fm = crate::util::frontmatter::extract_block(&normalized)?;
            Some((
                crate::util::frontmatter::parse_i64(fm, "created_at").unwrap_or(0),
                crate::util::frontmatter::parse_i64(fm, "updated_at").unwrap_or(0),
            ))
        })
        .unwrap_or((0, 0));
    Some(Persona {
        preferences,
        identity,
        workflow,
        tech_stack,
        communication_style,
        created_at,
        updated_at,
        supersedes_id: None,
    })
}

#[derive(Deserialize)]
struct PersonaResponse {
    preferences: String,
    identity: String,
    workflow: String,
    tech_stack: String,
    communication_style: String,
}

#[cfg(test)]
#[path = "persona_tests.rs"]
mod tests;
