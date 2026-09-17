//! L5 Intent Prediction: Predict future user needs based on historical patterns
//!
//! Triggered after L4 updates. Analyzes L2 scenarios and L4 mental models to predict:
//! - likely-next-topics.md: Topics user is likely to ask about
//! - anticipated-needs.md: Needs user might have in next sessions

use crate::memory::llm::LlmClient;
use crate::memory::mental_model::{load_list_md, save_list_md, MentalModelGenerator};
use crate::memory::scenario::Scenario;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LikelyNextTopics {
    pub topics: Vec<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnticipatedNeeds {
    pub needs: Vec<String>,
    pub updated_at: i64,
}

pub struct IntentPredictor {
    llm: std::sync::Arc<LlmClient>,
    memory_dir: PathBuf,
}

impl IntentPredictor {
    pub fn new(llm: std::sync::Arc<LlmClient>, memory_dir: PathBuf) -> Self {
        Self { llm, memory_dir }
    }

    pub fn predict_likely_topics(
        &self,
        scenarios: &[Scenario],
        mental_models: &MentalModelGenerator,
    ) -> Result<LikelyNextTopics> {
        let context = self.build_context(scenarios, mental_models)?;

        let system = "You are a topic predictor. Based on the user's recent scenarios and mental models, predict what topics they are likely to ask about next.";
        let user = format!(
            "{}\n\nPredict 3-5 likely next topics. Return ONLY a JSON array of strings, no markdown.",
            context
        );

        // J9: chat_json tolerates markdown fences / prose around the JSON.
        let topics: Vec<String> = self.llm.chat_json(system, &user)?;

        Ok(LikelyNextTopics {
            topics,
            updated_at: chrono::Utc::now().timestamp(),
        })
    }

    pub fn predict_anticipated_needs(
        &self,
        scenarios: &[Scenario],
        mental_models: &MentalModelGenerator,
    ) -> Result<AnticipatedNeeds> {
        let context = self.build_context(scenarios, mental_models)?;

        let system = "You are a needs analyst. Based on the user's recent scenarios and mental models, predict what needs they might have in future sessions.";
        let user = format!(
            "{}\n\nPredict 3-5 anticipated needs. Return ONLY a JSON array of strings, no markdown.",
            context
        );

        let needs: Vec<String> = self.llm.chat_json(system, &user)?;

        Ok(AnticipatedNeeds {
            needs,
            updated_at: chrono::Utc::now().timestamp(),
        })
    }

    pub fn save_likely_topics(&self, topics: &LikelyNextTopics) -> Result<PathBuf> {
        save_list_md(
            &self.memory_dir.join("intent/likely-next-topics.md"),
            "Likely Next Topics",
            topics.updated_at,
            &topics.topics,
        )
    }

    pub fn save_anticipated_needs(&self, needs: &AnticipatedNeeds) -> Result<PathBuf> {
        save_list_md(
            &self.memory_dir.join("intent/anticipated-needs.md"),
            "Anticipated Needs",
            needs.updated_at,
            &needs.needs,
        )
    }

    pub fn load_likely_topics(&self) -> Result<Option<LikelyNextTopics>> {
        // J9: updated_at now parses from the "Updated: …" line (was hardcoded 0).
        Ok(
            load_list_md(&self.memory_dir.join("intent/likely-next-topics.md"))?
                .map(|(topics, updated_at)| LikelyNextTopics { topics, updated_at }),
        )
    }

    pub fn load_anticipated_needs(&self) -> Result<Option<AnticipatedNeeds>> {
        Ok(
            load_list_md(&self.memory_dir.join("intent/anticipated-needs.md"))?
                .map(|(needs, updated_at)| AnticipatedNeeds { needs, updated_at }),
        )
    }

    fn build_context(
        &self,
        scenarios: &[Scenario],
        mental_models: &MentalModelGenerator,
    ) -> Result<String> {
        let mut context = String::new();

        // Add mental models
        if let Some(workflow) = mental_models.load_workflow_patterns()? {
            context.push_str("Workflow Patterns:\n");
            for pattern in &workflow.patterns {
                context.push_str(&format!("- {}\n", pattern));
            }
            context.push('\n');
        }

        if let Some(decision) = mental_models.load_decision_framework()? {
            context.push_str("Decision Framework:\n");
            for criterion in &decision.criteria {
                context.push_str(&format!("- {}\n", criterion));
            }
            context.push('\n');
        }

        if let Some(comm) = mental_models.load_communication_style()? {
            context.push_str("Communication Style:\n");
            for pref in &comm.preferences {
                context.push_str(&format!("- {}\n", pref));
            }
            context.push('\n');
        }

        // Add recent scenarios
        context.push_str("Recent Scenarios:\n");
        for scenario in scenarios.iter().take(15) {
            context.push_str(&format!("- {}: {}\n", scenario.title, scenario.summary));
        }

        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_likely_topics_serialization() {
        let topics = LikelyNextTopics {
            topics: vec!["Topic 1".to_string(), "Topic 2".to_string()],
            updated_at: 1234567890,
        };

        let json = serde_json::to_string(&topics).unwrap();
        let deserialized: LikelyNextTopics = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.topics.len(), 2);
        assert_eq!(deserialized.topics[0], "Topic 1");
    }

    #[test]
    fn test_save_and_load_likely_topics() {
        let temp_dir = TempDir::new().unwrap();
        let llm = std::sync::Arc::new(LlmClient::new("test", "test", "test"));
        let predictor = IntentPredictor::new(llm, temp_dir.path().to_path_buf());

        let topics = LikelyNextTopics {
            topics: vec!["Topic X".to_string(), "Topic Y".to_string()],
            updated_at: 1_700_000_000,
        };

        predictor.save_likely_topics(&topics).unwrap();

        let loaded = predictor.load_likely_topics().unwrap().unwrap();
        assert_eq!(loaded.topics.len(), 2);
        assert_eq!(loaded.topics[0], "Topic X");
        assert_eq!(loaded.topics[1], "Topic Y");
        // J9: was hardcoded 0; must now come back from the "Updated:" line.
        assert_eq!(loaded.updated_at, 1_700_000_000);
    }

    #[test]
    fn test_save_and_load_anticipated_needs_parses_updated_at() {
        let temp_dir = TempDir::new().unwrap();
        let llm = std::sync::Arc::new(LlmClient::new("test", "test", "test"));
        let predictor = IntentPredictor::new(llm, temp_dir.path().to_path_buf());

        let needs = AnticipatedNeeds {
            needs: vec!["Need 1".to_string(), "Need 2".to_string()],
            updated_at: 1_600_000_000,
        };

        predictor.save_anticipated_needs(&needs).unwrap();

        let loaded = predictor.load_anticipated_needs().unwrap().unwrap();
        assert_eq!(loaded.needs, needs.needs);
        assert_eq!(loaded.updated_at, 1_600_000_000);
    }
}
