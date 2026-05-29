//! L3 Persona: user profile generation from L2 scenarios
//!
//! Pipeline:
//! 1. Aggregate all L2 scenarios
//! 2. LLM generates comprehensive user persona
//! 3. Store as persona.md in memory/
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
            return Err(anyhow::anyhow!("No scenarios available for persona generation"));
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

    /// Save persona to Markdown file
    pub fn save_persona(&self, persona: &Persona) -> anyhow::Result<PathBuf> {
        let path = self.memory_dir.join("persona.md");

        let content = format!(
            r#"---
created_at: {}
updated_at: {}
supersedes_id: {:?}
---

# User Persona

## Preferences
{}

## Identity
{}

## Workflow
{}

## Tech Stack
{}

## Communication Style
{}
"#,
            persona.created_at,
            persona.updated_at,
            persona.supersedes_id,
            persona.preferences,
            persona.identity,
            persona.workflow,
            persona.tech_stack,
            persona.communication_style
        );

        std::fs::write(&path, content)?;
        Ok(path)
    }

    /// Load persona from file
    pub fn load_persona(&self) -> anyhow::Result<Option<Persona>> {
        let path = self.memory_dir.join("persona.md");

        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)?;

        // Parse YAML frontmatter
        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Ok(None);
        }

        let frontmatter = parts[1].trim();
        let body = parts[2];

        let mut persona: Persona = serde_yaml::from_str(frontmatter)?;

        // Parse body sections
        persona.preferences = extract_section(body, "## Preferences").unwrap_or_default();
        persona.identity = extract_section(body, "## Identity").unwrap_or_default();
        persona.workflow = extract_section(body, "## Workflow").unwrap_or_default();
        persona.tech_stack = extract_section(body, "## Tech Stack").unwrap_or_default();
        persona.communication_style =
            extract_section(body, "## Communication Style").unwrap_or_default();

        Ok(Some(persona))
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

/// Extract section content from Markdown
fn extract_section(content: &str, header: &str) -> Option<String> {
    let start = content.find(header)?;
    let after_header = &content[start + header.len()..];

    // Find next header or end of file
    let end = after_header
        .find("\n## ")
        .unwrap_or(after_header.len());

    Some(after_header[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_section() {
        let content = r#"
## Preferences
User likes Rust and Python.

## Identity
Software developer.

## Workflow
Works on weekdays.
"#;

        assert_eq!(
            extract_section(content, "## Preferences"),
            Some("User likes Rust and Python.".to_string())
        );
        assert_eq!(
            extract_section(content, "## Identity"),
            Some("Software developer.".to_string())
        );
    }

    #[test]
    fn test_extract_section_not_found() {
        let content = "## Other\nContent";
        assert_eq!(extract_section(content, "## Missing"), None);
    }
}
