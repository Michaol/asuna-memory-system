//! L4 Mental Model: Abstract cognitive frameworks from L2/L3
//!
//! Triggered daily or every 100 L1 atoms (background async).
//! Outputs structured Markdown files:
//! - workflow-patterns.md: User's typical work patterns
//! - decision-framework.md: How user makes decisions
//! - communication-style.md: User's communication preferences

use crate::memory::llm::LlmClient;
use crate::memory::persona::Persona;
use crate::memory::scenario::Scenario;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowPatterns {
    pub patterns: Vec<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionFramework {
    pub criteria: Vec<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommunicationStyle {
    pub preferences: Vec<String>,
    pub updated_at: i64,
}

pub struct MentalModelGenerator {
    llm: Arc<LlmClient>,
    memory_dir: PathBuf,
    l1_count: Arc<AtomicUsize>,
}

impl MentalModelGenerator {
    pub fn new(llm: Arc<LlmClient>, memory_dir: PathBuf) -> Self {
        Self {
            llm,
            memory_dir,
            l1_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn increment_l1_count(&self) {
        self.l1_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn should_generate(&self) -> bool {
        self.l1_count.load(Ordering::SeqCst) >= 100
    }

    pub fn reset_count(&self) {
        self.l1_count.store(0, Ordering::SeqCst);
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

        let response = self.llm.chat(system, &user)?;
        let patterns: Vec<String> = serde_json::from_str(&response)?;

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

        let response = self.llm.chat(system, &user)?;
        let criteria: Vec<String> = serde_json::from_str(&response)?;

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

        let response = self.llm.chat(system, &user)?;
        let preferences: Vec<String> = serde_json::from_str(&response)?;

        Ok(CommunicationStyle {
            preferences,
            updated_at: chrono::Utc::now().timestamp(),
        })
    }

    pub fn save_workflow_patterns(&self, workflow: &WorkflowPatterns) -> Result<PathBuf> {
        let path = self.memory_dir.join("mental_models/workflow-patterns.md");
        std::fs::create_dir_all(path.parent().unwrap())?;

        let content = format!(
            "# Workflow Patterns\n\nUpdated: {}\n\n{}",
            chrono::DateTime::from_timestamp(workflow.updated_at, 0)
                .unwrap_or_else(chrono::Utc::now)
                .format("%Y-%m-%d %H:%M:%S UTC"),
            workflow
                .patterns
                .iter()
                .map(|p| format!("- {}", p))
                .collect::<Vec<_>>()
                .join("\n")
        );

        std::fs::write(&path, content)?;
        Ok(path)
    }

    pub fn save_decision_framework(&self, framework: &DecisionFramework) -> Result<PathBuf> {
        let path = self.memory_dir.join("mental_models/decision-framework.md");
        std::fs::create_dir_all(path.parent().unwrap())?;

        let content = format!(
            "# Decision Framework\n\nUpdated: {}\n\n{}",
            chrono::DateTime::from_timestamp(framework.updated_at, 0)
                .unwrap_or_else(chrono::Utc::now)
                .format("%Y-%m-%d %H:%M:%S UTC"),
            framework
                .criteria
                .iter()
                .map(|c| format!("- {}", c))
                .collect::<Vec<_>>()
                .join("\n")
        );

        std::fs::write(&path, content)?;
        Ok(path)
    }

    pub fn save_communication_style(&self, style: &CommunicationStyle) -> Result<PathBuf> {
        let path = self.memory_dir.join("mental_models/communication-style.md");
        std::fs::create_dir_all(path.parent().unwrap())?;

        let content = format!(
            "# Communication Style\n\nUpdated: {}\n\n{}",
            chrono::DateTime::from_timestamp(style.updated_at, 0)
                .unwrap_or_else(chrono::Utc::now)
                .format("%Y-%m-%d %H:%M:%S UTC"),
            style
                .preferences
                .iter()
                .map(|p| format!("- {}", p))
                .collect::<Vec<_>>()
                .join("\n")
        );

        std::fs::write(&path, content)?;
        Ok(path)
    }

    pub fn load_workflow_patterns(&self) -> Result<Option<WorkflowPatterns>> {
        let path = self.memory_dir.join("mental_models/workflow-patterns.md");
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)?;
        let patterns: Vec<String> = content
            .lines()
            .filter(|line| line.starts_with("- "))
            .map(|line| line.trim_start_matches("- ").to_string())
            .collect();

        // Parse updated_at from file metadata (format: "Updated: 2024-01-15 10:30:00 UTC")
        let updated_at = content
            .lines()
            .find(|line| line.starts_with("Updated: "))
            .and_then(|line| {
                let timestamp_str = line.trim_start_matches("Updated: ").trim_end_matches(" UTC");
                chrono::NaiveDateTime::parse_from_str(timestamp_str, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|dt| dt.and_utc().timestamp())
            })
            .unwrap_or_else(|| {
                // Fallback to file modification time
                std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
                    .unwrap_or(0)
            });

        Ok(Some(WorkflowPatterns {
            patterns,
            updated_at,
        }))
    }

    pub fn load_decision_framework(&self) -> Result<Option<DecisionFramework>> {
        let path = self.memory_dir.join("mental_models/decision-framework.md");
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)?;
        let criteria: Vec<String> = content
            .lines()
            .filter(|line| line.starts_with("- "))
            .map(|line| line.trim_start_matches("- ").to_string())
            .collect();

        // Parse updated_at from file metadata
        let updated_at = content
            .lines()
            .find(|line| line.starts_with("Updated: "))
            .and_then(|line| {
                let timestamp_str = line.trim_start_matches("Updated: ").trim_end_matches(" UTC");
                chrono::NaiveDateTime::parse_from_str(timestamp_str, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|dt| dt.and_utc().timestamp())
            })
            .unwrap_or_else(|| {
                std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
                    .unwrap_or(0)
            });

        Ok(Some(DecisionFramework {
            criteria,
            updated_at,
        }))
    }

    pub fn load_communication_style(&self) -> Result<Option<CommunicationStyle>> {
        let path = self.memory_dir.join("mental_models/communication-style.md");
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)?;
        let preferences: Vec<String> = content
            .lines()
            .filter(|line| line.starts_with("- "))
            .map(|line| line.trim_start_matches("- ").to_string())
            .collect();

        // Parse updated_at from file metadata
        let updated_at = content
            .lines()
            .find(|line| line.starts_with("Updated: "))
            .and_then(|line| {
                let timestamp_str = line.trim_start_matches("Updated: ").trim_end_matches(" UTC");
                chrono::NaiveDateTime::parse_from_str(timestamp_str, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|dt| dt.and_utc().timestamp())
            })
            .unwrap_or_else(|| {
                std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
                    .unwrap_or(0)
            });

        Ok(Some(CommunicationStyle {
            preferences,
            updated_at,
        }))
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
    fn test_l1_counter() {
        let temp_dir = TempDir::new().unwrap();
        let llm = Arc::new(LlmClient::new("test", "test", "test"));
        let generator = MentalModelGenerator::new(llm, temp_dir.path().to_path_buf());

        assert_eq!(generator.l1_count.load(Ordering::SeqCst), 0);
        assert!(!generator.should_generate());

        for _ in 0..99 {
            generator.increment_l1_count();
        }
        assert_eq!(generator.l1_count.load(Ordering::SeqCst), 99);
        assert!(!generator.should_generate());

        generator.increment_l1_count();
        assert_eq!(generator.l1_count.load(Ordering::SeqCst), 100);
        assert!(generator.should_generate());

        generator.reset_count();
        assert_eq!(generator.l1_count.load(Ordering::SeqCst), 0);
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
}
