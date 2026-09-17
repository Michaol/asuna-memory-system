//! L2 Scenario aggregation: group related L1 atoms into scene blocks
//!
//! Pipeline:
//! 1. Cluster L1 atoms by vector similarity (threshold > 0.8)
//! 2. LLM generates scenario summary for each cluster
//! 3. Store as a `memory_type='scenario'` row (the ONLY read surface —
//!    `/recall` L2 reads the DB), plus a human-readable Markdown mirror in
//!    memory/scenarios/.
//!
//! S14a (U1): the mirror's file reader (`load_scenarios` /
//! `load_scenario_from_file`) was deleted — it could never parse what
//! `save_scenario` wrote (the frontmatter omitted `summary`), had zero
//! callers, and the DB rows are the single source of truth. Files are for
//! humans only.
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

/// YAML frontmatter for the human-readable `.md` mirror (S14a U1: serialized
/// through serde_yaml — never hand-formatted — so titles containing `": "`
/// stay valid YAML). There is no code-side reader: DB rows are the only read
/// surface.
#[derive(Serialize)]
struct ScenarioFrontmatter<'a> {
    title: &'a str,
    atom_ids: &'a [i64],
    created_at: i64,
    updated_at: i64,
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
    pub fn cluster_atoms(atoms: &[(i64, String, Vec<f32>)], threshold: f32) -> Vec<Vec<usize>> {
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

    /// Save the scenario's human-readable Markdown mirror.
    ///
    /// The filename is `{created_at}_{db_id}.md` — keyed by the
    /// `bounded_memory` row id so the pipeline's cap-eviction can delete the
    /// mirror file alongside its DB row (J12; the old `{created_at}_{title}`
    /// naming had no DB mapping and forced title sanitization). Callers must
    /// invoke this only after (and only when) the DB row was inserted.
    pub fn save_scenario(&self, scenario: &Scenario, db_id: i64) -> anyhow::Result<PathBuf> {
        std::fs::create_dir_all(&self.scenarios_dir)?;

        let filename = format!("{}_{}.md", scenario.created_at, db_id);
        let path = self.scenarios_dir.join(&filename);

        let frontmatter = serde_yaml::to_string(&ScenarioFrontmatter {
            title: &scenario.title,
            atom_ids: &scenario.atom_ids,
            created_at: scenario.created_at,
            updated_at: scenario.updated_at,
        })?;

        // serde_yaml::to_string 的输出以 '\n' 结尾——不可 trim_end，否则闭合
        // "---" 会粘在最后一行 YAML 上（`updated_at: 2000---`），对任何标准
        // frontmatter 解析器整个文件都不可读。
        let content = format!("---\n{}---\n\n{}\n", frontmatter, scenario.summary);

        std::fs::write(&path, content)?;
        Ok(path)
    }
}

#[derive(Deserialize)]
struct ScenarioResponse {
    title: String,
    summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// U1/J12: the mirror frontmatter must be valid YAML even for a title
    /// containing ": " (the old hand-formatted write produced `title: Rust:
    /// 所有权` → invalid), and the filename must map back to the DB row id.
    #[test]
    fn test_save_scenario_writes_valid_yaml_frontmatter() {
        let tmp = tempfile::TempDir::new().unwrap();
        let llm = LlmClient::new("test", "test", "test");
        let pipeline = PipelineConfig::default();
        let aggregator = ScenarioAggregator::new(&llm, tmp.path(), &pipeline);

        let scenario = Scenario {
            title: "Rust: 所有权与生命周期".to_string(),
            atom_ids: vec![11, 22],
            summary: "用户在调试 Rust 借用检查。\n跨多行。".to_string(),
            created_at: 1000,
            updated_at: 2000,
        };
        let path = aggregator.save_scenario(&scenario, 7).unwrap();
        assert_eq!(path.file_name().unwrap().to_str().unwrap(), "1000_7.md");

        let content = std::fs::read_to_string(&path).unwrap();
        // 闭合分隔符必须独占一行：splitn("---") 对粘连形态（`2000---`）过于宽容
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
        assert_eq!(fm["title"].as_str().unwrap(), "Rust: 所有权与生命周期");
        let ids: Vec<i64> = fm["atom_ids"]
            .as_sequence()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![11, 22]);
        assert_eq!(fm["created_at"].as_i64().unwrap(), 1000);
        assert_eq!(fm["updated_at"].as_i64().unwrap(), 2000);
        assert_eq!(parts[2].trim(), "用户在调试 Rust 借用检查。\n跨多行。");
    }
}
