use super::*;

/// F1: a padded config model must not carry whitespace into the request
/// body (same yardstick as base_url/api_key). Non-empty after trim, so
/// resolve_env never consults the env for it — no env mutation needed.
#[test]
fn test_llm_model_config_value_trimmed() {
    let mut llm = LlmConfig {
        model: "  cfg-model  ".into(),
        ..LlmConfig::default()
    };
    llm.resolve_env();
    assert_eq!(llm.model, "cfg-model");
}

/// F2: a whitespace-only api_format counts as unset and triggers
/// auto-detection on a native DashScope URL; on compatible-mode it stays
/// empty (OpenAI). api_key is filled so resolve_env reads no env var.
#[test]
fn test_embedding_whitespace_api_format_auto_detects() {
    let mut native = EmbeddingConfig {
        api_url: "https://dashscope.aliyuncs.com/api/v1".into(),
        api_key: "sk-test".into(),
        api_format: "   ".into(),
        ..EmbeddingConfig::default()
    };
    native.resolve_env();
    assert_eq!(native.api_format, "dashscope");

    let mut compat = EmbeddingConfig {
        api_url: "https://dashscope.aliyuncs.com/compatible-mode/v1".into(),
        api_key: "sk-test".into(),
        api_format: "  ".into(),
        ..EmbeddingConfig::default()
    };
    compat.resolve_env();
    assert_eq!(compat.api_format, "");
}
