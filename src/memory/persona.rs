//! L3 Persona: user profile generation from L2 scenarios
//!
//! Pipeline:
//! 1. Aggregate all L2 scenarios
//! 2. LLM generates comprehensive user persona
//! 3. Store persona.md in memory/ as the human-readable mirror
//!
//! S14a (U1): `load_persona` / `extract_section` were deleted — the
//! frontmatter `save_persona` writes never round-tripped through them (3 keys
//! vs 8 required fields, `supersedes_id: {:?}` emitted the string "None"),
//! they had zero callers, and the `bounded_memory` target='user' row is the
//! only programmatic read surface (`/recall` L3 and `/persona` read the DB;
//! `/persona` also serves persona.md as raw text on its legacy path).
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

/// YAML frontmatter for the human-readable `persona.md` mirror (S14a U1:
/// serialized through serde_yaml — never hand-formatted — so `supersedes_id`
/// emits a real `null` instead of the Debug string "None"). There is no
/// code-side reader; see the module docs for the single read surface.
#[derive(Serialize)]
struct PersonaFrontmatter {
    created_at: i64,
    updated_at: i64,
    supersedes_id: Option<i64>,
}

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

        let system = "You are a user profiling system. Given a list of scenario summaries, create a comprehensive user persona.

Return JSON format:
{
  \"preferences\": \"User likes/dislikes (2-3 sentences)\",
  \"identity\": \"User role and background (1-2 sentences)\",
  \"workflow\": \"Typical work patterns (2-3 sentences)\",
  \"tech_stack\": \"Technologies and tools used (1-2 sentences)\",
  \"communication_style\": \"How user communicates (1 sentence)\"
}";

        let user = format!("Scenario summaries:\n{}", scenario_summaries.join("\n"));

        let response: PersonaResponse = self.llm.chat_json(system, &user)?;

        let now = crate::util::time::now_unix_ms();

        Ok(Persona {
            preferences: response.preferences,
            identity: response.identity,
            workflow: response.workflow,
            tech_stack: response.tech_stack,
            communication_style: response.communication_style,
            created_at: now,
            updated_at: now,
            supersedes_id: None,
        })
    }

    /// Save persona to Markdown file (human-readable mirror — see module
    /// docs; the DB row is the read surface).
    pub fn save_persona(&self, persona: &Persona) -> anyhow::Result<PathBuf> {
        let path = self.memory_dir.join("persona.md");

        let frontmatter = serde_yaml::to_string(&PersonaFrontmatter {
            created_at: persona.created_at,
            updated_at: persona.updated_at,
            supersedes_id: persona.supersedes_id,
        })?;

        // 不可 trim_end frontmatter：serde_yaml 输出以 '\n' 结尾，粘上闭合
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

        std::fs::write(&path, content)?;
        Ok(path)
    }
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
mod tests {
    use super::*;

    /// U1: the mirror's frontmatter must be valid YAML, `supersedes_id:
    /// None` must serialize as a real YAML null (the old `{:?}` emitted the
    /// string "None"), and the body sections must stay human-readable.
    #[test]
    fn test_save_persona_writes_valid_yaml_frontmatter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let llm = LlmClient::new("test", "test", "test");
        let generator = PersonaGenerator::new(&llm, tmp.path());

        let persona = Persona {
            preferences: "喜欢 Rust: 所有权系统".to_string(),
            identity: "开发者".to_string(),
            workflow: "白天编码".to_string(),
            tech_stack: "Rust, SQLite".to_string(),
            communication_style: "简洁".to_string(),
            created_at: 1000,
            updated_at: 2000,
            supersedes_id: None,
        };
        let path = generator.save_persona(&persona).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        // 闭合分隔符必须独占一行（防 trim_end 粘连回归，同 scenario.rs）
        assert!(content.starts_with("---\n"));
        assert!(
            content[4..].contains("\n---\n"),
            "closing --- must start its own line: {:?}",
            content
        );
        let parts: Vec<&str> = content.splitn(3, "---").collect();
        assert_eq!(parts.len(), 3);
        let fm: serde_yaml::Value =
            serde_yaml::from_str(parts[1].trim()).expect("frontmatter must be valid YAML");
        assert_eq!(fm["created_at"].as_i64().unwrap(), 1000);
        assert_eq!(fm["updated_at"].as_i64().unwrap(), 2000);
        assert!(fm["supersedes_id"].is_null(), "None must be YAML null");
        // Some(id) round-trips as a number, not a Debug string.
        let content2 = std::fs::read_to_string(
            generator
                .save_persona(&Persona {
                    supersedes_id: Some(42),
                    ..persona
                })
                .unwrap(),
        )
        .unwrap();
        let fm2: serde_yaml::Value =
            serde_yaml::from_str(content2.split("---").nth(1).unwrap().trim()).unwrap();
        assert_eq!(fm2["supersedes_id"].as_i64().unwrap(), 42);
        assert!(parts[2].contains("## Preferences"));
        assert!(parts[2].contains("喜欢 Rust: 所有权系统"));
    }
}
