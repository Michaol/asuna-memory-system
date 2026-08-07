//! Retrieval benchmark baseline (v2.6 Step 7).
//!
//! Fixture-based quality + latency measurement for the retrieval paths,
//! per docs/plans/2026-08-07-v2.6-design.md section 7. Chinese corpus with
//! golden relevance judgments; metrics: Recall@5, MRR, p50/p95 latency.
//!
//! Marked #[ignore]: run explicitly with
//!   cargo test -- --ignored retrieval_benchmark
//!
//! Baseline discipline: measure BEFORE changing retrieval code, then re-run
//! after (v2.6.0 gates: budget truncation must not hurt recall; rerank asset
//! release is contingent on a measured win; v2.6.1 consolidation must not
//! regress the recall surface).
//!
//! Recorded baseline (v2.5.3 worktree measurement, identical on v2.6.0):
//!   Success@5: 1.000 · Recall@5: 0.100 (structural cap) · MRR: 0.833
//!   latency: p50≈0.22ms p95≈0.34ms (print-only, machine-dependent)

#[cfg(test)]
mod tests {
    use crate::fact::search::{search_sessions, SearchMode, SearchParams};
    use crate::index::db::Db;
    use std::time::Instant;

    /// Corpus: 4 topics × 10 turns each + 4 distractor turns.
    /// Each topic's turns share distinctive vocabulary so golden judgments
    /// are deterministic by construction.
    fn topics() -> Vec<(&'static str, Vec<&'static str>)> {
        vec![
            (
                "rust",
                vec![
                    "用户在调试 Rust 的所有权问题，borrow checker 报错了",
                    "讨论 Rust 的生命周期标注，泛型函数的 lifetime 推导",
                    "用户想用 Rust 重写网关的热路径，关心零成本抽象",
                    "Rust 的 cargo workspaces 多 crate 组织结构讨论",
                    "用户遇到 Rust 异步编程的 Pin 和 Future 问题",
                    "比较 Rust 与 Go 的并发模型，channel 对比 async",
                    "用户在写 Rust 的过程宏，syn 和 quote 的用法",
                    "Rust 编译器升级后出现新的 clippy 警告",
                    "用户优化 Rust 二进制的体积，strip 和 lto 配置",
                    "讨论 Rust 的错误处理，anyhow 与 thiserror 的选择",
                ],
            ),
            (
                "烹饪",
                vec![
                    "用户学做红烧肉，炒糖色的火候掌握不好",
                    "讨论意面的酱汁，番茄肉酱要炖多久",
                    "用户烤面包失败，面团发酵时间不够",
                    "中餐的刀工练习，切丝切片的基本功",
                    "用户问牛排的熟度，三分熟和五分熟的区别",
                    "煲汤的技巧，排骨玉米汤要去血沫",
                    "用户尝试做寿司，米饭的酸碱度和卷法",
                    "讨论烘焙的称量精度，酵母比例的影响",
                    "用户的早餐计划，燕麦粥配水果坚果",
                    "川菜麻辣味的调配，花椒和辣椒的比例",
                ],
            ),
            (
                "旅行",
                vec![
                    "用户计划去京都旅行，想看枫叶季的红叶",
                    "讨论冰岛的自驾环岛路线，租车注意事项",
                    "用户订了去云南的机票，大理丽江的行程",
                    "旅行打包清单，转换插头和常用药品",
                    "用户在比较酒店和民宿，位置与价格的权衡",
                    "新西兰南岛的徒步路线，米尔福德峡湾",
                    "用户的签证办理进度，材料准备清单",
                    "讨论旅行保险的选择，航班延误的理赔",
                    "用户想去葡萄牙的里斯本，蛋挞店推荐",
                    "长途飞行的时差调整，褪黑素的使用",
                ],
            ),
            (
                "项目",
                vec![
                    "Asuna 记忆系统的 SQLite 分库方案讨论",
                    "用户项目的发布流程，CI 流水线的测试门禁",
                    "讨论数据库备份策略，增量备份与恢复演练",
                    "用户要写项目的技术文档，架构图的画法",
                    "生产环境的监控告警，错误率的阈值设置",
                    "用户评估依赖升级的风险，语义化版本的约束",
                    "讨论代码审查规范，提交信息的格式约定",
                    "用户项目的性能预算，启动时间的优化目标",
                    "灰度发布的策略，按用户百分比放量",
                    "讨论事故复盘流程，根因分析的五个为什么",
                ],
            ),
        ]
    }

    /// Golden queries → expected topic. Queries are consecutive-token phrases
    /// that occur verbatim in exactly one topic (jieba FTS is a phrase match,
    /// so free-form multi-word questions would not hit — that is the system's
    /// documented keyword behavior, not a benchmark artifact).
    fn golden_queries() -> Vec<(&'static str, &'static str)> {
        vec![
            ("生命周期", "rust"),
            ("红烧肉", "烹饪"),
            ("枫叶季", "旅行"),
            ("数据库备份", "项目"),
            ("cargo workspaces", "rust"),
            ("航班延误", "旅行"),
        ]
    }

    fn seed_corpus(db: &Db) -> std::collections::HashMap<&'static str, Vec<i64>> {
        let conn = db.conn();
        let mut relevant: std::collections::HashMap<&'static str, Vec<i64>> =
            std::collections::HashMap::new();

        for (topic, turns) in topics() {
            let session_id = format!("bench-{}", topic);
            conn.execute(
                "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                 VALUES (?1, 1000, 'bench.jsonl', 1000, 1000)",
                rusqlite::params![session_id],
            )
            .unwrap();
            for (i, preview) in turns.iter().enumerate() {
                conn.execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                     VALUES (?1, ?2, ?3, 'user', ?4)",
                    rusqlite::params![session_id, (i + 1) as i64, 2000 + i as i64, preview],
                )
                .unwrap();
                let id: i64 = conn
                    .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                    .unwrap();
                relevant.entry(topic).or_default().push(id);
            }
        }

        // Distractors: each contains ONE golden phrase verbatim but belongs to
        // no topic — gives every query a competing candidate so MRR is a real
        // ranking signal (inert distractors sharing no tokens test nothing
        // under phrase-match FTS). Lengths deliberately differ from the target
        // turns so bm25 ranks are not tied.
        conn.execute(
            "INSERT INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
             VALUES ('bench-distract', 1000, 'bench.jsonl', 1000, 1000)",
            [],
        )
        .unwrap();
        let distractors = [
            "这部纪录片讲了动物生命周期，拍得很美",
            "整理相册发现一张红烧肉的招牌照片，拍糊了",
            "枫叶季的壁纸换上了，桌面看着挺舒服",
            "整理旧硬盘，数据库备份文件占了大半空间",
            "文章里提到 cargo workspaces 这个词，没细看",
            "新闻说昨天大面积航班延误，机场人山人海",
        ];
        for (i, preview) in distractors.iter().enumerate() {
            conn.execute(
                "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                 VALUES ('bench-distract', ?1, ?2, 'assistant', ?3)",
                rusqlite::params![(i + 1) as i64, 9000 + i as i64, preview],
            )
            .unwrap();
        }

        relevant
    }

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
    /// Retrieval benchmark baseline. #[ignore]: run with
    /// `cargo test -- --ignored retrieval_benchmark --nocapture`
    #[test]
    #[ignore]
    fn test_retrieval_benchmark_baseline() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let relevant = seed_corpus(&db);
        let queries = golden_queries();

        let k = 5usize;
        let iterations = 20usize;

        let mut recall_scores: Vec<f64> = Vec::new();
        let mut reciprocal_ranks: Vec<f64> = Vec::new();
        let mut latencies_ms: Vec<f64> = Vec::new();

        for _ in 0..iterations {
            for (query, topic) in &queries {
                let params = SearchParams {
                    query: query.to_string(),
                    search_mode: SearchMode::Keyword,
                    top_k: k,
                    after_ms: None,
                    before_ms: None,
                    role: None,
                };

                let start = Instant::now();
                let results = search_sessions(&db, None, &params).unwrap();
                latencies_ms.push(start.elapsed().as_secs_f64() * 1000.0);

                let expected = &relevant[*topic];
                let hits = results
                    .iter()
                    .filter(|r| expected.contains(&r.turn_id))
                    .count();

                // Recall@5 over the full relevance set
                recall_scores.push(hits as f64 / expected.len() as f64);

                // MRR: rank of the first relevant hit
                let rr = results
                    .iter()
                    .position(|r| expected.contains(&r.turn_id))
                    .map(|pos| 1.0 / (pos as f64 + 1.0))
                    .unwrap_or(0.0);
                reciprocal_ranks.push(rr);
            }
        }

        let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
        latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let success5 = recall_scores.iter().filter(|&&r| r > 0.0).count() as f64
            / recall_scores.len() as f64;
        let recall5 = mean(&recall_scores);
        let mrr = mean(&reciprocal_ranks);
        let p50 = percentile(&latencies_ms, 0.50);
        let p95 = percentile(&latencies_ms, 0.95);
        let corpus_turns: usize = relevant.values().map(|v| v.len()).sum::<usize>() + 6;

        println!("═══ AMS retrieval benchmark (keyword, top_k={}) ═══", k);
        println!(
            "corpus: {} turns, {} golden queries, {} iterations",
            corpus_turns,
            queries.len(),
            iterations
        );
        println!("Success@5: {:.3}  (queries with ≥1 relevant hit in top-{})", success5, k);
        println!(
            "Recall@5 : {:.3}  (phrase-match structural cap = 1/|turns-per-topic|, informational)",
            recall5
        );
        println!("MRR      : {:.3}", mrr);
        println!("latency  : p50={:.2}ms  p95={:.2}ms", p50, p95);

        // Primary gate: every query must surface its topic in top-5.
        assert!(
            (success5 - 1.0).abs() < 1e-9,
            "Success@5 {:.3} < 1.0 — a query lost its topic, retrieval regression",
            success5
        );
        // Ranking gate with competing distractor candidates. Floor recorded
        // from the measured v2.5.3/v2.6.0 baseline; a rerank or fusion change
        // that demotes exact matches below this trips it.
        assert!(
            mrr >= 0.65,
            "MRR {:.3} below baseline floor 0.65 — ranking regression",
            mrr
        );
    }

    /// Non-ignored smoke variant: one pass, loose assertions, keeps the
    /// benchmark plumbing honest in normal CI runs without the #[ignore] gate.
    #[test]
    fn test_retrieval_benchmark_smoke() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let relevant = seed_corpus(&db);

        let params = SearchParams {
            query: "生命周期".to_string(),
            search_mode: SearchMode::Keyword,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        let results = search_sessions(&db, None, &params).unwrap();
        assert!(
            results.iter().any(|r| relevant["rust"].contains(&r.turn_id)),
            "smoke: keyword search must find the rust topic"
        );
    }
}
