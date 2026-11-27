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

    // 无已有记忆时完全新颖
    assert_eq!(scorer.score_novelty(&embedding, &existing), 1.0);
    // 同向对比：完全重复向量 novelty 必须跌至 ~0.0（击杀恒返 1.0 或失效余弦计算）
    let dup = scorer.score_novelty(&embedding, std::slice::from_ref(&embedding));
    assert!(
        dup < 1e-6,
        "identical existing vector must yield ~0 novelty, got {dup}"
    );
    // 正交向量 + 负相关向量：novelty 保持 ~1.0（夹紧下界 0，不得 >1.0）
    let ortho = scorer.score_novelty(
        &[1.0, 0.0, 0.0],
        &[vec![0.0, 1.0, 0.0], vec![-1.0, 0.0, 0.0]],
    );
    assert!(
        (ortho - 1.0).abs() < 1e-6,
        "orthogonal/opposite vectors must yield 1.0, got {ortho}"
    );
    // 零范数输入不得产生 NaN
    let zero = scorer.score_novelty(&[0.0, 0.0, 0.0], &[embedding]);
    assert!(zero.is_finite() && (0.0..=1.0).contains(&zero));
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

/// J10: length thresholds count characters, not bytes. 8 CJK chars are
/// 24 bytes — byte logic scored them "in the 20..=200 sweet spot"
/// (+0.1), char logic applies the <10-char penalty (-0.2).
#[test]
fn test_score_confidence_short_cjk_gets_penalty() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);

    let content = "用户喜欢喝绿茶哦"; // 8 chars / 24 bytes
    assert_eq!(content.chars().count(), 8);
    assert_eq!(content.len(), 24);

    let confidence = scorer.score_confidence(content, "短上下文");
    assert!(
        (confidence - 0.3).abs() < 1e-9,
        "expected base 0.5 - 0.2 short-content penalty, got {}",
        confidence
    );
}

/// J10: 80 CJK chars are 240 bytes — byte logic missed the sweet spot;
/// char logic gives +0.1.
#[test]
fn test_score_confidence_cjk_sweet_spot() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);

    let content = "用".repeat(80); // 80 chars / 240 bytes
    assert_eq!(content.len(), 240);

    let confidence = scorer.score_confidence(&content, "短上下文");
    assert!(
        (confidence - 0.6).abs() < 1e-9,
        "expected base 0.5 + 0.1 sweet-spot bonus, got {}",
        confidence
    );
}

/// L15/C11: the long-context bonus counts CHARACTERS, not bytes. 200 CJK
/// chars are 600 BYTES — the old byte logic awarded the +0.1 bonus here
/// (600 > 500), the char rule does not (200 ≤ 500). With real turn
/// previews flowing in (C11) the branch must trigger on >500 chars
/// instead (501 CJK = 1503 bytes: true under both, pinned from the other
/// side).
#[test]
fn test_score_confidence_context_char_not_byte_threshold() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);

    let content = "a".repeat(30); // sweet spot +0.1, no digits/uncertainty
    assert_eq!(content.chars().count(), 30);

    let context_200_cjk = "用".repeat(200); // 200 chars / 600 bytes
    assert_eq!(context_200_cjk.len(), 600);
    let confidence = scorer.score_confidence(&content, &context_200_cjk);
    assert!(
        (confidence - 0.6).abs() < 1e-9,
        "600-byte / 200-char context must NOT earn the >500 bonus under char rule, got {}",
        confidence
    );

    let context_501_cjk = "用".repeat(501); // > 500 chars → bonus
    let confidence = scorer.score_confidence(&content, &context_501_cjk);
    assert!(
        (confidence - 0.7).abs() < 1e-9,
        "501-char context must earn the bonus, got {}",
        confidence
    );
}

#[test]
fn test_score_recency() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);

    let now_ms = chrono::Utc::now().timestamp_millis();
    let recency = scorer.score_recency(now_ms);
    // 容差断言：δ≥1ms 时 exp(-0.1*hours) 为 1-2.8e-8 而非精确 1.0，
    // 负载下跨毫秒边界会使精确相等断言 flaky（S15 验证轮实测）。
    assert!((recency - 1.0).abs() < 1e-6, "当前时刻应约为 1.0");
}

#[test]
fn test_score_without_llm() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);

    let content = "用户喜欢 Rust";
    let embedding = vec![1.0, 0.0, 0.0];
    let existing: Vec<Vec<f32>> = vec![];
    let context = "对话上下文";
    let turn_timestamp_ms = chrono::Utc::now().timestamp_millis();

    let result = scorer
        .score(
            content,
            "preference",
            &embedding,
            &existing,
            context,
            turn_timestamp_ms,
        )
        .unwrap();

    // Utility 应该是 0.5（LLM 不可用时的默认值）
    assert_eq!(result.dimensions.utility, 0.5);
    assert_eq!(result.dimensions.novelty, 1.0);
    // 容差：同 test_score_recency——毫秒边界下 recency 为 1-2.8e-8
    assert!((result.dimensions.recency - 1.0).abs() < 1e-6);
    assert_eq!(result.dimensions.importance, 0.8); // preference

    // 检查加权分数
    let expected =
        0.5 * 0.3 + 1.0 * 0.2 + 1.0 * 0.2 + 0.8 * 0.2 + result.dimensions.confidence * 0.1;
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
    let turn_timestamp_ms = chrono::Utc::now().timestamp_millis();

    let result = scorer
        .score(
            content,
            "preference",
            &embedding,
            &existing,
            context,
            turn_timestamp_ms,
        )
        .unwrap();

    // 分数应该低于 0.8（因为 Utility 是 0.5）
    assert!(!result.admitted);
}

#[test]
fn test_parse_batch_utility_scores_validation() {
    let obj = parse_batch_utility_scores(r#"{"scores": [1.5, -0.2]}"#, 2).unwrap();
    assert_eq!(obj, vec![1.0, 0.0]);
    let bare = parse_batch_utility_scores(r#"[0.8, 0.4]"#, 2).unwrap();
    assert_eq!(bare, vec![0.8, 0.4]);
    assert!(parse_batch_utility_scores(r#"{"scores": [0.8]}"#, 2).is_err());
    assert!(parse_batch_utility_scores("not json", 2).is_err());
}

/// A5: id 锚定条目按 id 归位——乱序响应重排到 item 顺序；重复/越界 id、
/// 数量不符、非有限分数一律 bail（score_batch 据此走逐条回退）。
#[test]
fn test_parse_batch_anchored_scores_permutation_validation() {
    let ordered = parse_batch_utility_scores(
        r#"{"scores": [{"id": 1, "score": 0.9}, {"id": 2, "score": 0.1}]}"#,
        2,
    )
    .unwrap();
    assert_eq!(ordered, vec![0.9, 0.1]);

    // 置换：id=2 在前仍按 id 归位，而不是按返回顺序错配。
    let permuted = parse_batch_utility_scores(
        r#"{"scores": [{"id": 2, "score": 0.1}, {"id": 1, "score": 0.9}]}"#,
        2,
    )
    .unwrap();
    assert_eq!(permuted, vec![0.9, 0.1]);

    // clamp 与平铺路径同口径。
    let clamped = parse_batch_utility_scores(
        r#"{"scores": [{"id": 1, "score": 1.5}, {"id": 2, "score": -0.2}]}"#,
        2,
    )
    .unwrap();
    assert_eq!(clamped, vec![1.0, 0.0]);

    for bad in [
        r#"{"scores": [{"id": 1, "score": 0.9}, {"id": 1, "score": 0.1}]}"#, // 重复 id
        r#"{"scores": [{"id": 1, "score": 0.9}, {"id": 3, "score": 0.1}]}"#, // 越界 id
        r#"{"scores": [{"id": 0, "score": 0.9}, {"id": 2, "score": 0.1}]}"#, // id 从 1 起
        r#"{"scores": [{"id": 1, "score": 0.9}]}"#,                          // 数量不符
        r#"{"scores": [{"id": 1, "score": 0.9}, {"id": 2, "score": "NaN"}]}"#, // 非有限
    ] {
        assert!(
            parse_batch_utility_scores(bad, 2).is_err(),
            "must bail: {bad}"
        );
    }
}

/// A5 端到端：LLM 乱序返回锚定分数 → utility 仍按 id 绑定到正确原子
/// （第一条 decision 0.95 准入，第二条 0.05 拒绝），单次批量调用。
#[test]
fn test_score_batch_anchored_permutation_assigns_by_id() {
    let (url, calls) = crate::memory::llm::spawn_test_llm_stub(vec![
        r#"{"scores": [{"id": 2, "score": 0.05}, {"id": 1, "score": 0.95}]}"#,
    ]);
    let llm = LlmClient::new(&url, "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let now = chrono::Utc::now().timestamp_millis();
    let emb = vec![1.0, 0.0, 0.0];
    let items = [
        BatchScoreInput {
            content: "Important Rust architecture decision 2026",
            atom_type: "decision",
            embedding: &emb,
        },
        BatchScoreInput {
            content: "maybe chat",
            atom_type: "relationship",
            embedding: &emb,
        },
    ];
    let existing: [&[f32]; 0] = [];
    let results = scorer.score_batch(&items, &existing, "context", now);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(results.len(), 2);
    let first = results[0].as_ref().unwrap();
    let second = results[1].as_ref().unwrap();
    assert!((first.dimensions.utility - 0.95).abs() < 1e-9);
    assert!((second.dimensions.utility - 0.05).abs() < 1e-9);
    assert!(first.admitted);
    assert!(!second.admitted);
}

/// A5: 锚点不可信（重复 id）→ 与解析失败同姿势，回退逐条评分而不是
/// 猜测绑定。
#[test]
fn test_score_batch_invalid_anchor_falls_back_per_item() {
    let (url, calls) = crate::memory::llm::spawn_test_llm_stub(vec![
        r#"{"scores": [{"id": 1, "score": 0.9}, {"id": 1, "score": 0.1}]}"#,
        "0.9",
        "0.1",
    ]);
    let llm = LlmClient::new(&url, "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let now = chrono::Utc::now().timestamp_millis();
    let emb = vec![1.0, 0.0, 0.0];
    let items = [
        BatchScoreInput {
            content: "First fact 1234567890",
            atom_type: "decision",
            embedding: &emb,
        },
        BatchScoreInput {
            content: "maybe second",
            atom_type: "fact",
            embedding: &emb,
        },
    ];
    let existing: [&[f32]; 0] = [];
    let results = scorer.score_batch(&items, &existing, "ctx", now);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert!((results[0].as_ref().unwrap().dimensions.utility - 0.9).abs() < 1e-9);
    assert!((results[1].as_ref().unwrap().dimensions.utility - 0.1).abs() < 1e-9);
}

#[test]
fn test_score_batch_single_llm_call_for_multiple_items() {
    let (url, calls) = crate::memory::llm::spawn_test_llm_stub(vec![r#"{"scores": [0.95, 0.05]}"#]);
    let llm = LlmClient::new(&url, "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let now = chrono::Utc::now().timestamp_millis();
    let emb = vec![1.0, 0.0, 0.0];
    let items = [
        BatchScoreInput {
            content: "Important Rust architecture decision 2026",
            atom_type: "decision",
            embedding: &emb,
        },
        BatchScoreInput {
            content: "maybe chat",
            atom_type: "relationship",
            embedding: &emb,
        },
    ];
    let existing: [&[f32]; 0] = [];
    let results = scorer.score_batch(&items, &existing, "context", now);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(results.len(), 2);
    let first = results[0].as_ref().unwrap();
    let second = results[1].as_ref().unwrap();
    assert!(first.admitted);
    assert!(!second.admitted);
    assert!((first.dimensions.utility - 0.95).abs() < 1e-9);
    assert!((second.dimensions.utility - 0.05).abs() < 1e-9);
}

#[test]
fn test_score_batch_falls_back_on_malformed_json() {
    let (url, calls) =
        crate::memory::llm::spawn_test_llm_stub(vec!["not valid json", "0.9", "0.1"]);
    let llm = LlmClient::new(&url, "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let now = chrono::Utc::now().timestamp_millis();
    let emb = vec![1.0, 0.0, 0.0];
    let items = [
        BatchScoreInput {
            content: "First fact 1234567890",
            atom_type: "decision",
            embedding: &emb,
        },
        BatchScoreInput {
            content: "maybe second",
            atom_type: "fact",
            embedding: &emb,
        },
    ];
    let existing: [&[f32]; 0] = [];
    let results = scorer.score_batch(&items, &existing, "ctx", now);
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert!((results[0].as_ref().unwrap().dimensions.utility - 0.9).abs() < 1e-9);
    assert!((results[1].as_ref().unwrap().dimensions.utility - 0.1).abs() < 1e-9);
}

#[test]
fn test_score_utility_ignores_nan_from_llm() {
    let (url, _calls) = crate::memory::llm::spawn_test_llm_stub(vec!["NaN"]);
    let llm = LlmClient::new(&url, "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let score = scorer.score_utility(&llm, "fact", "ctx").unwrap();
    assert!(
        (score - 0.5).abs() < 1e-9,
        "NaN must fall back to 0.5, got {score}"
    );
}

/// M11: 非数字响应同样降级 0.5 且返回 Ok——传播错误会让 execute_score
/// 跳过该原子（丢记忆），只允许 warn + 降级。
#[test]
fn test_score_utility_unparseable_falls_back_without_error() {
    let (url, _calls) = crate::memory::llm::spawn_test_llm_stub(vec!["I cannot rate this."]);
    let llm = LlmClient::new(&url, "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let score = scorer.score_utility(&llm, "fact", "ctx").unwrap();
    assert!(
        (score - 0.5).abs() < 1e-9,
        "unparseable response must fall back to 0.5, got {score}"
    );
}

/// M11: 英文不确定词大小写无关（"Maybe"/"PROBABLY" 此前漏判 -0.1）。
#[test]
fn test_score_confidence_uncertain_english_case_insensitive() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);
    let plain = "用户喜欢使用 Rust 语言进行系统编程"; // 20 chars → sweet spot
    let base = scorer.score_confidence(plain, "短上下文");
    assert!((base - 0.6).abs() < 1e-9, "base confidence {base}");
    for prefix in ["Maybe ", "maybe ", "PROBABLY ", "Perhaps "] {
        let content = format!("{prefix}{plain}");
        let confidence = scorer.score_confidence(&content, "短上下文");
        assert!(
            (confidence - (base - 0.1)).abs() < 1e-9,
            "{content} must take the uncertainty penalty, got {confidence}"
        );
    }
}

/// W3.1: 批量网络宕机必须整批降级 utility=0.5 保留记忆（全 Ok），而非
/// 逐条 Err 被 execute_score 当 skip 丢弃。127.0.0.1:9（discard 端口）
/// 连接必拒——离线确定性，同仓库既有降级测试姿势。
#[test]
fn test_score_batch_network_outage_degrades_to_keep() {
    let llm = LlmClient::new("http://127.0.0.1:9/v1", "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let now = chrono::Utc::now().timestamp_millis();
    let emb = vec![1.0, 0.0, 0.0];
    let items = [
        BatchScoreInput {
            content: "Important Rust architecture decision 2026",
            atom_type: "decision",
            embedding: &emb,
        },
        BatchScoreInput {
            content: "maybe chat",
            atom_type: "relationship",
            embedding: &emb,
        },
    ];
    let existing: [&[f32]; 0] = [];
    let results = scorer.score_batch(&items, &existing, "context", now);
    assert_eq!(results.len(), 2);
    assert!(
        results.iter().all(Result::is_ok),
        "network outage must keep every atom, not drop via Err: {results:?}"
    );
    for r in &results {
        assert!(
            (r.as_ref().unwrap().dimensions.utility - 0.5).abs() < 1e-9,
            "outage must degrade utility to the no-LLM fallback"
        );
    }
    assert!(
        results[0].as_ref().unwrap().admitted,
        "the decision atom must survive the outage (memory kept)"
    );
}

/// W3.1（单条路径）：单原子会话走 score_with_refs → score_utility，网络
/// 宕机同样降级保留——不得让该会话唯一的 L1 原子因 Err 被跳过。
#[test]
fn test_score_batch_single_item_network_outage_degrades() {
    let llm = LlmClient::new("http://127.0.0.1:9/v1", "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let now = chrono::Utc::now().timestamp_millis();
    let emb = vec![1.0, 0.0, 0.0];
    let items = [BatchScoreInput {
        content: "Important Rust architecture decision 2026",
        atom_type: "decision",
        embedding: &emb,
    }];
    let existing: [&[f32]; 0] = [];
    let results = scorer.score_batch(&items, &existing, "ctx", now);
    assert_eq!(results.len(), 1);
    let score = results[0]
        .as_ref()
        .expect("single-item outage must keep the atom");
    assert!((score.dimensions.utility - 0.5).abs() < 1e-9);
    assert!(score.admitted);
}

/// importance 类型匹配与 confidence_level_order 同口径：trim + 大小写
/// 无关（"Fact"/"FACT" 此前落 0.5 未知档）。
#[test]
fn test_score_importance_case_and_whitespace_insensitive() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);
    assert_eq!(scorer.score_importance("Fact"), 0.7);
    assert_eq!(scorer.score_importance("FACT"), 0.7);
    assert_eq!(scorer.score_importance(" decision "), 0.9);
    assert_eq!(scorer.score_importance("PREFERENCE"), 0.8);
    assert_eq!(scorer.score_importance("Relationship"), 0.6);
    assert_eq!(scorer.score_importance("Unknown"), 0.5);
}

use crate::test_support::WarnLog;

/// C2 回归：score_utility 遇到不可解析的 LLM 响应时，warn 日志中的原始文本必须先脱敏凭据再截断。
#[test]
fn test_c2_score_utility_unparseable_redacts_credentials_in_log() {
    let synthetic_secret = ["sk-", "liveadmission9876543210abcd"].concat();
    let body =
        Box::leak(format!("invalid rating token {synthetic_secret} leaked").into_boxed_str());
    let (url, _calls) = crate::memory::llm::spawn_test_llm_stub(vec![body]);
    let llm = LlmClient::new(&url, "k", "m");
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, Some(&llm));
    let log = WarnLog::default();

    let score = crate::test_support::with_warn_default(log.clone(), || {
        scorer.score_utility(&llm, "fact", "ctx").unwrap()
    });
    assert!((score - UTILITY_FALLBACK).abs() < 1e-9);
    let msgs = log.0.lock().unwrap().clone();
    assert!(
        msgs.iter().any(|m| m.contains("[REDACTED]")),
        "credential in unparseable LLM response must be redacted in warn logs: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains(&synthetic_secret)),
        "raw secret must never appear in warn logs: {msgs:?}"
    );
}

/// M7 回归：i64::MIN / i64::MAX 极端时间差不得在 debug 模式下减法溢出 panic，
/// 且语义方向正确（极古时间衰减至 ~0.0，当前/未来时间不低于 ~1.0 并夹紧在 [0,1]）。
#[test]
fn test_m7_score_recency_extreme_min_timestamp_does_not_panic() {
    let config = default_config();
    let scorer = AdmissionScorer::new(&config, None);

    let ancient = scorer.score_recency(i64::MIN);
    assert!(
        ancient.is_finite() && (0.0..1e-6).contains(&ancient),
        "i64::MIN ancient timestamp must decay near 0.0, got {ancient}"
    );

    let now_ms = chrono::Utc::now().timestamp_millis();
    let fresh = scorer.score_recency(now_ms);
    assert!(
        fresh > 0.99 && fresh <= 1.0,
        "current timestamp must be near 1.0, got {fresh}"
    );

    let future = scorer.score_recency(i64::MAX);
    assert_eq!(
        future, 1.0,
        "far-future timestamp must clamp cleanly to 1.0 without overflow"
    );
}
