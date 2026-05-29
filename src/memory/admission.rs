//! A-MAC 5 维准入评分系统
//!
//! 决定新提取的原子事实是否值得存储为长期记忆。
//!
//! 5 个维度：
//! - Utility: LLM 评分 (0-1) — 需要 LLM
//! - Novelty: 与已有 L1 的向量距离 (规则)
//! - Recency: 时间衰减函数 (规则)
//! - Importance: 类型权重 (规则)
//! - Confidence: 对话文本可信度 (规则)
//!
//! 4 个规则维度 < 65ms，仅 Utility 需要 LLM 调用。

use crate::config::AdmissionConfig;
use crate::memory::llm::LlmClient;
use serde::{Deserialize, Serialize};

/// 准入评分结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdmissionScore {
    pub score: f64,
    pub admitted: bool,
    pub dimensions: ScoreDimensions,
}

/// 5 维评分明细
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreDimensions {
    pub utility: f64,
    pub novelty: f64,
    pub recency: f64,
    pub importance: f64,
    pub confidence: f64,
}

/// A-MAC 准入评分器
pub struct AdmissionScorer<'a> {
    config: &'a AdmissionConfig,
    llm: Option<&'a LlmClient>,
}

impl<'a> AdmissionScorer<'a> {
    pub fn new(config: &'a AdmissionConfig, llm: Option<&'a LlmClient>) -> Self {
        Self { config, llm }
    }

    /// Get the admission threshold
    pub fn threshold(&self) -> f64 {
        self.config.threshold
    }

    /// 计算准入评分
    ///
    /// # Arguments
    /// * `content` - 待评估的原子事实内容
    /// * `atom_type` - 原子事实类型 (fact/preference/decision/relationship)
    /// * `embedding` - 原子事实的向量表示
    /// * `existing_embeddings` - 已有 L1 原子的向量列表
    /// * `conversation_context` - 对话上下文（用于 Utility 评分）
    pub fn score(
        &self,
        content: &str,
        atom_type: &str,
        embedding: &[f32],
        existing_embeddings: &[Vec<f32>],
        conversation_context: &str,
    ) -> anyhow::Result<AdmissionScore> {
        // 4 个规则维度（快速计算）
        let novelty = self.score_novelty(embedding, existing_embeddings);
        let recency = self.score_recency();
        let importance = self.score_importance(atom_type);
        let confidence = self.score_confidence(content, conversation_context);

        // Utility 维度（需要 LLM）
        let utility = if let Some(llm) = self.llm {
            self.score_utility(llm, content, conversation_context)?
        } else {
            // LLM 不可用时使用中性值
            0.5
        };

        // 加权求和
        let weights = &self.config.weights;
        let score = utility * weights[0]
            + novelty * weights[1]
            + recency * weights[2]
            + importance * weights[3]
            + confidence * weights[4];

        let admitted = score >= self.config.threshold;

        Ok(AdmissionScore {
            score,
            admitted,
            dimensions: ScoreDimensions {
                utility,
                novelty,
                recency,
                importance,
                confidence,
            },
        })
    }

    /// Utility: LLM 评分 (0-1)
    ///
    /// 评估原子事实在未来对话中的潜在价值。
    fn score_utility(
        &self,
        llm: &LlmClient,
        content: &str,
        conversation_context: &str,
    ) -> anyhow::Result<f64> {
        let system = "You are a memory utility evaluator. Rate how useful this fact will be for future conversations on a scale of 0.0 to 1.0.

Consider:
- Will this fact likely be referenced again?
- Does it contain actionable or important information?
- Is it specific enough to be useful?

Respond with ONLY a number between 0.0 and 1.0 (e.g., \"0.75\").";

        let user = format!(
            "Conversation context:\n{}\n\nFact to evaluate:\n{}",
            conversation_context, content
        );

        let response = llm.chat(system, &user)?;
        let score: f64 = response
            .trim()
            .parse::<f64>()
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);

        Ok(score)
    }

    /// Novelty: 与已有 L1 的向量距离 (规则)
    ///
    /// 计算与最近邻的平均距离，越高越新颖。
    fn score_novelty(&self, embedding: &[f32], existing_embeddings: &[Vec<f32>]) -> f64 {
        if existing_embeddings.is_empty() {
            return 1.0; // 没有任何已有记忆，完全新颖
        }

        let similarities: Vec<f64> = existing_embeddings
            .iter()
            .map(|e| crate::memory::dedup::cosine_similarity(embedding, e) as f64)
            .collect();

        // 使用最大相似度（最近邻）
        let max_sim = similarities
            .iter()
            .cloned()
            .fold(0.0_f64, f64::max);

        // 转换为新颖度（相似度越高，新颖度越低）
        (1.0_f64 - max_sim).clamp(0.0, 1.0)
    }

    /// Recency: 时间衰减函数 (规则)
    ///
    /// 基于当前时间戳，越新越高。
    /// 使用指数衰减：e^(-λt)，其中 t 是小时数，λ=0.1
    fn score_recency(&self) -> f64 {
        // 假设提取发生在对话进行中，使用固定值
        // 实际场景中可以根据对话开始时间计算
        let hours_since_start: f64 = 0.0; // 当前时刻
        let lambda: f64 = 0.1;

        (-lambda * hours_since_start).exp().clamp(0.0, 1.0)
    }

    /// Importance: 类型权重 (规则)
    ///
    /// 不同类型的原子事实有不同的重要性权重。
    fn score_importance(&self, atom_type: &str) -> f64 {
        match atom_type {
            "decision" => 0.9,    // 决策最重要
            "preference" => 0.8,  // 偏好次之
            "fact" => 0.7,        // 事实
            "relationship" => 0.6, // 关系
            _ => 0.5,             // 未知类型
        }
    }

    /// Confidence: 对话文本可信度 (规则)
    ///
    /// 基于对话上下文的清晰度和完整性评估。
    fn score_confidence(&self, content: &str, conversation_context: &str) -> f64 {
        let mut score: f64 = 0.5;

        // 1. 内容长度（太短或太长都不可靠）
        let len = content.len();
        if (20..=200).contains(&len) {
            score += 0.1;
        } else if len < 10 {
            score -= 0.2;
        }

        // 2. 是否包含具体细节（数字、日期、名称）
        if content.contains(|c: char| c.is_numeric()) {
            score += 0.1;
        }

        // 3. 对话上下文长度（越长越可靠）
        if conversation_context.len() > 500 {
            score += 0.1;
        }

        // 4. 是否包含不确定性词汇
        let uncertain_words = ["可能", "也许", "大概", "似乎", "maybe", "perhaps", "probably"];
        if uncertain_words.iter().any(|w| content.contains(w)) {
            score -= 0.1;
        }

        score.clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AdmissionConfig;

    fn default_config() -> AdmissionConfig {
        AdmissionConfig {
            enabled: true,
            threshold: 0.6,
            weights: [0.3, 0.2, 0.2, 0.2, 0.1],
        }
    }

    #[test]
    fn test_score_novelty_empty() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        let embedding = vec![1.0, 2.0, 3.0];
        let existing: Vec<Vec<f32>> = vec![];

        let novelty = scorer.score_novelty(&embedding, &existing);
        assert_eq!(novelty, 1.0);
    }

    #[test]
    fn test_score_novelty_similar() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        let embedding = vec![1.0, 0.0, 0.0];
        let existing = vec![vec![1.0, 0.0, 0.0]]; // 完全相同

        let novelty = scorer.score_novelty(&embedding, &existing);
        assert!(novelty < 0.01); // 相似度接近 1.0，新颖度接近 0
    }

    #[test]
    fn test_score_novelty_different() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        let embedding = vec![1.0, 0.0, 0.0];
        let existing = vec![vec![0.0, 1.0, 0.0]]; // 正交

        let novelty = scorer.score_novelty(&embedding, &existing);
        assert!(novelty > 0.9); // 相似度接近 0，新颖度接近 1
    }

    #[test]
    fn test_score_importance() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        assert_eq!(scorer.score_importance("decision"), 0.9);
        assert_eq!(scorer.score_importance("preference"), 0.8);
        assert_eq!(scorer.score_importance("fact"), 0.7);
        assert_eq!(scorer.score_importance("relationship"), 0.6);
        assert_eq!(scorer.score_importance("unknown"), 0.5);
    }

    #[test]
    fn test_score_confidence_good() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        let content = "用户喜欢使用 Rust 语言进行系统编程，已有 5 年经验";
        let context = "这是一个关于技术栈选择的长对话...（超过500字符的上下文）";

        let confidence = scorer.score_confidence(content, &context.repeat(20));
        assert!(confidence > 0.7);
    }

    #[test]
    fn test_score_confidence_uncertain() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        let content = "用户可能喜欢 Rust";
        let context = "短对话";

        let confidence = scorer.score_confidence(content, context);
        assert!(confidence < 0.6);
    }

    #[test]
    fn test_score_recency() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        let recency = scorer.score_recency();
        assert_eq!(recency, 1.0); // 当前时刻应该是 1.0
    }

    #[test]
    fn test_score_without_llm() {
        let config = default_config();
        let scorer = AdmissionScorer::new(&config, None);

        let content = "用户喜欢 Rust";
        let embedding = vec![1.0, 0.0, 0.0];
        let existing: Vec<Vec<f32>> = vec![];
        let context = "对话上下文";

        let result = scorer
            .score(content, "preference", &embedding, &existing, context)
            .unwrap();

        // Utility 应该是 0.5（LLM 不可用时的默认值）
        assert_eq!(result.dimensions.utility, 0.5);
        assert_eq!(result.dimensions.novelty, 1.0);
        assert_eq!(result.dimensions.recency, 1.0);
        assert_eq!(result.dimensions.importance, 0.8); // preference

        // 检查加权分数
        let expected = 0.5 * 0.3 + 1.0 * 0.2 + 1.0 * 0.2 + 0.8 * 0.2 + result.dimensions.confidence * 0.1;
        assert!((result.score - expected).abs() < 0.01);
    }

    #[test]
    fn test_admission_threshold() {
        let config = AdmissionConfig {
            enabled: true,
            threshold: 0.8, // 高阈值
            weights: [0.3, 0.2, 0.2, 0.2, 0.1],
        };
        let scorer = AdmissionScorer::new(&config, None);

        let content = "用户喜欢 Rust";
        let embedding = vec![1.0, 0.0, 0.0];
        let existing: Vec<Vec<f32>> = vec![];
        let context = "对话上下文";

        let result = scorer
            .score(content, "preference", &embedding, &existing, context)
            .unwrap();

        // 分数应该低于 0.8（因为 Utility 是 0.5）
        assert!(!result.admitted);
    }
}
