use super::*;
use crate::util::{dirs_home, recover_poison};
use std::path::{Path, PathBuf};

/// W3 回归：空/纯空白 data_dir 必须被拒绝——否则 expand_tilde 原样返回 ""，
/// profile_dir 退化为相对路径 "profiles/default"，create_dir_all("") 又返回 Ok，
/// 整个数据树静默落到进程 CWD，换个 cwd 启动就"记忆全丢"。
#[test]
fn test_w3_empty_data_dir_rejected() {
    for bad in ["", "   ", "\t"] {
        let cfg = Config {
            data_dir: PathBuf::from(bad),
            ..Config::default()
        };
        let err = cfg.validate().expect_err("空 data_dir 必须被拒绝");
        assert!(err.to_string().contains("data_dir"), "{err}");
    }
}

#[test]
fn test_graph_missing_detected() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 1024, "batch_size": 32}}"#,
    ).unwrap();
    let config = Config::load(&p).unwrap();
    assert!(
        config.graph_using_defaults,
        "should detect missing graph section"
    );
}

#[test]
fn test_graph_present_not_flagged() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"data_dir": ".", "profile_id": "default", "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200}, "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true}, "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true}, "embedding": {"model_name": "test", "dimensions": 1024, "batch_size": 32}, "graph": {"enabled": false, "remind_on_save": false}}"#,
    ).unwrap();
    let config = Config::load(&p).unwrap();
    assert!(
        !config.graph_using_defaults,
        "should not flag when graph section present"
    );
    assert!(!config.graph.enabled);
}

#[test]
fn test_load_nonexistent_uses_defaults() {
    let config = Config::load(Path::new("/nonexistent/path/config.json")).unwrap();
    assert!(
        config.graph_using_defaults,
        "nonexistent config should flag defaults"
    );
}

#[test]
fn test_embedding_api_config_compatible_mode_stays_openai() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"data_dir": ".", "profile_id": "default", "embedding": {"dimensions": 1024, "batch_size": 10, "api_url": "https://dashscope.aliyuncs.com/compatible-mode/v1", "api_key": "sk-test", "api_model": "text-embedding-v4"}}"#,
    ).unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(
        config.embedding.api_url,
        "https://dashscope.aliyuncs.com/compatible-mode/v1"
    );
    assert_eq!(config.embedding.api_key, "sk-test");
    assert_eq!(config.embedding.api_model, "text-embedding-v4");
    // compatible-mode speaks OpenAI /embeddings, NOT DashScope native format
    assert_eq!(config.embedding.api_format, "");
    assert_eq!(config.embedding.batch_size, 10);
}

#[test]
fn test_dashscope_native_url_auto_detects_format_and_clamps_batch_size() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"embedding": {"batch_size": 32, "api_url": "https://dashscope.aliyuncs.com/api/v1", "api_key": "sk-test", "api_model": "text-embedding-v4"}}"#,
    )
    .unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(config.embedding.api_format, "dashscope");
    assert_eq!(config.embedding.batch_size, 10);
}

/// v2.6.1/v3.0 regression: DashScope rejects >10 inputs per embedding request
/// (HTTP 400). Both compatible-mode and native DashScope URLs must clamp batch_size <= 10.
#[test]
fn test_dashscope_batch_size_clamped_to_provider_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"data_dir": ".", "profile_id": "default", "embedding": {"dimensions": 1024, "batch_size": 32, "api_url": "https://dashscope.aliyuncs.com/compatible-mode/v1", "api_key": "sk-test", "api_model": "text-embedding-v4"}}"#,
    ).unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(
        config.embedding.api_format, "",
        "compatible-mode URL must keep OpenAI format"
    );
    assert_eq!(
        config.embedding.batch_size, 10,
        "batch_size must clamp to DashScope's 10-input cap even on compatible-mode"
    );
}

/// W1: the clamp must use the same case/whitespace yardstick as
/// ApiFormat::from_str — a mixed-case or padded api_format on a non-DashScope
/// proxy URL used to escape the 10-input cap.
#[test]
fn test_dashscope_format_clamp_is_case_and_whitespace_insensitive() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    for format in ["DashScope", "DASHSCOPE", " dashscope "] {
        let root = serde_json::json!({
            "embedding": {
                "dimensions": 1024,
                "batch_size": 32,
                "api_url": "https://proxy.example/v1",
                "api_key": "sk-test",
                "api_model": "text-embedding-v4",
                "api_format": format,
            }
        });
        std::fs::write(&p, root.to_string()).unwrap();
        let config = Config::load(&p).unwrap();
        assert_eq!(
            config.embedding.batch_size, 10,
            "api_format={format:?} must clamp to DashScope's 10-input cap"
        );
        assert_eq!(
            crate::embedder::api::ApiFormat::from_str(&config.embedding.api_format),
            crate::embedder::api::ApiFormat::DashScope,
            "the clamp must agree with the embedder's own parse"
        );
    }
}

/// qwen3.7-text-embedding / -flash accept 20 inputs per native DashScope request
/// (official docs), vs 10 for text-embedding-v3/v4. The clamp must be model-aware
/// on both the legacy shared domain and the workspace domain, and must never
/// raise a batch_size that is already below the cap.
#[test]
fn test_qwen37_native_format_clamps_to_20() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    for url in [
        "https://dashscope.aliyuncs.com/api/v1",
        "https://ws-abc123.cn-beijing.maas.aliyuncs.com/api/v1",
    ] {
        for model in [
            "qwen3.7-text-embedding-flash",
            "qwen3.7-text-embedding",
            "Qwen3.7-Text-Embedding-Flash",
        ] {
            let root = serde_json::json!({
                "embedding": {
                    "dimensions": 1024,
                    "batch_size": 32,
                    "api_url": url,
                    "api_key": "sk-test",
                    "api_model": model,
                }
            });
            std::fs::write(&p, root.to_string()).unwrap();
            let config = Config::load(&p).unwrap();
            assert_eq!(config.embedding.api_format, "dashscope", "url={url}");
            assert_eq!(config.embedding.batch_size, 20, "url={url} model={model}");
        }
    }
    // Below the cap: the clamp shrinks only, never raises.
    let root = serde_json::json!({
        "embedding": {
            "dimensions": 1024,
            "batch_size": 16,
            "api_url": "https://dashscope.aliyuncs.com/api/v1",
            "api_key": "sk-test",
            "api_model": "qwen3.7-text-embedding-flash",
        }
    });
    std::fs::write(&p, root.to_string()).unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(config.embedding.batch_size, 16);
}

/// Compatible-mode keeps the conservative 10 cap even for flash: its
/// compatible-mode batch limit is not documented, and compatible-mode URLs
/// must stay OpenAI format on both domain families.
#[test]
fn test_qwen37_compatible_mode_keeps_10_cap() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    for url in [
        "https://dashscope.aliyuncs.com/compatible-mode/v1",
        "https://ws-abc123.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
    ] {
        let root = serde_json::json!({
            "embedding": {
                "dimensions": 1024,
                "batch_size": 32,
                "api_url": url,
                "api_key": "sk-test",
                "api_model": "qwen3.7-text-embedding-flash",
            }
        });
        std::fs::write(&p, root.to_string()).unwrap();
        let config = Config::load(&p).unwrap();
        assert_eq!(config.embedding.api_format, "", "url={url}");
        assert_eq!(config.embedding.batch_size, 10, "url={url}");
    }
}

/// Regression guard: the 20 cap is qwen3.7-only. text-embedding-v4 on a
/// workspace URL must still clamp to 10 (v3/v4 hard cap).
#[test]
fn test_workspace_domain_v4_still_clamps_to_10() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"embedding": {"dimensions": 1024, "batch_size": 32, "api_url": "https://ws-abc123.cn-beijing.maas.aliyuncs.com/api/v1", "api_key": "sk-test", "api_model": "text-embedding-v4"}}"#,
    )
    .unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(config.embedding.api_format, "dashscope");
    assert_eq!(config.embedding.batch_size, 10);
}

#[test]
fn test_embedding_explicit_format() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"data_dir": ".", "profile_id": "default", "embedding": {"dimensions": 1024, "batch_size": 32, "api_url": "https://dashscope.aliyuncs.com/api/v1", "api_key": "sk-test", "api_model": "text-embedding-v4", "api_format": "openai"}}"#,
    ).unwrap();
    let config = Config::load(&p).unwrap();
    // Explicit "openai" overrides auto-detection
    assert_eq!(config.embedding.api_format, "openai");
    assert_eq!(config.embedding.batch_size, 10);
}

#[test]
fn test_embedding_defaults_no_api() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"data_dir": ".", "profile_id": "default", "embedding": {"model_name": "test", "dimensions": 1024, "batch_size": 32}}"#,
    ).unwrap();
    let config = Config::load(&p).unwrap();
    assert!(config.embedding.api_url.is_empty());
    assert!(config.embedding.api_model.is_empty());
}

#[test]
fn test_removed_embedding_keys_and_model_path_ignored() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"model_path": "/legacy/models/dir", "embedding": {"model_name": "custom-model", "dimensions": 1024}}"#,
    )
    .unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(config.embedding.dimensions, 1024);
    assert_eq!(config.embedding.batch_size, 32);
}

#[test]
fn test_config_rejects_non_1024_dimensions() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    for bad_dim in [0usize, 384, 512, 768, 1536] {
        std::fs::write(
            &p,
            format!(r#"{{"embedding": {{"dimensions": {bad_dim}}}}}"#),
        )
        .unwrap();
        let err = Config::load(&p).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("1024"), "dim={bad_dim} error: {msg}");
        assert!(msg.contains("rebuild --full"), "dim={bad_dim} error: {msg}");
    }
}

#[test]
fn test_config_validate_rejects_invalid_numeric_fields() {
    let mut c = Config::default();
    assert!(c.validate().is_ok());

    let mut bad = c.clone();
    bad.embedding.batch_size = 0;
    assert!(bad.validate().is_err());

    let mut bad = c.clone();
    bad.conversation.preview_length = 0;
    assert!(bad.validate().is_err());

    for bad_cluster in [0, 1] {
        let mut bad = c.clone();
        bad.scenarios.min_cluster_size = bad_cluster;
        assert!(bad.validate().is_err(), "min_cluster_size={bad_cluster}");
    }

    // v4-M: 零值尺标补齐（top_k=0 恒空、char_limit=0 渲染空 .md）
    let mut bad = c.clone();
    bad.search.default_top_k = 0;
    assert!(bad.validate().is_err());

    let mut bad = c.clone();
    bad.memory.memory_char_limit = 0;
    assert!(bad.validate().is_err());

    let mut bad = c.clone();
    bad.memory.user_char_limit = 0;
    assert!(bad.validate().is_err());

    for bad_ratio in [-0.1, 1.1, f64::NAN, f64::INFINITY] {
        let mut bad = c.clone();
        bad.memory.atom_capacity_ratio = bad_ratio;
        assert!(bad.validate().is_err(), "ratio={bad_ratio:?}");
    }

    for bad_thresh in [-0.01, 1.01, f64::NAN] {
        let mut bad = c.clone();
        bad.admission.threshold = bad_thresh;
        assert!(bad.validate().is_err(), "threshold={bad_thresh:?}");
    }

    let mut bad = c.clone();
    bad.admission.weights = [0.0, 0.0, 0.0, 0.0, 0.0];
    assert!(bad.validate().is_err());

    let mut bad = c.clone();
    bad.admission.weights[0] = -0.1;
    assert!(bad.validate().is_err());

    let mut bad = c.clone();
    bad.admission.weights[0] = f64::NAN;
    assert!(bad.validate().is_err());

    for bad_sim in [-1.5, 1.5, f32::NAN] {
        c.scenarios.similarity_threshold = bad_sim;
        assert!(c.validate().is_err(), "similarity_threshold={bad_sim:?}");
    }
}

/// M6: every weight can be finite while the sum overflows to +inf, and
/// search_mode must be refused at startup (its only consumer parses strictly).
#[test]
fn test_config_validate_rejects_infinite_weight_sum_and_invalid_search_mode() {
    let c = Config::default();

    let mut bad = c.clone();
    bad.admission.weights = [1e308; 5];
    assert!(bad.validate().is_err(), "weight sum overflowed to +inf");

    for mode in ["", "vector", "fts", "Hybrid", "unknown"] {
        let mut bad = c.clone();
        bad.search.search_mode = mode.to_string();
        assert!(bad.validate().is_err(), "search_mode={mode:?}");
    }
    for mode in ["hybrid", "semantic", "keyword"] {
        let mut ok = c.clone();
        ok.search.search_mode = mode.to_string();
        assert!(ok.validate().is_ok(), "search_mode={mode:?}");
    }
}

#[test]
fn test_expand_tilde_handles_separators_and_named_user() {
    let home = dirs_home();
    assert_eq!(expand_tilde(Path::new("~")), home);
    assert_eq!(expand_tilde(Path::new("~/.asuna")), home.join(".asuna"));
    assert_eq!(expand_tilde(Path::new("~\\.asuna")), home.join(".asuna"));
    assert_eq!(expand_tilde(Path::new("~alice")), PathBuf::from("~alice"));
    assert_eq!(
        expand_tilde(Path::new("/abs/path")),
        PathBuf::from("/abs/path")
    );
}

/// J2/S14d: a minimal config.json — every section and key is optional,
/// container-level `#[serde(default)]` fills documented defaults.
#[test]
fn test_empty_config_loads_with_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(&p, "{}").unwrap();
    let config = Config::load(&p).unwrap();
    assert!(config.data_dir.ends_with(".asuna"));
    assert_eq!(config.profile_id, "default");
    assert_eq!(config.conversation.preview_length, 200);
    assert_eq!(config.memory.memory_char_limit, 2200);
    assert_eq!(config.memory.user_char_limit, 1375);
    assert!(config.memory.security_scan);
    assert!((config.memory.atom_capacity_ratio - 0.3).abs() < f64::EPSILON);
    assert_eq!(config.search.default_top_k, 5);
    assert_eq!(config.search.search_mode, "hybrid");
    assert_eq!(config.embedding.dimensions, 1024);
    assert_eq!(config.embedding.batch_size, 32);
    assert!(config.graph.enabled);
    assert!(config.pipeline.enable_extraction);
    assert_eq!(config.pipeline.every_n_turns, 5);
    assert!(config.admission.enabled);
    assert_eq!(config.recall.token_budget, 2000);
    assert!(!config.scenarios.enabled);
    assert_eq!(config.persona.trigger_every_n, 10);
    // { } has no graph key → the "graph section missing" runtime flag
    // still fires (doctor hint relies on it).
    assert!(config.graph_using_defaults);
}

#[test]
fn test_single_section_subset_loads() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(&p, r#"{"embedding": {"batch_size": 16}}"#).unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(config.embedding.dimensions, 1024);
    assert_eq!(config.embedding.batch_size, 16);
    assert!(config.embedding.api_url.is_empty());
    assert_eq!(config.profile_id, "default");
    assert_eq!(config.recall.token_budget, 2000);

    // 空对象应回落全部默认值
    let empty_p = tmp.path().join("empty.json");
    std::fs::write(&empty_p, "{}").unwrap();
    let default_cfg = Config::load(&empty_p).unwrap();
    assert_eq!(
        default_cfg.embedding.batch_size,
        Config::default().embedding.batch_size
    );

    // 类型错误必须被拒绝而非静默回落默认
    let bad_p = tmp.path().join("bad.json");
    std::fs::write(&bad_p, r#"{"embedding": {"batch_size": "sixteen"}}"#).unwrap();
    assert!(
        Config::load(&bad_p).is_err(),
        "wrong field type in config must fail to load"
    );
}

/// Field-level defaults inside a PRESENT section: keys not mentioned
/// must come from the section's `Default`, not serde's type defaults
/// (e.g. `enable_extraction` must stay TRUE, not false).
#[test]
fn test_partial_section_keeps_struct_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{"pipeline": {"every_n_turns": 3}, "graph": {"enabled": false}, "memory": {"security_scan": false}}"#,
    )
    .unwrap();
    let config = Config::load(&p).unwrap();
    assert_eq!(config.pipeline.every_n_turns, 3);
    assert!(
        config.pipeline.enable_extraction,
        "bool default must be true, not serde's false"
    );
    assert!(!config.graph.enabled);
    assert!(
        config.graph.remind_on_save,
        "bool default must be true, not serde's false"
    );
    assert!(!config.memory.security_scan);
    assert_eq!(config.memory.memory_char_limit, 2200);
}

/// J35/S14d full removal-back-compat anchor: every deleted key
/// (recall.strategy/max_results/timeout_ms, pipeline.idle_timeout_seconds/
/// l2_min_interval_seconds/enable_warmup, the whole privacy section,
/// search.fts_enabled, conversation.enabled/auto_embed,
/// memory.memory_enabled/user_profile_enabled, embedding.model_name)
/// must be ignored by an otherwise-valid config.
#[test]
fn test_all_removed_keys_still_load() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(
        &p,
        r#"{
            "data_dir": ".",
            "privacy": {"l0_retention_days": 90, "l1_retention_days": 0, "auto_cleanup": true},
            "recall": {"strategy": "vector", "max_results": 7, "timeout_ms": 111, "token_budget": 1500},
            "pipeline": {"idle_timeout_seconds": 600, "l2_min_interval_seconds": 3600, "enable_warmup": false},
            "search": {"default_top_k": 9, "fts_enabled": false},
            "conversation": {"enabled": false, "auto_embed": false, "preview_length": 50},
            "memory": {"memory_enabled": false, "user_profile_enabled": false},
            "embedding": {"model_name": "old-model", "dimensions": 1024}
        }"#,
    )
    .unwrap();
    let config = Config::load(&p).unwrap();
    // surviving keys in the same sections are honored
    assert_eq!(config.recall.token_budget, 1500);
    assert_eq!(config.search.default_top_k, 9);
    assert_eq!(config.conversation.preview_length, 50);
    assert_eq!(config.embedding.dimensions, 1024);
    // and the deleted ones left no trace: their old defaults did NOT
    // override the section defaults (warmup false must not disable anything)
    assert!(config.pipeline.enable_extraction);
    assert_eq!(config.pipeline.every_n_turns, 5);
}

/// J2: serde-defaulting must not break the documented precedence
/// config.json explicit > env > built-in default for the env-backed
/// fields (llm.*; gateway/embedding precedence is covered by the
/// existing bind-host / api tests below).
#[test]
fn test_llm_precedence_config_wins_over_env_wins_over_default() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    let llm_envs: &[(&str, Option<&str>)] = &[
        ("AMS_LLM_BASE_URL", Some("http://from-env")),
        ("AMS_LLM_API_KEY", Some("env-key")),
        ("AMS_LLM_MODEL", Some("env-model")),
        ("OPENAI_MODEL", None),
    ];
    with_gateway_envs(llm_envs, || {
        // explicit config.json values win over env
        std::fs::write(
            &p,
            r#"{"llm": {"base_url": "http://from-config", "api_key": "cfg-key", "model": "cfg-model"}}"#,
        )
        .unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.llm.base_url, "http://from-config");
        assert_eq!(c.llm.api_key, "cfg-key");
        assert_eq!(c.llm.model, "cfg-model");
        // section present but fields omitted → serde default ("") lets
        // env fill them
        std::fs::write(&p, r#"{"llm": {}}"#).unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.llm.base_url, "http://from-env");
        assert_eq!(c.llm.model, "env-model");
    });
    // no config values, no env → resolve_env's built-in model fallback
    let no_envs: &[(&str, Option<&str>)] = &[
        ("AMS_LLM_BASE_URL", None),
        ("AMS_LLM_API_KEY", None),
        ("AMS_LLM_MODEL", None),
        ("OPENAI_BASE_URL", None),
        ("OPENAI_API_KEY", None),
        ("OPENAI_MODEL", None),
    ];
    with_gateway_envs(no_envs, || {
        std::fs::write(&p, "{}").unwrap();
        let c = Config::load(&p).unwrap();
        assert_eq!(c.llm.model, "deepseek-v3");
        assert!(c.llm.base_url.is_empty());
    });
    // M2: AMS_LLM_BASE_URL must NEVER silently pair with OPENAI_API_KEY
    with_gateway_envs(
        &[
            ("AMS_LLM_BASE_URL", Some("https://custom.llm.example/v1")),
            ("AMS_LLM_API_KEY", None),
            ("OPENAI_BASE_URL", None),
            ("OPENAI_API_KEY", Some("sk-openai-leak-guard")),
        ],
        || {
            let mut llm = LlmConfig::default();
            llm.resolve_env();
            assert_eq!(llm.base_url, "https://custom.llm.example/v1");
            assert!(
                llm.api_key.is_empty(),
                "OPENAI_API_KEY must not pair with AMS_LLM_BASE_URL"
            );
        },
    );
    // OPENAI_BASE_URL + OPENAI_API_KEY pairing continues to work
    with_gateway_envs(
        &[
            ("AMS_LLM_BASE_URL", None),
            ("AMS_LLM_API_KEY", None),
            ("OPENAI_BASE_URL", Some("https://api.openai.com/v1")),
            ("OPENAI_API_KEY", Some("sk-openai-valid")),
        ],
        || {
            let mut llm = LlmConfig::default();
            llm.resolve_env();
            assert_eq!(llm.base_url, "https://api.openai.com/v1");
            assert_eq!(llm.api_key, "sk-openai-valid");
        },
    );
}

#[test]
fn test_config_rejects_invalid_profile_id() {
    let mut c = Config {
        profile_id: "../escape".into(),
        ..Config::default()
    };
    assert!(c.validate().is_err());

    c.profile_id = "CON".into();
    assert!(c.validate().is_err());
}

// ── U11/U12: gateway env resolution + bind validation ──

/// resolve_env reads process-wide env vars, so these tests serialize on a
/// mutex and remove every var they touched afterwards (the unsafe blocks
/// mirror embedder/mod.rs's cross-version set_var style). Other tests in
/// this file only assert on fields resolve_env never mutates, so a
/// concurrent Config::load observing a set var cannot change their results.
static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvRestore {
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvRestore {
    fn apply(vars: &[(&'static str, Option<&str>)]) -> Self {
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            match v {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        Self { saved }
    }
}

impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (k, prev) in &self.saved {
            match prev {
                Some(val) => unsafe { std::env::set_var(k, val) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}

fn with_gateway_envs<T>(vars: &[(&'static str, Option<&str>)], f: impl FnOnce() -> T) -> T {
    let _g = recover_poison(&ENV_GUARD);
    let _restore = EnvRestore::apply(vars);
    f()
}

/// F1-a: 纯空白 config model 视同未设置，env _unset 时落内置默认（注释声明的回归）。
#[test]
fn test_llm_model_whitespace_only_falls_back() {
    with_gateway_envs(&[("AMS_LLM_MODEL", None), ("OPENAI_MODEL", None)], || {
        let mut llm = LlmConfig {
            model: "   ".into(),
            ..LlmConfig::default()
        };
        llm.resolve_env();
        assert_eq!(llm.model, "deepseek-v3");
    });
}

const GW_ENVS: &[(&str, Option<&str>)] = &[
    ("AMS_GATEWAY_API_KEY", None),
    ("AMS_GATEWAY_AUTH_ENABLED", None),
    ("AMS_GATEWAY_BIND_HOST", None),
];

#[test]
fn test_gateway_resolve_env_no_envs() {
    with_gateway_envs(GW_ENVS, || {
        let mut gw = GatewayConfig::default();
        gw.resolve_env();
        assert!(!gw.auth_enabled);
        assert!(gw.api_key.is_empty());
        assert_eq!(gw.bind_host, "127.0.0.1");
    });
}

#[test]
fn test_gateway_resolve_env_key_implies_auth() {
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", Some("s3cret")),
            ("AMS_GATEWAY_AUTH_ENABLED", None),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert_eq!(gw.api_key, "s3cret");
            assert!(gw.auth_enabled, "env key must imply auth (U11)");
        },
    );
}

#[test]
fn test_gateway_resolve_env_key_overrides_explicit_false() {
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", Some("s3cret")),
            ("AMS_GATEWAY_AUTH_ENABLED", None),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig {
                auth_enabled: false,
                api_key: "from-config".into(),
                cors_origins: vec![],
                bind_host: "127.0.0.1".into(),
            };
            gw.resolve_env();
            assert!(
                gw.auth_enabled,
                "env key wins over false (secure direction)"
            );
            // config.json key still takes precedence over the env value
            assert_eq!(gw.api_key, "from-config");
        },
    );
}

#[test]
fn test_gateway_resolve_env_auth_enabled_switch() {
    // explicit false keeps auth off even with an env key present
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", Some("s3cret")),
            ("AMS_GATEWAY_AUTH_ENABLED", Some("false")),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert!(!gw.auth_enabled, "explicit switch outranks the implication");
        },
    );
    // explicit true with no key: auth on (run_gateway then refuses to
    // start without a key — pre-existing fail-closed behavior)
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", None),
            ("AMS_GATEWAY_AUTH_ENABLED", Some("1")),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert!(gw.auth_enabled);
            assert!(gw.api_key.is_empty());
        },
    );
    // unparseable value: warn + ignore, config state untouched
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", None),
            ("AMS_GATEWAY_AUTH_ENABLED", Some("maybe")),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert!(!gw.auth_enabled);
        },
    );
    // NB8 matrix: explicit true outranks everything, config value kept off
    // with no key stays off (log order no longer changes these finals)
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", Some("s3cret")),
            ("AMS_GATEWAY_AUTH_ENABLED", Some("true")),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert!(gw.auth_enabled);
        },
    );
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", None),
            ("AMS_GATEWAY_AUTH_ENABLED", Some("0")),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig {
                auth_enabled: true,
                api_key: "from-config".into(),
                cors_origins: vec![],
                bind_host: String::new(),
            };
            gw.resolve_env();
            assert!(!gw.auth_enabled, "explicit env switch outranks config.json");
        },
    );
    // NB4: an all-whitespace env key is "unset" (auth must NOT engage —
    // the startup guard / middleware see the same empty key).
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", Some("   ")),
            ("AMS_GATEWAY_AUTH_ENABLED", None),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert!(gw.api_key.is_empty(), "whitespace key trims to unset");
            assert!(!gw.auth_enabled, "whitespace-only key must not enable auth");
        },
    );
    // NB4: a padded real key resolves to the trimmed secret (same
    // yardstick as key_from_env) and still implies auth.
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", Some(" s3cret ")),
            ("AMS_GATEWAY_AUTH_ENABLED", None),
            ("AMS_GATEWAY_BIND_HOST", None),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert_eq!(gw.api_key, "s3cret");
            assert!(gw.auth_enabled);
        },
    );
}

#[test]
fn test_gateway_resolve_env_bind_host() {
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", None),
            ("AMS_GATEWAY_AUTH_ENABLED", None),
            ("AMS_GATEWAY_BIND_HOST", Some("0.0.0.0")),
        ],
        || {
            // the "" default (= what Config::load sees when the key or
            // whole section is absent) means "unset", so env fills it…
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert_eq!(gw.bind_host, "0.0.0.0");
            // …but an explicit config.json value wins
            let mut gw = GatewayConfig {
                bind_host: "127.0.0.5".into(),
                ..GatewayConfig::default()
            };
            gw.resolve_env();
            assert_eq!(gw.bind_host, "127.0.0.5");
        },
    );
}

/// v4-M: bind_host 与 api_key/base_url 同尺标——采纳的 env 值与 config 值都 trim。
#[test]
fn test_gateway_bind_host_trimmed() {
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", None),
            ("AMS_GATEWAY_AUTH_ENABLED", None),
            ("AMS_GATEWAY_BIND_HOST", Some("  0.0.0.0  ")),
        ],
        || {
            let mut gw = GatewayConfig::default();
            gw.resolve_env();
            assert_eq!(gw.bind_host, "0.0.0.0", "padded env value must be trimmed");
        },
    );
    with_gateway_envs(GW_ENVS, || {
        let mut gw = GatewayConfig {
            bind_host: "  127.0.0.5  ".into(),
            ..GatewayConfig::default()
        };
        gw.resolve_env();
        assert_eq!(
            gw.bind_host, "127.0.0.5",
            "padded config value must be trimmed"
        );
    });
}

/// v4-M: Config::load 的读/解析错误必须带上文件路径上下文。
#[test]
fn test_load_errors_include_file_path() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("broken.json");
    std::fs::write(&p, "{ not json").unwrap();
    let err = Config::load(&p).unwrap_err();
    assert!(err.to_string().contains("broken.json"), "got: {err}");
    // 合法 JSON 但字段类型错误（from_value 路径）同样要带路径
    std::fs::write(&p, r#"{"profile_id": 42}"#).unwrap();
    let err = Config::load(&p).unwrap_err();
    assert!(err.to_string().contains("broken.json"), "got: {err}");
}

/// U12's real scenario, end to end through Config::load: the Docker path
/// ships no config.json (or one without gateway.bind_host), so
/// AMS_GATEWAY_BIND_HOST must survive to the effective value run_gateway
/// binds — a hand-built struct state is not enough (the env branch used to
/// be dead code because every load path pre-filled loopback).
#[test]
fn test_gateway_config_load_bind_host_env() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    let load_with_gateway = |gateway_json: Option<&str>| -> Config {
        let mut root = serde_json::json!({
            "data_dir": ".",
            "profile_id": "default",
            "conversation": {"enabled": true, "auto_embed": true, "preview_length": 200},
            "memory": {"memory_enabled": true, "user_profile_enabled": true, "memory_char_limit": 2200, "user_char_limit": 1375, "security_scan": true},
            "search": {"default_top_k": 5, "search_mode": "hybrid", "fts_enabled": true},
            "embedding": {"model_name": "test", "dimensions": 1024, "batch_size": 32, "api_url": "https://example.invalid/v1", "api_key": "sk-test", "api_format": "openai"},
        });
        if let Some(g) = gateway_json {
            root["gateway"] = serde_json::from_str(g).unwrap();
        }
        std::fs::write(&p, root.to_string()).unwrap();
        Config::load(&p).unwrap()
    };
    with_gateway_envs(
        &[
            ("AMS_GATEWAY_API_KEY", None),
            ("AMS_GATEWAY_AUTH_ENABLED", None),
            ("AMS_GATEWAY_BIND_HOST", Some("0.0.0.0")),
        ],
        || {
            // gateway section present but bind_host key absent → env wins
            let c = load_with_gateway(Some(
                r#"{"auth_enabled": true, "api_key": "k", "cors_origins": []}"#,
            ));
            assert_eq!(c.gateway.bind_host, "0.0.0.0");
            // no gateway section at all → env wins
            let c = load_with_gateway(None);
            assert_eq!(c.gateway.bind_host, "0.0.0.0");
            // explicit bind_host in config.json outranks env
            let c = load_with_gateway(Some(
                r#"{"auth_enabled": true, "api_key": "k", "cors_origins": [], "bind_host": "127.0.0.5"}"#,
            ));
            assert_eq!(c.gateway.bind_host, "127.0.0.5");
        },
    );
    // env unset → loopback fallback on every path
    with_gateway_envs(GW_ENVS, || {
        let c = load_with_gateway(None);
        assert_eq!(c.gateway.bind_host, "127.0.0.1");
        let c = load_with_gateway(Some(
            r#"{"auth_enabled": false, "api_key": "", "cors_origins": []}"#,
        ));
        assert_eq!(c.gateway.bind_host, "127.0.0.1");
    });
}

#[test]
fn test_validate_gateway_bind_loopback_needs_no_auth() {
    for host in [
        "127.0.0.1",
        "127.9.9.9",
        "::1",
        "[::1]",
        "localhost",
        "LOCALhost",
        " 127.0.0.1 ",
    ] {
        validate_gateway_bind(host, false, "")
            .unwrap_or_else(|e| panic!("loopback {host:?} must bind without auth: {e}"));
    }
}

#[test]
fn test_validate_gateway_bind_non_loopback_requires_auth() {
    for host in [
        "0.0.0.0",
        "::",
        "[::]",
        "10.0.0.5",
        "memory.internal.example",
    ] {
        let err = validate_gateway_bind(host, false, "k")
            .err()
            .unwrap_or_else(|| panic!("non-loopback {host:?} without auth must be refused"));
        assert!(
            err.to_string().contains("requires authentication"),
            "host {host:?} → {err}"
        );
        // with auth + key it passes
        validate_gateway_bind(host, true, "k")
            .unwrap_or_else(|e| panic!("non-loopback {host:?} with auth must pass: {e}"));
    }
    // auth "enabled" with an empty key is not authentication
    assert!(validate_gateway_bind("0.0.0.0", true, "").is_err());
    assert!(validate_gateway_bind("0.0.0.0", true, "   ").is_err());
}

#[test]
fn test_validate_gateway_bind_invalid_addresses() {
    assert!(validate_gateway_bind("", false, "").is_err());
    assert!(validate_gateway_bind("   ", false, "").is_err());
    let err = validate_gateway_bind("127.0.0.1:8080", true, "k")
        .expect_err("host:port inside bind_host must be rejected");
    assert!(err.to_string().contains("not a valid address"), "{err}");
    let err = validate_gateway_bind("0.0.0.0:8765", false, "").expect_err("same");
    assert!(err.to_string().contains("not a valid address"), "{err}");
    // NB5: IPv6 brackets must be PAIRED — "[::1" (loopback once stripped)
    // used to slip through and answer the loopback question for a typo.
    let err =
        validate_gateway_bind("[::1", false, "").expect_err("unbalanced '[' must be rejected");
    assert!(err.to_string().contains("invalid bind_host"), "{err}");
    let err =
        validate_gateway_bind("::1]", false, "").expect_err("unbalanced ']' must be rejected");
    assert!(err.to_string().contains("invalid bind_host"), "{err}");
    // paired form stays valid (loopback, no auth needed)
    validate_gateway_bind("[::1]", false, "")
        .unwrap_or_else(|e| panic!("paired IPv6 literal must pass: {e}"));
}

/// W7: brackets are IPv6-literal syntax only — "[localhost]" / "[127.0.0.1]"
/// are not addresses and must be refused instead of answering "loopback".
#[test]
fn test_validate_gateway_bind_rejects_non_ipv6_brackets() {
    for host in ["[localhost]", "[127.0.0.1]", "[::1]:8765", "[]"] {
        let err = validate_gateway_bind(host, true, "k")
            .err()
            .unwrap_or_else(|| panic!("bracketed non-IPv6 {host:?} must be refused"));
        assert!(
            err.to_string().contains("invalid bind_host"),
            "host {host:?} → {err}"
        );
    }
    // genuine IPv6 literals keep their existing behavior
    validate_gateway_bind("[::1]", false, "")
        .unwrap_or_else(|e| panic!("bracketed IPv6 loopback must pass: {e}"));
    validate_gateway_bind("[2001:db8::1]", true, "k")
        .unwrap_or_else(|e| panic!("bracketed IPv6 with auth must pass: {e}"));
}

#[test]
fn test_gateway_bind_addr_formatting() {
    assert_eq!(gateway_bind_addr("127.0.0.1", 8765), "127.0.0.1:8765");
    assert_eq!(gateway_bind_addr("localhost", 80), "localhost:80");
    // bare IPv6 must be bracketed, bracketed forms kept verbatim
    assert_eq!(gateway_bind_addr("::1", 8765), "[::1]:8765");
    assert_eq!(gateway_bind_addr("::", 8765), "[::]:8765");
    assert_eq!(gateway_bind_addr("[::1]", 8765), "[::1]:8765");
    assert_eq!(gateway_bind_addr(" 0.0.0.0 ", 8765), "0.0.0.0:8765");
}

#[test]
fn test_llm_resolve_env_treats_empty_string_as_unset() {
    with_gateway_envs(
        &[("AMS_LLM_MODEL", Some("")), ("OPENAI_MODEL", None)],
        || {
            let mut cfg = LlmConfig::default();
            cfg.resolve_env();
            assert_eq!(cfg.model, "deepseek-v3");
        },
    );
}

/// W1: padded config.json values must not carry whitespace into the endpoint
/// URL or the Authorization header.
#[test]
fn test_llm_and_embedding_api_fields_trimmed() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    let root = serde_json::json!({
        "embedding": {
            "dimensions": 1024,
            "api_url": " https://proxy.example/v1/ ",
            "api_key": " sk-emb ",
            "api_model": "text-embedding-v4",
        },
        "llm": {
            "base_url": " https://llm.example/v1 ",
            "api_key": " sk-llm ",
            "model": "cfg-model",
        },
    });
    std::fs::write(&p, root.to_string()).unwrap();
    let c = Config::load(&p).unwrap();
    assert_eq!(c.embedding.api_url, "https://proxy.example/v1/");
    assert_eq!(c.embedding.api_key, "sk-emb");
    assert_eq!(c.llm.base_url, "https://llm.example/v1");
    assert_eq!(c.llm.api_key, "sk-llm");
}

/// W1: env-provided values get the same trim, and a whitespace-only config
/// value counts as unset (same semantics as gateway.api_key).
#[test]
fn test_llm_and_embedding_env_values_trimmed() {
    with_gateway_envs(
        &[
            ("AMS_EMBEDDING_API_KEY", Some(" sk-env ")),
            ("AMS_LLM_BASE_URL", Some(" https://env.example/v1 ")),
            ("AMS_LLM_API_KEY", Some(" sk-llm-env ")),
            ("OPENAI_BASE_URL", None),
            ("OPENAI_API_KEY", None),
        ],
        || {
            let mut emb = EmbeddingConfig {
                api_url: "   ".into(),
                api_key: "  ".into(),
                ..EmbeddingConfig::default()
            };
            emb.resolve_env();
            assert!(emb.api_url.is_empty(), "whitespace-only URL trims to unset");
            assert_eq!(emb.api_key, "sk-env");

            let mut llm = LlmConfig::default();
            llm.resolve_env();
            assert_eq!(llm.base_url, "https://env.example/v1");
            assert_eq!(llm.api_key, "sk-llm-env");
        },
    );
}

/// W23: Debug output must never carry plaintext API keys or URL userinfo —
/// llm/embedding/gateway keys render [REDACTED], base URLs go through
/// sanitize_url_for_log.
#[test]
fn test_config_debug_redacts_keys_and_url_userinfo() {
    let mut c = Config::default();
    c.llm.base_url = "https://user:llm-pass@llm.example/v1".into();
    c.llm.api_key = "sk-llm-plaintext-secret".into();
    c.embedding.api_url = "https://user:emb-pass@emb.example/v1".into();
    c.embedding.api_key = "sk-emb-plaintext-secret".into();
    c.gateway.api_key = "gw-plaintext-secret".into();
    let dbg = format!("{c:?}");
    for secret in [
        "sk-llm-plaintext-secret",
        "sk-emb-plaintext-secret",
        "gw-plaintext-secret",
        "llm-pass",
        "emb-pass",
        "user:",
    ] {
        assert!(!dbg.contains(secret), "Debug leaked {secret:?}: {dbg}");
    }
    assert!(
        dbg.contains("[REDACTED]"),
        "keys must show [REDACTED]: {dbg}"
    );
    assert!(dbg.contains("https://llm.example/v1"), "{dbg}");
    assert!(dbg.contains("https://emb.example/v1"), "{dbg}");
}

/// W9a: the batch_size upper bound is inclusive — 1024 passes, 1025 fails.
#[test]
fn test_embedding_batch_size_upper_bound() {
    let mut c = Config::default();
    c.embedding.batch_size = 1025;
    let err = c.validate().expect_err("batch_size=1025 must be rejected");
    assert!(err.to_string().contains("batch_size"), "{err}");
    c.embedding.batch_size = 1024;
    assert!(c.validate().is_ok(), "batch_size=1024 must pass");
}

/// M5: a padded data_dir trims cleanly during Config::load resolution so the
/// profile tree does not drift into a whitespace-padded directory name.
#[test]
fn test_padded_data_dir_trimmed_on_load() {
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().join("config.json");
    std::fs::write(&p, r#"{"data_dir": "  /tmp/ams  "}"#).unwrap();
    let c = Config::load(&p).unwrap();
    assert_eq!(c.data_dir, PathBuf::from("/tmp/ams"));
    assert!(c.profile_dir().starts_with(Path::new("/tmp/ams")));
}
