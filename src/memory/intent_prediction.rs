//! L5 Intent Prediction: Predict future user needs based on historical patterns
//!
//! Refreshed in the pipeline's L3-L5 consolidation cycle right after the L4
//! docs (Phase 4b, S14c: `run_consolidation`). Analyzes the recent atom rows
//! plus the L4 mental models on disk and predicts:
//! - likely-next-topics.md: Topics user is likely to ask about
//! - anticipated-needs.md: Needs user might have in next sessions
//!
//! The read surface is `/recall` L5 (`memory/retrieval.rs`): the free
//! `load_*_from` loaders below are deliberately LLM-free so the gateway
//! handler needs none of this module's generation machinery.

use crate::memory::llm::LlmClient;
use crate::memory::mental_model::{load_list_md, save_list_md, MentalModelGenerator};
use crate::memory::scenario::Scenario;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LikelyNextTopics {
    pub topics: Vec<String>,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnticipatedNeeds {
    pub needs: Vec<String>,
    pub updated_at: i64,
}

/// L5 文档目录（`<memory_dir>/intent/`）——公开给 pipeline 的触发锚
/// （按目录 mtime 判断上次生成时间），避免布局字面量在两处漂移。
pub fn docs_dir(memory_dir: &Path) -> PathBuf {
    memory_dir.join("intent")
}

/// Single join site for the L5 document layout (`<memory_dir>/intent/
/// <file>.md`) — same rationale as `mental_model::mental_model_path`.
fn intent_path(memory_dir: &Path, file: &str) -> PathBuf {
    docs_dir(memory_dir).join(file)
}

/// Pure-file L5 loaders (S14c), LLM-free for the `/recall` L5 read surface;
/// `updated_at` semantics per [`load_list_md`].
pub fn load_likely_topics_from(memory_dir: &Path) -> Result<Option<LikelyNextTopics>> {
    Ok(
        load_list_md(&intent_path(memory_dir, "likely-next-topics.md"))?
            .map(|(topics, updated_at)| LikelyNextTopics { topics, updated_at }),
    )
}

pub fn load_anticipated_needs_from(memory_dir: &Path) -> Result<Option<AnticipatedNeeds>> {
    Ok(
        load_list_md(&intent_path(memory_dir, "anticipated-needs.md"))?
            .map(|(needs, updated_at)| AnticipatedNeeds { needs, updated_at }),
    )
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
            &intent_path(&self.memory_dir, "likely-next-topics.md"),
            "Likely Next Topics",
            topics.updated_at,
            &topics.topics,
        )
    }

    pub fn save_anticipated_needs(&self, needs: &AnticipatedNeeds) -> Result<PathBuf> {
        save_list_md(
            &intent_path(&self.memory_dir, "anticipated-needs.md"),
            "Anticipated Needs",
            needs.updated_at,
            &needs.needs,
        )
    }

    // The instance loaders have no production caller (the pipeline only
    // writes, /recall reads through the free loaders) — kept as the
    // generator-bound API, pinned by the equivalence test below.
    #[allow(dead_code)]
    pub fn load_likely_topics(&self) -> Result<Option<LikelyNextTopics>> {
        // J9: updated_at now parses from the "Updated: …" line (was hardcoded 0).
        load_likely_topics_from(&self.memory_dir)
    }

    #[allow(dead_code)]
    pub fn load_anticipated_needs(&self) -> Result<Option<AnticipatedNeeds>> {
        load_anticipated_needs_from(&self.memory_dir)
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

    /// S14c(7d): the pure-file `load_*_from` free loaders (the /recall L5
    /// read path) and the predictor-bound instance loaders must return
    /// exactly the same documents, present or absent.
    #[test]
    fn free_loaders_match_instance_loaders() {
        let temp_dir = TempDir::new().unwrap();
        let llm = std::sync::Arc::new(LlmClient::new("test", "test", "test"));
        let predictor = IntentPredictor::new(llm, temp_dir.path().to_path_buf());
        let dir = temp_dir.path();

        predictor
            .save_likely_topics(&LikelyNextTopics {
                topics: vec!["T1".to_string(), "T2".to_string()],
                updated_at: 1_700_000_000,
            })
            .unwrap();
        // anticipated-needs.md deliberately absent → both must agree on None.
        assert_eq!(
            load_likely_topics_from(dir).unwrap(),
            predictor.load_likely_topics().unwrap()
        );
        assert_eq!(
            load_anticipated_needs_from(dir).unwrap(),
            predictor.load_anticipated_needs().unwrap()
        );
        assert_eq!(load_anticipated_needs_from(dir).unwrap(), None);
    }
}
