//! L2 Scenario aggregation: group related L1 atoms into scene blocks
//!
//! Pipeline:
//! 1. Cluster L1 atoms by vector similarity (threshold > 0.8)
//! 2. LLM generates scenario summary for each cluster
//! 3. Store as Markdown files in memory/scenarios/
//!
//! Each scenario block contains:
//! - Title (from LLM)
//! - Related atom IDs
//! - Summary (from LLM)
//! - Metadata (creation time, atom count)

use crate::config::PipelineConfig;
use crate::memory::dedup::cosine_similarity;
use crate::memory::llm::LlmClient;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// L2 Scenario block
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub title: String,
    pub atom_ids: Vec<i64>,
    pub summary: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// L2 Scenario aggregator
pub struct ScenarioAggregator<'a> {
    llm: &'a LlmClient,
    scenarios_dir: PathBuf,
    config: &'a PipelineConfig,
}

impl<'a> ScenarioAggregator<'a> {
    pub fn new(llm: &'a LlmClient, scenarios_dir: &Path, config: &'a PipelineConfig) -> Self {
        Self {
            llm,
            scenarios_dir: scenarios_dir.to_path_buf(),
            config,
        }
    }

    /// Cluster atoms by vector similarity and generate scenarios.
    ///
    /// # Arguments
    /// * `atoms` - List of (atom_id, content, embedding) tuples
    /// * `threshold` - Cosine similarity threshold for clustering
    /// * `min_cluster` - Minimum atoms in a cluster to form a scenario
    ///   (singletons are skipped — they are not a "scene").
    pub fn aggregate(
        &self,
        atoms: &[(i64, String, Vec<f32>)],
        threshold: f32,
        min_cluster: usize,
    ) -> anyhow::Result<Vec<Scenario>> {
        if atoms.is_empty() {
            return Ok(vec![]);
        }

        let clusters = Self::cluster_atoms(atoms, threshold);

        let mut scenarios = Vec::new();
        for cluster in clusters {
            if cluster.len() < min_cluster {
                // Skip single-atom clusters (not a "scene")
                continue;
            }

            // Generate scenario summary using LLM
            let scenario = self.generate_scenario(&cluster, atoms)?;
            scenarios.push(scenario);
        }

        Ok(scenarios)
    }

    /// Cluster atoms using simple greedy algorithm (does not require LLM)
    pub fn cluster_atoms(
        atoms: &[(i64, String, Vec<f32>)],
        threshold: f32,
    ) -> Vec<Vec<usize>> {
        let mut clusters: Vec<Vec<usize>> = Vec::new();
        let mut assigned = vec![false; atoms.len()];

        for i in 0..atoms.len() {
            if assigned[i] {
                continue;
            }

            let mut cluster = vec![i];
            assigned[i] = true;

            // Find all atoms similar to atom i
            for j in (i + 1)..atoms.len() {
                if assigned[j] {
                    continue;
                }

                let similarity = cosine_similarity(&atoms[i].2, &atoms[j].2);
                if similarity > threshold {
                    cluster.push(j);
                    assigned[j] = true;
                }
            }

            if cluster.len() >= 2 {
                clusters.push(cluster);
            }
        }

        clusters
    }

    /// Generate scenario summary from cluster using LLM
    fn generate_scenario(
        &self,
        cluster: &[usize],
        atoms: &[(i64, String, Vec<f32>)],
    ) -> anyhow::Result<Scenario> {
        let atom_ids: Vec<i64> = cluster.iter().map(|&i| atoms[i].0).collect();
        let contents: Vec<String> = cluster.iter().map(|&i| atoms[i].1.clone()).collect();

        let system = "You are a memory summarization system. Given a list of related facts, create a concise scenario summary.

Return JSON format:
{
  \"title\": \"Short descriptive title (max 50 chars)\",
  \"summary\": \"2-3 sentence summary of the common theme\"
}";

        let user = format!("Related facts:\n{}", contents.join("\n- "));

        let response: ScenarioResponse = self.llm.chat_json(system, &user)?;

        let now = crate::util::time::now_unix_ms();

        Ok(Scenario {
            title: response.title,
            atom_ids,
            summary: response.summary,
            created_at: now,
            updated_at: now,
        })
    }

    /// Save scenario to Markdown file
    pub fn save_scenario(&self, scenario: &Scenario) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.scenarios_dir)?;

        let filename = format!(
            "{}_{}.md",
            scenario.created_at,
            sanitize_filename(&scenario.title)
        );
        let path = self.scenarios_dir.join(&filename);

        let content = format!(
            r#"---
title: {}
atom_ids: {:?}
created_at: {}
updated_at: {}
---

{}
"#,
            scenario.title,
            scenario.atom_ids,
            scenario.created_at,
            scenario.updated_at,
            scenario.summary
        );

        std::fs::write(&path, content)?;
        Ok(path)
    }

    /// Load all scenarios from directory
    pub fn load_scenarios(&self) -> anyhow::Result<Vec<Scenario>> {
        if !self.scenarios_dir.exists() {
            return Ok(vec![]);
        }

        let mut scenarios = Vec::new();

        for entry in std::fs::read_dir(&self.scenarios_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Ok(scenario) = self.load_scenario_from_file(&path) {
                    scenarios.push(scenario);
                }
            }
        }

        Ok(scenarios)
    }

    /// Load single scenario from Markdown file
    fn load_scenario_from_file(&self, path: &Path) -> anyhow::Result<Scenario> {
        let content = std::fs::read_to_string(path)?;

        // Parse YAML frontmatter
        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            anyhow::bail!("Invalid scenario file format");
        }

        let frontmatter = parts[1].trim();
        let summary = parts[2].trim();

        let scenario: Scenario = serde_yaml::from_str(frontmatter)?;

        Ok(Scenario {
            summary: summary.to_string(),
            ..scenario
        })
    }
}

#[derive(Deserialize)]
struct ScenarioResponse {
    title: String,
    summary: String,
}

/// Sanitize filename (remove invalid characters)
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(50)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("Hello World!"), "Hello_World_");
        assert_eq!(sanitize_filename("user-preference"), "user-preference");
        assert_eq!(sanitize_filename("test/\\file"), "test__file");
    }

    #[test]
    fn test_cluster_atoms_empty() {
        let atoms: Vec<(i64, String, Vec<f32>)> = vec![];
        let clusters = ScenarioAggregator::cluster_atoms(&atoms, 0.8);
        assert!(clusters.is_empty());
    }

    #[test]
    fn test_cluster_atoms_single() {
        let atoms = vec![(1, "test".to_string(), vec![1.0, 0.0, 0.0])];
        let clusters = ScenarioAggregator::cluster_atoms(&atoms, 0.8);
        assert!(clusters.is_empty()); // Single atom doesn't form a cluster
    }

    #[test]
    fn test_cluster_atoms_similar() {
        let atoms = vec![
            (1, "rust programming".to_string(), vec![1.0, 0.0, 0.0]),
            (2, "rust language".to_string(), vec![0.95, 0.0, 0.0]),
            (3, "python code".to_string(), vec![0.0, 1.0, 0.0]),
        ];
        let clusters = ScenarioAggregator::cluster_atoms(&atoms, 0.8);
        assert_eq!(clusters.len(), 1); // Only the rust pair forms a cluster
        assert_eq!(clusters[0].len(), 2);
    }
}
