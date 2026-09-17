//! L4 Mental Model: Abstract cognitive frameworks from L2/L3
//!
//! Refreshed inside the pipeline's L3-L5 consolidation cycle (Phase 4b,
//! S14c: `run_consolidation` regenerates persona.md → these docs → L5 in
//! sequence, each step independently best-effort). Outputs structured
//! Markdown files:
//! - workflow-patterns.md: User's typical work patterns
//! - decision-framework.md: How user makes decisions
//! - communication-style.md: User's communication preferences
//!
//! The read surface is `/recall` L4 (`memory/retrieval.rs`): the free
//! `load_*_from` loaders below are deliberately LLM-free so the gateway
//! handler needs none of this module's generation machinery.

use crate::memory::llm::LlmClient;
use crate::memory::persona::Persona;
use crate::memory::scenario::Scenario;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowPatterns {
    pub patterns: Vec<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionFramework {
    pub criteria: Vec<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommunicationStyle {
    pub preferences: Vec<String>,
    pub updated_at: i64,
}

/// Write a list-style Markdown document: `# title`, an `Updated: …` line and
/// `- ` bullets. Shared by the three L4 mental models and the two L5 intent
/// documents (S14a J10: five isomorphic save/load pairs collapsed into these
/// two helpers — no public abstraction beyond crate-internal visibility).
pub(crate) fn save_list_md(
    path: &Path,
    title: &str,
    updated_at: i64,
    items: &[String],
) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let stamp = chrono::DateTime::from_timestamp(updated_at, 0)
        .unwrap_or_else(chrono::Utc::now)
        .format("%Y-%m-%d %H:%M:%S UTC");
    let bullets = items
        .iter()
        .map(|i| format!("- {}", i))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        path,
        format!("# {}\n\nUpdated: {}\n\n{}", title, stamp, bullets),
    )?;
    Ok(path.to_path_buf())
}

/// Parse a `save_list_md` document: `- ` lines become items; `updated_at`
/// comes from the `Updated: …` line (falling back to the file mtime — the L5
/// loads previously hardcoded 0, J9). `Ok(None)` when the file is absent.
pub(crate) fn load_list_md(path: &Path) -> Result<Option<(Vec<String>, i64)>> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path)?;
    let items: Vec<String> = content
        .lines()
        .filter(|line| line.starts_with("- "))
        .map(|line| line.trim_start_matches("- ").to_string())
        .collect();

    let updated_at = content
        .lines()
        .find(|line| line.starts_with("Updated: "))
        .and_then(|line| {
            let timestamp_str = line
                .trim_start_matches("Updated: ")
                .trim_end_matches(" UTC");
            chrono::NaiveDateTime::parse_from_str(timestamp_str, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|dt| dt.and_utc().timestamp())
        })
        .unwrap_or_else(|| {
            // Fallback to file modification time
            std::fs::metadata(path)
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0)
                })
                .unwrap_or(0)
        });

    Ok(Some((items, updated_at)))
}

/// L4 文档目录（`<memory_dir>/mental_models/`）——公开给 pipeline 的触发锚
/// （按目录 mtime 判断上次生成时间），避免布局字面量在两处漂移。
pub fn docs_dir(memory_dir: &Path) -> PathBuf {
    memory_dir.join("mental_models")
}

/// Single join site for the L4 document layout (`<memory_dir>/mental_models/
/// <file>.md`) — the save_* methods and the free loaders below must never
/// drift apart (S14c).
fn mental_model_path(memory_dir: &Path, file: &str) -> PathBuf {
    docs_dir(memory_dir).join(file)
}

/// Pure-file L4 loaders (S14c): the `/recall` L4 surface reads the documents
/// without constructing a [`MentalModelGenerator`] (and therefore without an
/// LLM client). `Ok(None)` when the file is absent; `updated_at` semantics
/// per [`load_list_md`] (parsed `Updated:` line → mtime fallback → 0).
pub fn load_workflow_patterns_from(memory_dir: &Path) -> Result<Option<WorkflowPatterns>> {
    Ok(
        load_list_md(&mental_model_path(memory_dir, "workflow-patterns.md"))?.map(
            |(patterns, updated_at)| WorkflowPatterns {
                patterns,
                updated_at,
            },
        ),
    )
}

pub fn load_decision_framework_from(memory_dir: &Path) -> Result<Option<DecisionFramework>> {
    Ok(
        load_list_md(&mental_model_path(memory_dir, "decision-framework.md"))?.map(
            |(criteria, updated_at)| DecisionFramework {
                criteria,
                updated_at,
            },
        ),
    )
}

pub fn load_communication_style_from(memory_dir: &Path) -> Result<Option<CommunicationStyle>> {
    Ok(
        load_list_md(&mental_model_path(memory_dir, "communication-style.md"))?.map(
            |(preferences, updated_at)| CommunicationStyle {
                preferences,
                updated_at,
            },
        ),
    )
}

pub struct MentalModelGenerator {
    llm: Arc<LlmClient>,
    memory_dir: PathBuf,
}

impl MentalModelGenerator {
    pub fn new(llm: Arc<LlmClient>, memory_dir: PathBuf) -> Self {
        Self { llm, memory_dir }
    }

    pub fn generate_workflow_patterns(
        &self,
        scenarios: &[Scenario],
        persona: Option<&Persona>,
    ) -> Result<WorkflowPatterns> {
        let context = self.build_context(scenarios, persona);

        let system = "You are a workflow analyst. Extract the user's typical work patterns from their scenarios and persona.";
        let user = format!(
            "{}\n\nExtract 3-5 key work patterns. Return ONLY a JSON array of strings, no markdown.",
            context
        );

        // J9: chat_json tolerates markdown fences / prose around the JSON.
        let patterns: Vec<String> = self.llm.chat_json(system, &user)?;

        Ok(WorkflowPatterns {
            patterns,
            updated_at: chrono::Utc::now().timestamp(),
        })
    }

    pub fn generate_decision_framework(
        &self,
        scenarios: &[Scenario],
        persona: Option<&Persona>,
    ) -> Result<DecisionFramework> {
        let context = self.build_context(scenarios, persona);

        let system = "You are a decision analyst. Extract how the user makes decisions from their scenarios and persona.";
        let user = format!(
            "{}\n\nExtract 3-5 key decision criteria. Return ONLY a JSON array of strings, no markdown.",
            context
        );

        let criteria: Vec<String> = self.llm.chat_json(system, &user)?;

        Ok(DecisionFramework {
            criteria,
            updated_at: chrono::Utc::now().timestamp(),
        })
    }

    pub fn generate_communication_style(
        &self,
        scenarios: &[Scenario],
        persona: Option<&Persona>,
    ) -> Result<CommunicationStyle> {
        let context = self.build_context(scenarios, persona);

        let system = "You are a communication analyst. Extract the user's communication preferences from their scenarios and persona.";
        let user = format!(
            "{}\n\nExtract 3-5 key communication preferences. Return ONLY a JSON array of strings, no markdown.",
            context
        );

        let preferences: Vec<String> = self.llm.chat_json(system, &user)?;

        Ok(CommunicationStyle {
            preferences,
            updated_at: chrono::Utc::now().timestamp(),
        })
    }

    pub fn save_workflow_patterns(&self, workflow: &WorkflowPatterns) -> Result<PathBuf> {
        save_list_md(
            &mental_model_path(&self.memory_dir, "workflow-patterns.md"),
            "Workflow Patterns",
            workflow.updated_at,
            &workflow.patterns,
        )
    }

    pub fn save_decision_framework(&self, framework: &DecisionFramework) -> Result<PathBuf> {
        save_list_md(
            &mental_model_path(&self.memory_dir, "decision-framework.md"),
            "Decision Framework",
            framework.updated_at,
            &framework.criteria,
        )
    }

    pub fn save_communication_style(&self, style: &CommunicationStyle) -> Result<PathBuf> {
        save_list_md(
            &mental_model_path(&self.memory_dir, "communication-style.md"),
            "Communication Style",
            style.updated_at,
            &style.preferences,
        )
    }

    pub fn load_workflow_patterns(&self) -> Result<Option<WorkflowPatterns>> {
        load_workflow_patterns_from(&self.memory_dir)
    }

    pub fn load_decision_framework(&self) -> Result<Option<DecisionFramework>> {
        load_decision_framework_from(&self.memory_dir)
    }

    pub fn load_communication_style(&self) -> Result<Option<CommunicationStyle>> {
        load_communication_style_from(&self.memory_dir)
    }

    fn build_context(&self, scenarios: &[Scenario], persona: Option<&Persona>) -> String {
        let mut context = String::new();

        if let Some(p) = persona {
            context.push_str("User Persona:\n");
            context.push_str(&format!("- Identity: {}\n", p.identity));
            context.push_str(&format!("- Workflow: {}\n", p.workflow));
            context.push_str(&format!("- Tech Stack: {}\n", p.tech_stack));
            context.push_str(&format!("- Communication: {}\n", p.communication_style));
            context.push('\n');
        }

        context.push_str("Recent Scenarios:\n");
        for scenario in scenarios.iter().take(20) {
            context.push_str(&format!("- {}: {}\n", scenario.title, scenario.summary));
        }

        context
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_workflow_patterns_serialization() {
        let workflow = WorkflowPatterns {
            patterns: vec!["Pattern 1".to_string(), "Pattern 2".to_string()],
            updated_at: 1234567890,
        };

        let json = serde_json::to_string(&workflow).unwrap();
        let deserialized: WorkflowPatterns = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.patterns.len(), 2);
        assert_eq!(deserialized.patterns[0], "Pattern 1");
    }

    #[test]
    fn test_save_and_load_workflow_patterns() {
        let temp_dir = TempDir::new().unwrap();
        let llm = Arc::new(LlmClient::new("test", "test", "test"));
        let generator = MentalModelGenerator::new(llm, temp_dir.path().to_path_buf());

        let workflow = WorkflowPatterns {
            patterns: vec!["Pattern A".to_string(), "Pattern B".to_string()],
            updated_at: chrono::Utc::now().timestamp(),
        };

        generator.save_workflow_patterns(&workflow).unwrap();

        let loaded = generator.load_workflow_patterns().unwrap().unwrap();
        assert_eq!(loaded.patterns.len(), 2);
        assert_eq!(loaded.patterns[0], "Pattern A");
        assert_eq!(loaded.patterns[1], "Pattern B");
    }

    /// J10: `updated_at` survives the render/parse round trip via the
    /// `Updated: …` line (fixed second-granularity value, so a mismatch is
    /// unambiguous — the mtime fallback would drift).
    #[test]
    fn test_list_md_roundtrips_updated_at() {
        let temp_dir = TempDir::new().unwrap();
        let llm = Arc::new(LlmClient::new("test", "test", "test"));
        let generator = MentalModelGenerator::new(llm, temp_dir.path().to_path_buf());

        let workflow = WorkflowPatterns {
            patterns: vec!["P1".to_string()],
            updated_at: 1_700_000_000,
        };
        generator.save_workflow_patterns(&workflow).unwrap();
        let loaded = generator.load_workflow_patterns().unwrap().unwrap();
        assert_eq!(loaded.updated_at, 1_700_000_000);

        let framework = DecisionFramework {
            criteria: vec!["C1".to_string()],
            updated_at: 1_600_000_000,
        };
        generator.save_decision_framework(&framework).unwrap();
        let loaded = generator.load_decision_framework().unwrap().unwrap();
        assert_eq!(loaded.updated_at, 1_600_000_000);

        let style = CommunicationStyle {
            preferences: vec!["S1".to_string()],
            updated_at: 1_500_000_000,
        };
        generator.save_communication_style(&style).unwrap();
        let loaded = generator.load_communication_style().unwrap().unwrap();
        assert_eq!(loaded.updated_at, 1_500_000_000);
    }

    /// S14c(7d): the pure-file `load_*_from` free loaders (the /recall L4
    /// read path) and the generator-bound instance loaders — which the
    /// generator's own pipeline context uses — must return exactly the same
    /// documents, present or absent.
    #[test]
    fn free_loaders_match_instance_loaders() {
        let temp_dir = TempDir::new().unwrap();
        let llm = Arc::new(LlmClient::new("test", "test", "test"));
        let generator = MentalModelGenerator::new(llm, temp_dir.path().to_path_buf());
        let dir = temp_dir.path();

        generator
            .save_workflow_patterns(&WorkflowPatterns {
                patterns: vec!["P1".to_string(), "P2".to_string()],
                updated_at: 1_700_000_000,
            })
            .unwrap();
        generator
            .save_decision_framework(&DecisionFramework {
                criteria: vec!["C1".to_string()],
                updated_at: 1_600_000_000,
            })
            .unwrap();
        // communication-style.md deliberately absent → both must agree on None.
        assert_eq!(
            load_workflow_patterns_from(dir).unwrap(),
            generator.load_workflow_patterns().unwrap()
        );
        assert_eq!(
            load_decision_framework_from(dir).unwrap(),
            generator.load_decision_framework().unwrap()
        );
        assert_eq!(
            load_communication_style_from(dir).unwrap(),
            generator.load_communication_style().unwrap()
        );
        assert_eq!(
            load_communication_style_from(dir).unwrap(),
            None,
            "absent doc loads as None"
        );
        assert_eq!(
            load_workflow_patterns_from(dir).unwrap().unwrap().patterns,
            vec!["P1".to_string(), "P2".to_string()]
        );
    }
}
