//! P7: Skill Memory - SOP 自动生成
//!
//! 追踪 Agent 解决同类问题的历史路径，识别重复模式（3+ 次），
//! 使用 LLM 从多次执行轨迹中抽象出通用 SOP。

use crate::memory::llm::LlmClient;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 执行轨迹
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionTrace {
    pub session_id: String,
    pub problem_type: String,
    pub steps: Vec<String>,
    pub success: bool,
    pub timestamp: i64,
}

/// SOP 技能
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub trigger_conditions: Vec<String>,
    pub steps: Vec<String>,
    pub success_rate: f32,
    pub usage_count: u32,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 技能记忆管理器
pub struct SkillMemory {
    llm: Arc<LlmClient>,
    skills_dir: PathBuf,
    traces: Arc<Mutex<HashMap<String, Vec<ExecutionTrace>>>>,
}

impl SkillMemory {
    pub fn new(llm: Arc<LlmClient>, memory_dir: &Path) -> Self {
        let skills_dir = memory_dir.join("skills");
        std::fs::create_dir_all(&skills_dir).ok();

        Self {
            llm,
            skills_dir,
            traces: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 记录执行轨迹
    pub fn record_trace(&self, trace: ExecutionTrace) -> Result<()> {
        let mut traces = self.traces.lock().unwrap();
        traces
            .entry(trace.problem_type.clone())
            .or_default()
            .push(trace);
        Ok(())
    }

    /// 检查是否应该提取技能（3+ 次相似问题）
    pub fn should_extract_skill(&self, problem_type: &str) -> bool {
        let traces = self.traces.lock().unwrap();
        traces
            .get(problem_type)
            .map(|t| t.len() >= 3)
            .unwrap_or(false)
    }

    /// 从执行轨迹中提取 SOP
    pub fn extract_skill(&self, problem_type: &str) -> Result<Skill> {
        let traces = self.traces.lock().unwrap();
        let problem_traces = traces
            .get(problem_type)
            .ok_or_else(|| anyhow::anyhow!("No traces found for problem type: {}", problem_type))?;

        if problem_traces.len() < 3 {
            return Err(anyhow::anyhow!(
                "Need at least 3 traces to extract skill, found {}",
                problem_traces.len()
            ));
        }

        // 准备 LLM 上下文
        let traces_text = problem_traces
            .iter()
            .enumerate()
            .map(|(i, t)| {
                format!(
                    "Trace {}:\n- Steps: {}\n- Success: {}\n",
                    i + 1,
                    t.steps.join(" → "),
                    t.success
                )
            })
            .collect::<Vec<_>>()
            .join("\n");

        let system = "You are a workflow analyst. Extract a general SOP from multiple execution traces.";
        let user = format!(
            "Problem type: {}\n\nExecution traces:\n{}\n\nExtract:\n1. Skill name (short, descriptive)\n2. Trigger conditions (3-5 bullet points)\n3. General steps (5-10 steps)\n\nReturn JSON with fields: name, trigger_conditions, steps.",
            problem_type, traces_text
        );

        let response = self.llm.chat(system, &user)?;
        let extracted: serde_json::Value = serde_json::from_str(&response)?;

        let success_count = problem_traces.iter().filter(|t| t.success).count();
        let success_rate = success_count as f32 / problem_traces.len() as f32;

        let now = chrono::Utc::now().timestamp();

        Ok(Skill {
            name: extracted["name"]
                .as_str()
                .unwrap_or(problem_type)
                .to_string(),
            trigger_conditions: extracted["trigger_conditions"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            steps: extracted["steps"]
                .as_array()
                .unwrap_or(&vec![])
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            success_rate,
            usage_count: 0,
            created_at: now,
            updated_at: now,
        })
    }

    /// 保存技能到文件
    pub fn save_skill(&self, skill: &Skill) -> Result<PathBuf> {
        let filename = format!("{}.md", skill.name.to_lowercase().replace(' ', "-"));
        let path = self.skills_dir.join(&filename);

        // Use serde_yaml for robust serialization
        let frontmatter = serde_yaml::to_string(skill)?;

        let steps_content = skill
            .steps
            .iter()
            .enumerate()
            .map(|(i, s)| format!("{}. {}", i + 1, s))
            .collect::<Vec<_>>()
            .join("\n");

        let triggers_content = skill
            .trigger_conditions
            .iter()
            .map(|c| format!("- {}", c))
            .collect::<Vec<_>>()
            .join("\n");

        let content = format!("---\n{}---\n\n# {}\n\n## Trigger Conditions\n{}\n\n## Steps\n{}\n",
            frontmatter, skill.name, triggers_content, steps_content);

        std::fs::write(&path, content)?;
        Ok(path)
    }

    /// 加载所有技能
    pub fn load_skills(&self) -> Result<Vec<Skill>> {
        let mut skills = Vec::new();

        for entry in std::fs::read_dir(&self.skills_dir)? {
            let entry = entry?;
            let path = entry.path();

            if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Ok(skill) = self.load_skill(&path) {
                    skills.push(skill);
                }
            }
        }

        Ok(skills)
    }

    /// 加载单个技能
    fn load_skill(&self, path: &Path) -> Result<Skill> {
        let content = std::fs::read_to_string(path)?;

        // 解析 YAML frontmatter
        let parts: Vec<&str> = content.splitn(3, "---").collect();
        if parts.len() < 3 {
            return Err(anyhow::anyhow!("Invalid skill file format"));
        }

        let frontmatter = parts[1].trim();
        let skill: Skill = serde_yaml::from_str(frontmatter)?;

        Ok(skill)
    }

    /// 记录技能使用
    pub fn record_usage(&self, skill_name: &str) -> Result<()> {
        let skills = self.load_skills()?;
        for mut skill in skills {
            if skill.name == skill_name {
                skill.usage_count += 1;
                skill.updated_at = chrono::Utc::now().timestamp();
                self.save_skill(&skill)?;
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_record_and_check_trace_count() {
        let temp_dir = TempDir::new().unwrap();
        let llm = Arc::new(LlmClient::new("http://test", "test-key", "test-model"));
        let skill_memory = SkillMemory::new(llm, temp_dir.path());

        let trace = ExecutionTrace {
            session_id: "session-1".to_string(),
            problem_type: "compile-error".to_string(),
            steps: vec!["Read error".to_string(), "Fix code".to_string()],
            success: true,
            timestamp: chrono::Utc::now().timestamp(),
        };

        skill_memory.record_trace(trace.clone()).unwrap();
        assert!(!skill_memory.should_extract_skill("compile-error"));

        skill_memory.record_trace(trace.clone()).unwrap();
        assert!(!skill_memory.should_extract_skill("compile-error"));

        skill_memory.record_trace(trace).unwrap();
        assert!(skill_memory.should_extract_skill("compile-error"));
    }

    #[test]
    fn test_save_and_load_skill() {
        let temp_dir = TempDir::new().unwrap();
        let llm = Arc::new(LlmClient::new("http://test", "test-key", "test-model"));
        let skill_memory = SkillMemory::new(llm, temp_dir.path());

        let skill = Skill {
            name: "Fix Compile Error".to_string(),
            trigger_conditions: vec![
                "Build fails with error".to_string(),
                "Error message contains syntax".to_string(),
            ],
            steps: vec![
                "Read error message".to_string(),
                "Identify error type".to_string(),
                "Fix code".to_string(),
            ],
            success_rate: 0.85,
            usage_count: 5,
            created_at: chrono::Utc::now().timestamp(),
            updated_at: chrono::Utc::now().timestamp(),
        };

        let path = skill_memory.save_skill(&skill).unwrap();
        assert!(path.exists());

        let skills = skill_memory.load_skills().unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "Fix Compile Error");
        assert_eq!(skills[0].success_rate, 0.85);
    }

    #[test]
    fn test_record_usage() {
        let temp_dir = TempDir::new().unwrap();
        let llm = Arc::new(LlmClient::new("http://test", "test-key", "test-model"));
        let skill_memory = SkillMemory::new(llm, temp_dir.path());

        let skill = Skill {
            name: "Test Skill".to_string(),
            trigger_conditions: vec!["Test".to_string()],
            steps: vec!["Step 1".to_string()],
            success_rate: 1.0,
            usage_count: 0,
            created_at: chrono::Utc::now().timestamp(),
            updated_at: chrono::Utc::now().timestamp(),
        };

        skill_memory.save_skill(&skill).unwrap();

        skill_memory.record_usage("Test Skill").unwrap();

        let skills = skill_memory.load_skills().unwrap();
        assert_eq!(skills[0].usage_count, 1);
    }
}
