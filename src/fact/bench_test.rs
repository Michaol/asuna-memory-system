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

    /// Golden queries → expected topic. Queries are concise single-token or
    /// multi-word phrases that identify one distinct topic (the system uses
    /// jieba-tokenized FTS5 AND with automatic OR fallback for multi-term queries).
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

        let success5 =
            recall_scores.iter().filter(|&&r| r > 0.0).count() as f64 / recall_scores.len() as f64;
        let recall5 = mean(&recall_scores);
        let mrr = mean(&reciprocal_ranks);
        let p50 = percentile(&latencies_ms, 0.50);
        let p95 = percentile(&latencies_ms, 0.95);
        let corpus_turns: usize = relevant.values().map(Vec::len).sum::<usize>() + 6;

        println!("═══ AMS retrieval benchmark (keyword, top_k={}) ═══", k);
        println!(
            "corpus: {} turns, {} golden queries, {} iterations",
            corpus_turns,
            queries.len(),
            iterations
        );
        println!(
            "Success@5: {:.3}  (queries with ≥1 relevant hit in top-{})",
            success5, k
        );
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
            results
                .iter()
                .any(|r| relevant["rust"].contains(&r.turn_id)),
            "smoke: keyword search must find the rust topic"
        );
    }

    /// Expanded 54-query, 8-stratum retrieval & recall benchmark (`T1.11`).
    /// Run explicitly via:
    /// `cargo test --locked --release -- --ignored retrieval_benchmark --nocapture`
    #[test]
    #[ignore]
    fn test_retrieval_benchmark_expanded() {
        super::expanded::run_expanded_benchmark();
    }
}

#[cfg(test)]
mod expanded {
    use crate::embedder::quantize_to_int8;
    use crate::fact::search::{search_sessions_with_vec, SearchMode, SearchParams};
    use crate::graph::{assert_triples, TripleInput};
    use crate::index::db::Db;
    use crate::index::vector::VectorStore;
    use crate::memory::graph_integration::{
        integrate_atom_with_graph_on_conn, multi_hop_query_with_truncation,
    };
    use crate::memory::retrieval::{RetrievalEngine, RECALL_BANNER};
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::Instant;
    use tempfile::TempDir;

    const T_OLD: i64 = 1_700_000_000_000;
    const T_MID: i64 = 1_750_000_000_000;
    const T_NEW: i64 = 1_775_000_000_000;

    fn percentile(sorted: &[f64], p: f64) -> f64 {
        if sorted.is_empty() {
            return 0.0;
        }
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }

    fn normalize_vec(vec: &mut [f32]) {
        let norm = vec
            .iter()
            .map(|&x| f64::from(x) * f64::from(x))
            .sum::<f64>()
            .sqrt();
        if norm > 0.0 && norm.is_finite() {
            let inv = 1.0 / norm;
            for x in vec.iter_mut() {
                *x = (f64::from(*x) * inv) as f32;
            }
        }
    }

    fn synthetic_topic_vec(topic_slot: usize, noise_seed: usize) -> Vec<f32> {
        let mut vec = vec![0.0f32; 1024];
        vec[topic_slot % 64] = 1.0;
        for step in 0..4usize {
            let axis = 128 + ((noise_seed.wrapping_mul(37).wrapping_add(step * 53)) % 128);
            vec[axis] += 0.03;
        }
        normalize_vec(&mut vec);
        vec
    }

    fn antipodal_topic_vec(avoid_slot: usize, alt_slot: usize, seed: usize) -> Vec<f32> {
        let mut vec = vec![0.0f32; 1024];
        vec[avoid_slot % 64] = -0.8;
        vec[64 + (alt_slot % 64)] = 0.6;
        let axis = 256 + (seed % 128);
        vec[axis] = 0.05;
        normalize_vec(&mut vec);
        vec
    }

    struct SeedCtx<'a> {
        db: &'a Db,
        vecs: VectorStore<'a>,
    }

    struct TurnSeed<'a> {
        session_id: &'a str,
        seq: i64,
        ts: i64,
        role: &'a str,
        preview: &'a str,
        emb: &'a [f32],
    }

    impl<'a> SeedCtx<'a> {
        fn new(db: &'a Db) -> Self {
            Self {
                db,
                vecs: VectorStore::new(db),
            }
        }

        fn ensure_session(&self, session_id: &str, start_ts: i64) {
            self.db
                .conn()
                .execute(
                    "INSERT OR IGNORE INTO sessions (session_id, start_ts, file_path, created_at, updated_at)
                     VALUES (?1, ?2, 'bench_expanded.jsonl', ?2, ?2)",
                    rusqlite::params![session_id, start_ts],
                )
                .unwrap();
        }

        fn insert_turn(&self, row: &TurnSeed<'_>) -> i64 {
            self.ensure_session(row.session_id, row.ts);
            self.db
                .conn()
                .execute(
                    "INSERT INTO turns (session_id, seq, timestamp_ms, role, preview)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![row.session_id, row.seq, row.ts, row.role, row.preview],
                )
                .unwrap();
            let id: i64 = self
                .db
                .conn()
                .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
                .unwrap();
            self.vecs.insert(id, row.emb).unwrap();
            id
        }
    }

    fn insert_l1_atom(
        db: &Db,
        content: &str,
        ts: i64,
        confidence: &str,
        supersedes_id: Option<i64>,
        emb: Option<&[f32]>,
    ) -> i64 {
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
                 VALUES ('memory', ?1, ?2, ?2, ?3, 'atom', ?4)",
                rusqlite::params![content, ts, confidence, supersedes_id],
            )
            .unwrap();
        let id: i64 = db
            .conn()
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .unwrap();
        if let Some(vec) = emb {
            let bytes = quantize_to_int8(vec);
            db.conn()
                .execute(
                    "INSERT INTO vec_bounded_memory (id, embedding) VALUES (?1, vec_int8(?2))",
                    rusqlite::params![id, bytes],
                )
                .unwrap();
        }
        id
    }

    struct TimeRoleSpec {
        query: &'static str,
        target_id: i64,
        forbidden_ids: [i64; 2],
        mode: SearchMode,
        query_vec: Option<Vec<f32>>,
        after_ms: Option<i64>,
        before_ms: Option<i64>,
        role: Option<&'static str>,
    }

    struct SupersededSpec {
        query: &'static str,
        live_marker: &'static str,
        legacy_marker: &'static str,
        query_vec: Vec<f32>,
    }

    struct GraphAtoms {
        asuna: i64,
        sqlite: i64,
        fts: i64,
        hermes: i64,
        int8: i64,
    }

    struct ExpandedCorpus {
        s1_kw: Vec<(&'static str, i64)>,
        s2_or: Vec<(&'static str, i64)>,
        s3_mix: Vec<(&'static str, i64)>,
        s4_dist: Vec<(&'static str, i64)>,
        s5_paraphrase: Vec<(&'static str, i64, Vec<f32>)>,
        s5_hybrid_gain: Vec<(&'static str, i64, Vec<f32>)>,
        s6_filters: Vec<TimeRoleSpec>,
        s7_superseded: Vec<SupersededSpec>,
        graph_atoms: GraphAtoms,
    }

    struct Stratum1to3 {
        s1: Vec<(&'static str, i64)>,
        s2: Vec<(&'static str, i64)>,
        s3: Vec<(&'static str, i64)>,
    }

    struct S5Seeds {
        paraphrase: Vec<(&'static str, i64, Vec<f32>)>,
        hybrid_gain: Vec<(&'static str, i64, Vec<f32>)>,
    }

    fn seed_keyword_stratum(
        ctx: &SeedCtx<'_>,
        session_id: &str,
        slot_base: usize,
        pairs: &[(&'static str, &'static str, [&'static str; 2])],
    ) -> Vec<(&'static str, i64)> {
        let mut out = Vec::with_capacity(pairs.len());
        let mut seq = 1i64;
        for (i, &(query, target_text, bg)) in pairs.iter().enumerate() {
            let emb = synthetic_topic_vec(slot_base + (i % 8), i * 10);
            let id = ctx.insert_turn(&TurnSeed {
                session_id,
                seq,
                ts: T_MID + seq,
                role: "user",
                preview: target_text,
                emb: &emb,
            });
            seq += 1;
            for (j, filler) in bg.iter().enumerate() {
                let bg_emb = synthetic_topic_vec(slot_base + (i % 8), i * 10 + j + 1);
                ctx.insert_turn(&TurnSeed {
                    session_id,
                    seq,
                    ts: T_MID + seq,
                    role: "assistant",
                    preview: filler,
                    emb: &bg_emb,
                });
                seq += 1;
            }
            out.push((query, id));
        }
        out
    }

    fn seed_s1_s2_s3(ctx: &SeedCtx<'_>) -> Stratum1to3 {
        let s1_pairs = [
            (
                "数据库分片",
                "用户对比了按租户哈希路由的数据库分片方案与跨库事务代价",
                [
                    "早期评估过单机主从复制与读写分离的运维开销",
                    "讨论连接池大小对在线报表延迟的影响",
                ],
            ),
            (
                "容器编排",
                "集群调度团队讨论容器编排中的滚动升级与亲和性规则",
                [
                    "记录了节点磁盘水位监控面板的告警阈值",
                    "对比物理机裸金属部署与虚拟化网卡性能",
                ],
            ),
            (
                "数字签名",
                "制品发布管道启用非对称数字签名以校验二进制完整性",
                [
                    "梳理了证书吊销列表定期同步的任务清单",
                    "检查构建机时间同步精度避免验签失败",
                ],
            ),
            (
                "热更新",
                "长连接网关需要支持配置热更新而不中断现有会话",
                [
                    "测试优雅停机排空期间旧连接的超时时长",
                    "统计路由表重载后的内存峰值波动范围",
                ],
            ),
            (
                "分布式锁",
                "定时任务选主机制采用基于租约续期的分布式锁避免双活",
                [
                    "记录时钟偏移对租约过期的潜在边界影响",
                    "测试后台心跳保活在网络抖动下的重试策略",
                ],
            ),
            (
                "声学模型",
                "语音识别流水线正在评估端到端声学模型在噪声下的鲁棒性",
                [
                    "整理降噪前端采样率重采样滤波器的参数设置",
                    "标注多方言测试集在安静环境下的字错率",
                ],
            ),
            (
                "回测框架",
                "量化策略研究员对比事件驱动回测框架的滑点模拟精度",
                [
                    "清理历史行情缺失分钟线的插值修复规则",
                    "核算手续费与冲击成本在调仓周期里的占比",
                ],
            ),
            (
                "逃逸分析",
                "编译器通过逃逸分析决定对象是否可以直接在栈上分配",
                [
                    "观察泛型单态化之后指令缓存命中的变化趋势",
                    "分析内联阈值调优对热点函数帧大小的影响",
                ],
            ),
        ];
        let s1 = seed_keyword_stratum(ctx, "bench-s1-zh", 0, &s1_pairs);

        let s2_pairs = [
            (
                "微服务的熔断保护",
                "高并发场景下，微服务链路通过自适应限流与熔断策略实现优雅降级保护",
                [
                    "网关按上游错误率统计窗口采样指标",
                    "服务治理平台支持动态调整超时预算",
                ],
            ),
            (
                "索引的压缩算法",
                "列式存储引擎对倒排索引执行位图编码压缩并采用帧级解码算法节省空间",
                [
                    "扫描冷数据时优先顺序预读邻近数据页",
                    "统计块头校验和在读放大中的占比",
                ],
            ),
            (
                "日志的脱敏规范",
                "审计系统在写入持久化日志之前必须执行凭据脱敏并遵循安全规范",
                [
                    "保留最近三十天审计流水的归档周期",
                    "统一各模块错误码字段的命名格式",
                ],
            ),
            (
                "缓存的雪崩防护",
                "网关层在本地缓存失效瞬间加入随机抖动避免雪崩从而加强整体防护",
                [
                    "热点键值设置分级过期时长平滑淘汰压力",
                    "记录本地二级命中率监控仪表盘变化",
                ],
            ),
            (
                "队列的积压告警",
                "异步消息队列在消费延迟升高产生积压时监控平台会触发阈值告警",
                [
                    "死信通道保留失败样本便于人工排查重放",
                    "消费者实例按分片数量弹性扩缩容",
                ],
            ),
            (
                "镜像的瘦身技巧",
                "多阶段构建配合静态链接能够让生产容器镜像大幅瘦身是一类实用技巧",
                [
                    "清理包管理器下载残余缓存减少构建层数",
                    "校验基础运行时只保留必要根证书",
                ],
            ),
            (
                "事务的隔离级别",
                "金融账务核心表要求数据库事务维持严格隔离并采用可串行化级别",
                [
                    "写前日志定期执行检查点缩短恢复时长",
                    "检测长读查询对版本链清理速度的影响",
                ],
            ),
            (
                "线程的上下文切换",
                "过多的阻塞工作线程会反复保存上下文状态并引发高频内核切换",
                [
                    "异步运行时把阻塞文件读写移入专用线程池",
                    "绑定工作负载核心核数减少缓存抖动",
                ],
            ),
        ];
        let s2 = seed_keyword_stratum(ctx, "bench-s2-or", 8, &s2_pairs);

        let s3_pairs = [
            (
                "asuna-memory 架构",
                "核心记忆服务 asuna-memory 采用单库多表与混合检索的三层架构",
                [
                    "梳理外置测试拆分与契约边界清单",
                    "核对跨模块依赖的编译隔离顺序",
                ],
            ),
            (
                "bounded_memory_fts 重建",
                "升级到新版分词器时必须在单事务内执行 bounded_memory_fts 的完整重建",
                [
                    "确认触发器删改动作与虚表内容一致",
                    "记录增量回填与全量清表的锁定范围",
                ],
            ),
            (
                "config.json 校验",
                "服务启动阶段加载 config.json 并执行配置项边界与鉴权开关的严格校验",
                [
                    "校验环境覆写变量去除前后空白字符",
                    "检查非回环监听地址强制启用密钥验证",
                ],
            ),
            (
                "v3.0.0 发布",
                "团队在 v3.0.0 里程碑发布前必须冻结磁盘格式契约与特征化基线",
                [
                    "核对跨语言插件版本号清单保持严格同步",
                    "验证镜像构建产物的健康探针响应",
                ],
            ),
            (
                "vec_bounded_memory 量化",
                "向量表 vec_bounded_memory 统一采用 int8 余弦距离量化以缩减内存占用",
                [
                    "记录零向量与非有限分量过滤保护逻辑",
                    "测试归一化之后绝对最大标度量化精度",
                ],
            ),
            (
                "sqlite-vec 扩展",
                "本地嵌入式库通过静态注册 sqlite-vec 扩展实现无外部服务的近邻搜索",
                [
                    "验证连接打开阶段自动加载扩展返回成功",
                    "检查分页阈值之外候选集的截断行为",
                ],
            ),
            (
                "user_version 迁移",
                "数据库打开时根据 user_version 编号触发原子化表结构迁移与回填流程",
                [
                    "确认旧版零版本历史表同样纳入升级检测",
                    "测试异常中断回滚不污染当前版本号",
                ],
            ),
            (
                "max_tokens 预算",
                "渐进披露召回接口按 max_tokens 预算自上而下整项截断上下文片段",
                [
                    "保护层保留前导边界防止单个长画像饿死",
                    "统计各层字符估算模型与截断标记",
                ],
            ),
        ];
        let s3 = seed_keyword_stratum(ctx, "bench-s3-mix", 16, &s3_pairs);

        Stratum1to3 { s1, s2, s3 }
    }

    fn seed_s4_distractors(ctx: &SeedCtx<'_>) -> Vec<(&'static str, i64)> {
        let specs = [
            (
                "内核 事件循环 零拷贝",
                "内核 事件循环 与 零拷贝 环形缓冲区设计",
            ),
            ("编译器 词法分析 语法树", "编译器 词法分析 构建 语法树 节点"),
            ("限流 令牌桶 漏桶", "限流 组件对比 令牌桶 与 漏桶 模型"),
            (
                "分片 一致性哈希 虚拟节点",
                "分片 路由使用 一致性哈希 与 虚拟节点 均衡",
            ),
            ("索引 倒排表 跳表", "索引 结构结合 倒排表 与 跳表 交集加速"),
            (
                "存储 写前日志 校验和",
                "存储 引擎写入 写前日志 携带页级 校验和",
            ),
            (
                "网络 多路复用 边缘触发",
                "网络 模型启用 多路复用 与 边缘触发 回调",
            ),
            (
                "安全 常量时间 摘要",
                "安全 认证采取 常量时间 比对与摘要验证",
            ),
        ];
        let mut out = Vec::with_capacity(specs.len());
        let mut seq = 1i64;
        for (i, &(query, concise_target)) in specs.iter().enumerate() {
            let terms: Vec<&str> = query.split_whitespace().collect();
            let emb = synthetic_topic_vec(56 + (i % 8), 200 + i);
            let id = ctx.insert_turn(&TurnSeed {
                session_id: "bench-s4-dist",
                seq,
                ts: T_MID + seq,
                role: "user",
                preview: concise_target,
                emb: &emb,
            });
            seq += 1;
            for d in 0..3usize {
                let noisy = format!(
                    "今天站会讨论了许多琐碎事务和前端排版细节，顺便提到了一句 {} 相关背景，接着又聊了半天值班排期，\
                     中间穿插 {} 的常规运维巡检记录，最后临近散会才简要带过 {} 但完全不是主题重点内容。",
                    terms[0], terms[1], terms[2]
                );
                let d_emb = synthetic_topic_vec(56 + (i % 8), 220 + i * 3 + d);
                ctx.insert_turn(&TurnSeed {
                    session_id: "bench-s4-dist",
                    seq,
                    ts: T_MID + seq,
                    role: "assistant",
                    preview: &noisy,
                    emb: &d_emb,
                });
                seq += 1;
            }
            out.push((query, id));
        }
        out
    }

    fn seed_s5_semantic_and_hybrid(ctx: &SeedCtx<'_>) -> S5Seeds {
        let paraphrases = [
            (
                20usize,
                "苹果派烘焙技法",
                "把甜脆红富士水果切块与肉桂黄油包进酥脆黄油塔皮中低温烤制半小时",
            ),
            (
                21usize,
                "量子纠缠原理",
                "相距光年的双光子共享不可分离的波函数在测量瞬间呈现联动坍缩特性",
            ),
            (
                22usize,
                "极光产生过程",
                "太阳风高能带电粒子沿地磁场磁力线沉降撞击高层大气释放绚丽辉光",
            ),
            (
                23usize,
                "潮汐锁定机制",
                "卫星自转周期与公转周期在漫长引力耗散扭矩作用下达成完全同步",
            ),
        ];
        let mut para_out = Vec::with_capacity(paraphrases.len());
        let mut seq = 1i64;
        for (i, &(slot, query, target)) in paraphrases.iter().enumerate() {
            let q_vec = synthetic_topic_vec(slot, 900 + i);
            let t_vec = synthetic_topic_vec(slot, 920 + i);
            let id = ctx.insert_turn(&TurnSeed {
                session_id: "bench-s5-para",
                seq,
                ts: T_MID + seq,
                role: "user",
                preview: target,
                emb: &t_vec,
            });
            seq += 1;
            for j in 0..2usize {
                let other_vec = synthetic_topic_vec(50 + j, 940 + i * 2 + j);
                ctx.insert_turn(&TurnSeed {
                    session_id: "bench-s5-para",
                    seq,
                    ts: T_MID + seq,
                    role: "assistant",
                    preview: "与查询毫不相干的日常会议记录与环境巡检说明条目",
                    emb: &other_vec,
                });
                seq += 1;
            }
            para_out.push((query, id, q_vec));
        }

        let hybrid_specs = [
            (24usize, "红杉基准", "红杉基准 长文深度报告详细阐述系统如何依赖混合向量召回与词频信号协同排序并在复杂长对话窗口里稳定锁定真实核心段落的工程实战经验", ["红杉基准 红杉基准 简短噪声一", "红杉基准 红杉基准 简短噪声二"]),
            (25usize, "青铜基准", "青铜基准 长文深度报告详细阐述系统如何依赖混合向量召回与词频信号协同排序并在复杂长对话窗口里稳定锁定真实核心段落的工程实战经验", ["青铜基准 青铜基准 简短噪声一", "青铜基准 青铜基准 简短噪声二"]),
            (26usize, "琥珀基准", "琥珀基准 长文深度报告详细阐述系统如何依赖混合向量召回与词频信号协同排序并在复杂长对话窗口里稳定锁定真实核心段落的工程实战经验", ["琥珀基准 琥珀基准 简短噪声一", "琥珀基准 琥珀基准 简短噪声二"]),
            (27usize, "玄铁基准", "玄铁基准 长文深度报告详细阐述系统如何依赖混合向量召回与词频信号协同排序并在复杂长对话窗口里稳定锁定真实核心段落的工程实战经验", ["玄铁基准 玄铁基准 简短噪声一", "玄铁基准 玄铁基准 简短噪声二"]),
        ];
        let mut hybrid_out = Vec::with_capacity(hybrid_specs.len());
        for (i, &(slot, query, long_target, [d1, d2])) in hybrid_specs.iter().enumerate() {
            let q_vec = synthetic_topic_vec(slot, 960 + i);
            let t_vec = synthetic_topic_vec(slot, 970 + i);
            let id = ctx.insert_turn(&TurnSeed {
                session_id: "bench-s5-hyb",
                seq,
                ts: T_MID + seq,
                role: "user",
                preview: long_target,
                emb: &t_vec,
            });
            seq += 1;
            for (j, d_text) in [d1, d2].iter().enumerate() {
                let d_vec = antipodal_topic_vec(slot, i * 2 + j, 980 + i * 2 + j);
                ctx.insert_turn(&TurnSeed {
                    session_id: "bench-s5-hyb",
                    seq,
                    ts: T_MID + seq,
                    role: "assistant",
                    preview: d_text,
                    emb: &d_vec,
                });
                seq += 1;
            }
            hybrid_out.push((query, id, q_vec));
        }

        S5Seeds {
            paraphrase: para_out,
            hybrid_gain: hybrid_out,
        }
    }

    struct FilterSeedCase {
        tok: &'static str,
        ok_ts: i64,
        ok_role: &'static str,
        bad1_ts: i64,
        bad1_role: &'static str,
        bad2_ts: i64,
        bad2_role: &'static str,
        mode: SearchMode,
        after_ms: Option<i64>,
        before_ms: Option<i64>,
        role: Option<&'static str>,
        slot: usize,
    }

    fn seed_s6_filters_and_fillers(ctx: &SeedCtx<'_>) -> Vec<TimeRoleSpec> {
        let cases = [
            FilterSeedCase {
                tok: "FLT_AFTER_TOK",
                ok_ts: T_NEW,
                ok_role: "user",
                bad1_ts: T_OLD,
                bad1_role: "user",
                bad2_ts: T_NEW - 1001,
                bad2_role: "assistant",
                mode: SearchMode::Keyword,
                after_ms: Some(T_NEW - 1000),
                before_ms: None,
                role: None,
                slot: 28,
            },
            FilterSeedCase {
                tok: "FLT_BEFORE_TOK",
                ok_ts: T_OLD,
                ok_role: "user",
                bad1_ts: T_NEW,
                bad1_role: "user",
                bad2_ts: T_OLD + 1001,
                bad2_role: "assistant",
                mode: SearchMode::Keyword,
                after_ms: None,
                before_ms: Some(T_OLD + 1000),
                role: None,
                slot: 29,
            },
            FilterSeedCase {
                tok: "FLT_WINDOW_TOK",
                ok_ts: T_MID,
                ok_role: "assistant",
                bad1_ts: T_MID - 1001,
                bad1_role: "assistant",
                bad2_ts: T_MID + 1001,
                bad2_role: "user",
                mode: SearchMode::Hybrid,
                after_ms: Some(T_MID - 1000),
                before_ms: Some(T_MID + 1000),
                role: None,
                slot: 30,
            },
            FilterSeedCase {
                tok: "FLT_USER_TOK",
                ok_ts: T_MID,
                ok_role: "user",
                bad1_ts: T_MID,
                bad1_role: "assistant",
                bad2_ts: T_MID,
                bad2_role: "system",
                mode: SearchMode::Keyword,
                after_ms: None,
                before_ms: None,
                role: Some("user"),
                slot: 31,
            },
            FilterSeedCase {
                tok: "FLT_ASST_TOK",
                ok_ts: T_NEW,
                ok_role: "assistant",
                bad1_ts: T_NEW,
                bad1_role: "user",
                bad2_ts: T_NEW,
                bad2_role: "system",
                mode: SearchMode::Hybrid,
                after_ms: None,
                before_ms: None,
                role: Some("assistant"),
                slot: 32,
            },
        ];
        let mut specs = Vec::with_capacity(cases.len());
        let mut seq = 1i64;
        for (i, c) in cases.into_iter().enumerate() {
            let vec_match = synthetic_topic_vec(c.slot, 700 + i);
            let target_txt = format!("{} 命中时间窗与角色约束的正确目标轮次", c.tok);
            let id = ctx.insert_turn(&TurnSeed {
                session_id: "bench-s6-flt",
                seq,
                ts: c.ok_ts,
                role: c.ok_role,
                preview: &target_txt,
                emb: &vec_match,
            });
            seq += 1;

            let bad1_txt = format!("{} 越界干扰项一不应被返回", c.tok);
            let bad1_id = ctx.insert_turn(&TurnSeed {
                session_id: "bench-s6-flt",
                seq,
                ts: c.bad1_ts,
                role: c.bad1_role,
                preview: &bad1_txt,
                emb: &vec_match,
            });
            seq += 1;

            let bad2_txt = format!("{} 越界干扰项二不应被返回", c.tok);
            let bad2_id = ctx.insert_turn(&TurnSeed {
                session_id: "bench-s6-flt",
                seq,
                ts: c.bad2_ts,
                role: c.bad2_role,
                preview: &bad2_txt,
                emb: &vec_match,
            });
            seq += 1;

            specs.push(TimeRoleSpec {
                query: c.tok,
                target_id: id,
                forbidden_ids: [bad1_id, bad2_id],
                mode: c.mode,
                query_vec: (c.slot >= 30 && c.tok != "FLT_USER_TOK").then_some(vec_match),
                after_ms: c.after_ms,
                before_ms: c.before_ms,
                role: c.role,
            });
        }

        // Seed 96 neutral background turns (+1 S8 recall anchor turn = 240 total L0 turns).
        for i in 0..96usize {
            let text = format!("常规运维背景日志流水编号 {i:03}，记录例行巡检状态正常且耗时平稳。");
            let emb = synthetic_topic_vec(34 + (i % 16), 1100 + i);
            ctx.insert_turn(&TurnSeed {
                session_id: "bench-bg-filler",
                seq: (i + 1) as i64,
                ts: T_OLD + (i as i64) * 1000,
                role: if i % 2 == 0 { "user" } else { "assistant" },
                preview: &text,
                emb: &emb,
            });
        }
        specs
    }

    fn seed_s7_and_l1_atoms(db: &Db) -> (Vec<SupersededSpec>, GraphAtoms) {
        let mut s7_specs = Vec::with_capacity(4);
        let sup_topics: [(&'static str, &'static str, &'static str); 4] = [
            (
                "SUP_TOPIC_1",
                "LEGACY_SUP_1 用户早期旧偏好：SUP_TOPIC_1 仅用单机脚本",
                "CURRENT_LIVE_1 用户现行规范：SUP_TOPIC_1 采用 Rust 状态机",
            ),
            (
                "SUP_TOPIC_2",
                "LEGACY_SUP_2 用户早期旧偏好：SUP_TOPIC_2 禁用连接池",
                "CURRENT_LIVE_2 用户现行规范：SUP_TOPIC_2 启用有界连接池",
            ),
            (
                "SUP_TOPIC_3",
                "LEGACY_SUP_3 用户早期旧偏好：SUP_TOPIC_3 关闭审计日志",
                "CURRENT_LIVE_3 用户现行规范：SUP_TOPIC_3 强制全量脱敏审计",
            ),
            (
                "SUP_TOPIC_4",
                "LEGACY_SUP_4 用户早期旧偏好：SUP_TOPIC_4 手动同步镜像",
                "CURRENT_LIVE_4 用户现行规范：SUP_TOPIC_4 自动对账修复",
            ),
        ];
        for (i, &(query, legacy_txt, live_txt)) in sup_topics.iter().enumerate() {
            let q_vec = synthetic_topic_vec(48 + i, 1300 + i);
            let old_id =
                insert_l1_atom(db, legacy_txt, T_OLD + i as i64, "high", None, Some(&q_vec));
            let live_vec = synthetic_topic_vec(48 + i, 1310 + i);
            let _live_id = insert_l1_atom(
                db,
                live_txt,
                T_NEW + i as i64,
                "medium",
                Some(old_id),
                Some(&live_vec),
            );
            s7_specs.push(SupersededSpec {
                query,
                live_marker: match i {
                    0 => "CURRENT_LIVE_1",
                    1 => "CURRENT_LIVE_2",
                    2 => "CURRENT_LIVE_3",
                    _ => "CURRENT_LIVE_4",
                },
                legacy_marker: match i {
                    0 => "LEGACY_SUP_1",
                    1 => "LEGACY_SUP_2",
                    2 => "LEGACY_SUP_3",
                    _ => "LEGACY_SUP_4",
                },
                query_vec: q_vec,
            });
        }

        // 4 additional non-S7 superseded pairs = 8 atoms (total superseded pair rows = 16).
        for i in 0..4usize {
            let v = synthetic_topic_vec(52 + i, 1400 + i);
            let old_id = insert_l1_atom(
                db,
                &format!("历史草稿策略第 {i} 号已过期条目"),
                T_OLD,
                "low",
                None,
                Some(&v),
            );
            insert_l1_atom(
                db,
                &format!("现行确认策略第 {i} 号有效条目"),
                T_MID,
                "high",
                Some(old_id),
                Some(&v),
            );
        }

        // 5 graph-linked live atoms + 19 standalone live atoms = 24 atoms (total L1 = 40).
        let atom_asuna = insert_l1_atom(
            db,
            "AsunaCore 是核心三层记忆架构中枢并支持渐进披露召回",
            T_NEW,
            "high",
            None,
            Some(&synthetic_topic_vec(56, 1501)),
        );
        let atom_sqlite = insert_l1_atom(
            db,
            "SqliteEngine 维护持久化会话与原子事实表并启用 WAL 模式",
            T_NEW,
            "high",
            None,
            Some(&synthetic_topic_vec(57, 1502)),
        );
        let atom_fts = insert_l1_atom(
            db,
            "Fts5Jieba 为中英文与连接符标识符提供统一全管道分词",
            T_NEW,
            "high",
            None,
            Some(&synthetic_topic_vec(58, 1503)),
        );
        let atom_hermes = insert_l1_atom(
            db,
            "HermesPlugin 通过同步网关接口完成召回、采集与会话结束钩子",
            T_NEW,
            "high",
            None,
            Some(&synthetic_topic_vec(59, 1504)),
        );
        let atom_int8 = insert_l1_atom(
            db,
            "Int8Quant 按每向量 f64 absmax 标度压缩 1024 维向量",
            T_NEW,
            "high",
            None,
            Some(&synthetic_topic_vec(60, 1505)),
        );
        for i in 0..19usize {
            let v = synthetic_topic_vec(52 + (i % 8), 1600 + i);
            insert_l1_atom(
                db,
                &format!("独立工程事实原子记录编号 {i:02}：严格保持数据向后兼容性"),
                T_MID + i as i64,
                "medium",
                None,
                Some(&v),
            );
        }

        (
            s7_specs,
            GraphAtoms {
                asuna: atom_asuna,
                sqlite: atom_sqlite,
                fts: atom_fts,
                hermes: atom_hermes,
                int8: atom_int8,
            },
        )
    }

    fn seed_l2_to_l5_and_graph(ctx: &SeedCtx<'_>, memory_dir: &Path, ga: &GraphAtoms) {
        let db = ctx.db;
        // Dedicated L0 anchor matching Stratum 8 query "AsunaCore 架构" cleanly across all 6 layers.
        let s8_vec = synthetic_topic_vec(61, 1700);
        ctx.insert_turn(&TurnSeed {
            session_id: "bench-s8-recall",
            seq: 1,
            ts: T_NEW + 500,
            role: "user",
            preview: "讨论 AsunaCore 架构的分层召回与存储设计",
            emb: &s8_vec,
        });

        // L2 scenarios (distinct memory_type='scenario', NOT 'atom').
        std::fs::create_dir_all(memory_dir.join("scenarios")).unwrap();
        std::fs::create_dir_all(memory_dir.join("mental_models")).unwrap();
        std::fs::create_dir_all(memory_dir.join("intent")).unwrap();
        for i in 0..2usize {
            db.conn()
                .execute(
                    "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
                     VALUES ('memory', ?1, ?2, ?2, 'high', 'scenario')",
                    rusqlite::params![format!("场景归档 {i}: 核心架构演化与兼容性守门"), T_NEW + i as i64],
                )
                .unwrap();
        }
        std::fs::write(
            memory_dir.join("scenarios").join("1775802600000_1.md"),
            "---\ntitle: \"核心架构演化\"\natom_ids: [1]\ncreated_at: 1775802600000\nupdated_at: 1775802600000\n---\n\n场景归档正文\n",
        )
        .unwrap();

        // L3 persona.md (enough tokens to exercise tight budget=220 truncation deterministically).
        std::fs::write(
            memory_dir.join("persona.md"),
            "---\ncreated_at: 1775802600000\nupdated_at: 1775802600000\n---\n\n# User Persona\n资深系统工程师，专注 Rust、SQLite 与检索系统架构升级，\
             长期关注渐进披露预算与数据零破坏兼容性验证流程。",
        )
        .unwrap();

        let now_utc = chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();
        for (rel, title, items) in [
            (
                "mental_models/workflow-patterns.md",
                "Workflow Patterns",
                &["先建回归 Oracle 再改核心代码", "每步改造执行全量门禁"][..],
            ),
            (
                "mental_models/decision-framework.md",
                "Decision Framework",
                &["疑难 API 先查官方文档", "严禁破坏历史客户数据"][..],
            ),
            (
                "mental_models/communication-style.md",
                "Communication Style",
                &["简洁结论配源码路径与实测输出"][..],
            ),
            (
                "intent/likely-next-topics.md",
                "Likely Next Topics",
                &["新旧二进制差分验证", "依赖受控升级"][..],
            ),
            (
                "intent/anticipated-needs.md",
                "Anticipated Needs",
                &["输出各分层召回量化指标报告"][..],
            ),
        ] {
            let mut body = format!("# {title}\n\nUpdated: {now_utc}\n\n");
            for item in items {
                body.push_str(&format!("- {item}\n"));
            }
            std::fs::write(memory_dir.join(rel), body).unwrap();
        }

        // Graph entities (>= 12) & relations (>= 12) with controlled multi-hop distances from AsunaCore.
        let triples = [
            ("AsunaCore", "depends_on", "SqliteEngine"),
            ("AsunaCore", "exposes", "RestGateway"),
            ("AsunaCore", "exposes", "McpStdio"),
            ("SqliteEngine", "uses", "Fts5Jieba"),
            ("SqliteEngine", "embeds", "SqliteVec"),
            ("RestGateway", "integrates", "HermesPlugin"),
            ("McpStdio", "speaks", "JsonRpc"),
            ("Fts5Jieba", "tokenizes", "ChineseCorpus"),
            ("SqliteVec", "stores", "Int8Quant"),
            ("RestGateway", "applies", "TokenBudget"),
            ("SqliteEngine", "fuses", "HybridRrf"),
            ("HermesPlugin", "bridges", "TokenBudget"),
        ];
        let inputs: Vec<TripleInput> = triples
            .iter()
            .map(|&(src, rel, dst)| TripleInput {
                src: src.to_string(),
                rel: rel.to_string(),
                dst: dst.to_string(),
                src_type: Some("component".to_string()),
                dst_type: Some("component".to_string()),
                confidence: Some(0.9),
                source_turn: None,
            })
            .collect();
        assert_triples(db, &inputs).unwrap();

        for (atom_id, content, entity) in [
            (ga.asuna, "AsunaCore atom", "AsunaCore"),
            (ga.sqlite, "SqliteEngine atom", "SqliteEngine"),
            (ga.fts, "Fts5Jieba atom", "Fts5Jieba"),
            (ga.hermes, "HermesPlugin atom", "HermesPlugin"),
            (ga.int8, "Int8Quant atom", "Int8Quant"),
        ] {
            integrate_atom_with_graph_on_conn(
                db.conn(),
                atom_id,
                content,
                None,
                None,
                &[entity.to_string()],
                &[],
            )
            .unwrap();
        }
    }

    fn seed_expanded_corpus(db: &Db, memory_dir: &Path) -> ExpandedCorpus {
        let ctx = SeedCtx::new(db);
        let s1to3 = seed_s1_s2_s3(&ctx);
        let s4_dist = seed_s4_distractors(&ctx);
        let s5 = seed_s5_semantic_and_hybrid(&ctx);
        let s6_filters = seed_s6_filters_and_fillers(&ctx);
        let (s7_superseded, graph_atoms) = seed_s7_and_l1_atoms(db);
        seed_l2_to_l5_and_graph(&ctx, memory_dir, &graph_atoms);

        ExpandedCorpus {
            s1_kw: s1to3.s1,
            s2_or: s1to3.s2,
            s3_mix: s1to3.s3,
            s4_dist,
            s5_paraphrase: s5.paraphrase,
            s5_hybrid_gain: s5.hybrid_gain,
            s6_filters,
            s7_superseded,
            graph_atoms,
        }
    }

    fn eval_keyword_batch(
        db: &Db,
        queries: &[(&'static str, i64)],
        latencies_ms: &mut Vec<f64>,
    ) -> (f64, f64) {
        let mut hits = 0usize;
        let mut rr_sum = 0.0f64;
        for &(query, expected_id) in queries {
            let params = SearchParams {
                query: query.to_string(),
                search_mode: SearchMode::Keyword,
                top_k: 5,
                after_ms: None,
                before_ms: None,
                role: None,
            };
            let t0 = Instant::now();
            let results = search_sessions_with_vec(db, None, &params).unwrap();
            latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            if let Some(pos) = results.iter().position(|r| r.turn_id == expected_id) {
                hits += 1;
                rr_sum += 1.0 / (pos as f64 + 1.0);
            }
        }
        (
            hits as f64 / queries.len() as f64,
            rr_sum / queries.len() as f64,
        )
    }

    struct S5Metrics {
        para_kw_ok: f64,
        para_sem_ok: f64,
        hyb_ok: f64,
        kw_mrr: f64,
        hyb_mrr: f64,
    }

    fn run_search_timed(
        db: &Db,
        query: &str,
        mode: SearchMode,
        q_vec: Option<Vec<f32>>,
        latencies_ms: &mut Vec<f64>,
    ) -> Vec<crate::fact::search::SearchResult> {
        let params = SearchParams {
            query: query.to_string(),
            search_mode: mode,
            top_k: 5,
            after_ms: None,
            before_ms: None,
            role: None,
        };
        let t0 = Instant::now();
        let res = search_sessions_with_vec(db, q_vec, &params).unwrap();
        latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        res
    }

    fn eval_s5_stratum(db: &Db, corpus: &ExpandedCorpus, latencies_ms: &mut Vec<f64>) -> S5Metrics {
        let mut para_kw_hits = 0usize;
        let mut para_sem_hits = 0usize;
        let mut s5_hyb_hits = 0usize;
        let mut kw_rr_sum = 0.0f64;
        let mut hyb_rr_sum = 0.0f64;

        for (query, target_id, q_vec) in &corpus.s5_paraphrase {
            if run_search_timed(db, query, SearchMode::Keyword, None, latencies_ms)
                .iter()
                .any(|r| r.turn_id == *target_id)
            {
                para_kw_hits += 1;
            }
            if run_search_timed(
                db,
                query,
                SearchMode::Semantic,
                Some(q_vec.clone()),
                latencies_ms,
            )
            .iter()
            .any(|r| r.turn_id == *target_id)
            {
                para_sem_hits += 1;
            }
            if let Some(pos) = run_search_timed(
                db,
                query,
                SearchMode::Hybrid,
                Some(q_vec.clone()),
                latencies_ms,
            )
            .iter()
            .position(|r| r.turn_id == *target_id)
            {
                s5_hyb_hits += 1;
                hyb_rr_sum += 1.0 / (pos as f64 + 1.0);
            }
        }

        for (query, target_id, q_vec) in &corpus.s5_hybrid_gain {
            if let Some(pos) = run_search_timed(db, query, SearchMode::Keyword, None, latencies_ms)
                .iter()
                .position(|r| r.turn_id == *target_id)
            {
                kw_rr_sum += 1.0 / (pos as f64 + 1.0);
            }
            if let Some(pos) = run_search_timed(
                db,
                query,
                SearchMode::Hybrid,
                Some(q_vec.clone()),
                latencies_ms,
            )
            .iter()
            .position(|r| r.turn_id == *target_id)
            {
                s5_hyb_hits += 1;
                hyb_rr_sum += 1.0 / (pos as f64 + 1.0);
            }
        }

        S5Metrics {
            para_kw_ok: para_kw_hits as f64 / corpus.s5_paraphrase.len() as f64,
            para_sem_ok: para_sem_hits as f64 / corpus.s5_paraphrase.len() as f64,
            hyb_ok: s5_hyb_hits as f64 / 8.0,
            kw_mrr: kw_rr_sum / 8.0,
            hyb_mrr: hyb_rr_sum / 8.0,
        }
    }

    fn eval_s6_filters(
        db: &Db,
        specs: &[TimeRoleSpec],
        latencies_ms: &mut Vec<f64>,
    ) -> (f64, f64, usize) {
        let mut hits = 0usize;
        let mut rr_sum = 0.0f64;
        let mut leaks = 0usize;
        for spec in specs {
            let params = SearchParams {
                query: spec.query.to_string(),
                search_mode: spec.mode.clone(),
                top_k: 5,
                after_ms: spec.after_ms,
                before_ms: spec.before_ms,
                role: spec.role.map(str::to_string),
            };
            let t0 = Instant::now();
            let res = search_sessions_with_vec(db, spec.query_vec.clone(), &params).unwrap();
            latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            for r in &res {
                let time_bad = spec.after_ms.is_some_and(|a| r.timestamp_ms < a)
                    || spec.before_ms.is_some_and(|b| r.timestamp_ms > b);
                let role_bad = spec.role.is_some_and(|expected| r.role != expected);
                if time_bad || role_bad || spec.forbidden_ids.contains(&r.turn_id) {
                    leaks += 1;
                }
            }
            if let Some(pos) = res.iter().position(|r| r.turn_id == spec.target_id) {
                hits += 1;
                rr_sum += 1.0 / (pos as f64 + 1.0);
            }
        }
        (
            hits as f64 / specs.len() as f64,
            rr_sum / specs.len() as f64,
            leaks,
        )
    }

    fn eval_s7_superseded(
        db: &Db,
        memory_dir: &Path,
        specs: &[SupersededSpec],
        latencies_ms: &mut Vec<f64>,
    ) -> (f64, f64, f64) {
        let engine = RetrievalEngine::new(db, memory_dir, 4000);
        let mut hits = 0usize;
        let mut rr_sum = 0.0f64;
        let mut leaked_queries = 0usize;
        for spec in specs {
            let t0 = Instant::now();
            let outcome = engine
                .recall_with_vec(spec.query, Some(&spec.query_vec), 5, None, None, Some(4000))
                .unwrap();
            latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

            let l1_items: Vec<&serde_json::Value> = outcome
                .memories
                .iter()
                .filter(|m| m["layer"] == "L1")
                .collect();
            let has_legacy = outcome.context.contains("LEGACY_SUP_")
                || l1_items.iter().any(|m| {
                    m["content"]
                        .as_str()
                        .is_some_and(|c| c.contains(spec.legacy_marker))
                });
            if has_legacy {
                leaked_queries += 1;
            }
            if let Some(pos) = l1_items.iter().position(|m| {
                m["content"]
                    .as_str()
                    .is_some_and(|c| c.contains(spec.live_marker))
            }) {
                hits += 1;
                rr_sum += 1.0 / (pos as f64 + 1.0);
            }
        }
        (
            hits as f64 / specs.len() as f64,
            rr_sum / specs.len() as f64,
            leaked_queries as f64 / specs.len() as f64,
        )
    }

    fn layer_rank(layer: &str) -> u8 {
        match layer {
            "L3" => 0,
            "L4" => 1,
            "L5" => 2,
            "L2" => 3,
            "L1" => 4,
            "L0" => 5,
            _ => 9,
        }
    }

    fn eval_s8_recall_and_graph(
        db: &Db,
        memory_dir: &Path,
        ga: &GraphAtoms,
        latencies_ms: &mut Vec<f64>,
    ) {
        let engine = RetrievalEngine::new(db, memory_dir, 4000);

        // Q1: Generous budget retains all 6 layers in strict L3->L4->L5->L2->L1->L0 order.
        let t0 = Instant::now();
        let full = engine
            .recall_with_vec("AsunaCore 架构", None, 5, None, None, Some(4000))
            .unwrap();
        latencies_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert!(!full.truncated, "generous budget 4000 must not truncate");
        assert!(full.context.starts_with(RECALL_BANNER));
        let layers: Vec<&str> = full
            .memories
            .iter()
            .map(|m| m["layer"].as_str().unwrap())
            .collect();
        for req in ["L3", "L4", "L5", "L2", "L1", "L0"] {
            assert!(layers.contains(&req), "missing layer {req} in {layers:?}");
        }
        assert!(layers
            .windows(2)
            .all(|w| layer_rank(w[0]) <= layer_rank(w[1])));

        // Q2: Tight budget truncates cleanly while preserving L3 persona + prefix order.
        let t1 = Instant::now();
        let tight = engine
            .recall_with_vec("AsunaCore 架构", None, 5, None, None, Some(220))
            .unwrap();
        latencies_ms.push(t1.elapsed().as_secs_f64() * 1000.0);
        assert!(
            tight.truncated,
            "tight budget 220 must truncate: {:?}",
            tight.memories
        );
        assert!(tight.context.starts_with(RECALL_BANNER));
        let tight_layers: Vec<&str> = tight
            .memories
            .iter()
            .map(|m| m["layer"].as_str().unwrap())
            .collect();
        assert_eq!(tight_layers.first().copied(), Some("L3"));
        assert!(tight_layers
            .windows(2)
            .all(|w| layer_rank(w[0]) <= layer_rank(w[1])));

        // Q3: Graph 1-hop returns ONLY atom_asuna.
        let t2 = Instant::now();
        let (hop1, tr1) = multi_hop_query_with_truncation(db, "AsunaCore", 1, None).unwrap();
        latencies_ms.push(t2.elapsed().as_secs_f64() * 1000.0);
        assert!(!tr1);
        assert_eq!(hop1, vec![ga.asuna], "1-hop must contain only atom_asuna");

        // Q4: Graph 3-hop discovers downstream atoms while excluding 4-hop atom_int8.
        let t3 = Instant::now();
        let (hop3, tr3) = multi_hop_query_with_truncation(db, "AsunaCore", 3, None).unwrap();
        latencies_ms.push(t3.elapsed().as_secs_f64() * 1000.0);
        assert!(!tr3);
        for expected in [ga.asuna, ga.sqlite, ga.fts, ga.hermes] {
            assert!(
                hop3.contains(&expected),
                "3-hop missing {expected}: {hop3:?}"
            );
        }
        assert!(
            !hop3.contains(&ga.int8),
            "4-hop atom_int8 leaked into 3-hop"
        );

        // Q5: Relation filter narrows traversal strictly to mentions edges.
        let t4 = Instant::now();
        let (rel_only, tr_rel) =
            multi_hop_query_with_truncation(db, "AsunaCore", 3, Some("mentions")).unwrap();
        latencies_ms.push(t4.elapsed().as_secs_f64() * 1000.0);
        assert!(!tr_rel);
        assert_eq!(rel_only, vec![ga.asuna]);
    }

    fn verify_corpus_counts(db: &Db) -> HashMap<&'static str, i64> {
        let mut counts = HashMap::new();
        for (k, sql) in [
            ("turns", "SELECT COUNT(*) FROM turns"),
            ("vec_turns", "SELECT COUNT(*) FROM vec_turns_rowids"),
            (
                "l1_atoms",
                "SELECT COUNT(*) FROM bounded_memory WHERE target='memory' AND memory_type='atom'",
            ),
            ("entities", "SELECT COUNT(*) FROM entities"),
            ("relations", "SELECT COUNT(*) FROM relations"),
        ] {
            let n: i64 = db.conn().query_row(sql, [], |r| r.get(0)).unwrap();
            counts.insert(k, n);
        }
        assert_eq!(counts["turns"], 240, "expected 240 L0 turns");
        assert_eq!(counts["vec_turns"], 240, "expected 240 L0 turn vectors");
        assert_eq!(counts["l1_atoms"], 40, "expected 40 L1 atoms");
        assert!(counts["entities"] >= 12, "entities={}", counts["entities"]);
        assert!(
            counts["relations"] >= 12,
            "relations={}",
            counts["relations"]
        );
        counts
    }

    pub(super) fn run_expanded_benchmark() {
        let tmp = TempDir::new().unwrap();
        let memory_dir = tmp.path().join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        let corpus = seed_expanded_corpus(&db, &memory_dir);
        let counts = verify_corpus_counts(&db);

        let mut latencies_ms = Vec::new();
        let (s1_ok, s1_mrr) = eval_keyword_batch(&db, &corpus.s1_kw, &mut latencies_ms);
        let (s2_ok, s2_mrr) = eval_keyword_batch(&db, &corpus.s2_or, &mut latencies_ms);
        let (s3_ok, s3_mrr) = eval_keyword_batch(&db, &corpus.s3_mix, &mut latencies_ms);
        let (s4_ok, s4_mrr) = eval_keyword_batch(&db, &corpus.s4_dist, &mut latencies_ms);
        let s5 = eval_s5_stratum(&db, &corpus, &mut latencies_ms);
        let (s6_ok, s6_mrr, s6_leaks) = eval_s6_filters(&db, &corpus.s6_filters, &mut latencies_ms);
        let (s7_ok, s7_mrr, sup_leak_rate) =
            eval_s7_superseded(&db, &memory_dir, &corpus.s7_superseded, &mut latencies_ms);
        eval_s8_recall_and_graph(&db, &memory_dir, &corpus.graph_atoms, &mut latencies_ms);

        let total_queries = corpus.s1_kw.len()
            + corpus.s2_or.len()
            + corpus.s3_mix.len()
            + corpus.s4_dist.len()
            + corpus.s5_paraphrase.len()
            + corpus.s5_hybrid_gain.len()
            + corpus.s6_filters.len()
            + corpus.s7_superseded.len()
            + 5;
        assert_eq!(total_queries, 54);

        let overall_success = (s1_ok * 8.0
            + s2_ok * 8.0
            + s3_ok * 8.0
            + s4_ok * 8.0
            + s5.hyb_ok * 8.0
            + s6_ok * 5.0
            + s7_ok * 4.0
            + 5.0)
            / 54.0;
        let overall_mrr = (s1_mrr * 8.0
            + s2_mrr * 8.0
            + s3_mrr * 8.0
            + s4_mrr * 8.0
            + s5.hyb_mrr * 8.0
            + s6_mrr * 5.0
            + s7_mrr * 4.0
            + 5.0)
            / 54.0;

        latencies_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = percentile(&latencies_ms, 0.50);
        let p95 = percentile(&latencies_ms, 0.95);

        println!("═══ AMS expanded retrieval benchmark (54 queries / 8 strata) ═══");
        println!(
            "corpus: {} turns, {} L1 atoms, {} entities, {} relations",
            counts["turns"], counts["l1_atoms"], counts["entities"], counts["relations"]
        );
        println!("S1 compound-zh      (8q): Success@5={s1_ok:.3}  MRR={s1_mrr:.3}");
        println!("S2 non-adj OR       (8q): Success@5={s2_ok:.3}  MRR={s2_mrr:.3}");
        println!("S3 mixed . _ - id   (8q): Success@5={s3_ok:.3}  MRR={s3_mrr:.3}");
        println!("S4 distractor       (8q): Success@5={s4_ok:.3}  MRR={s4_mrr:.3}");
        println!(
            "S5 semantic+hybrid  (8q): ParaKW={:.3} Sem={:.3} Hyb={:.3} MRR(kw={:.3} -> hyb={:.3})",
            s5.para_kw_ok, s5.para_sem_ok, s5.hyb_ok, s5.kw_mrr, s5.hyb_mrr
        );
        println!("S6 time+role filter (5q): Success@5={s6_ok:.3}  MRR={s6_mrr:.3}  FilterLeaks={s6_leaks}");
        println!(
            "S7 superseded gate  (4q): Success@5={s7_ok:.3}  MRR={s7_mrr:.3}  SupersededLeakRate={sup_leak_rate:.3}"
        );
        println!(
            "S8 recall+multihop  (5q): Passed=5/5 (L3->L4->L5->L2->L1->L0 + budget + 1/3/rel hops)"
        );
        println!(
            "Overall (54q): Success@5={overall_success:.3}  MRR={overall_mrr:.3}  latency: p50={p50:.2}ms  p95={p95:.2}ms"
        );

        for (label, ok, mrr, floor) in [
            ("S1", s1_ok, s1_mrr, 0.75),
            ("S2", s2_ok, s2_mrr, 0.75),
            ("S3", s3_ok, s3_mrr, 0.75),
            ("S4", s4_ok, s4_mrr, 0.80),
        ] {
            assert!((ok - 1.0).abs() < 1e-9, "{label} Success@5={ok:.3}");
            assert!(mrr >= floor, "{label} MRR={mrr:.3} < {floor:.2}");
        }
        assert!(
            s5.para_kw_ok.abs() < 1e-9,
            "S5 paraphrase Keyword={:.3}",
            s5.para_kw_ok
        );
        assert!(
            (s5.para_sem_ok - 1.0).abs() < 1e-9,
            "S5 Semantic={:.3}",
            s5.para_sem_ok
        );
        assert!((s5.hyb_ok - 1.0).abs() < 1e-9, "S5 Hybrid={:.3}", s5.hyb_ok);
        assert!(
            s5.hyb_mrr > s5.kw_mrr,
            "S5 Hybrid MRR ({:.3}) <= Keyword ({:.3})",
            s5.hyb_mrr,
            s5.kw_mrr
        );
        assert!((s6_ok - 1.0).abs() < 1e-9 && s6_leaks == 0);
        assert!((s7_ok - 1.0).abs() < 1e-9);
        assert!(
            sup_leak_rate.abs() < 1e-12,
            "SupersededLeakRate={sup_leak_rate:.3}"
        );
    }
}
