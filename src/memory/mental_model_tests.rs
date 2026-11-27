use super::*;
use tempfile::TempDir;

#[test]
fn test_workflow_patterns_serialization() {
    let workflow = WorkflowPatterns {
        patterns: vec!["Pattern 1".to_string(), "Pattern 2".to_string()],
        updated_at: 1234567890,
    };

    let json = serde_json::to_string(&workflow).unwrap();
    let deserialized: WorkflowPatterns = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.patterns.len(), 2);
    assert_eq!(deserialized.patterns[0], "Pattern 1");
}

#[test]
fn test_save_and_load_workflow_patterns() {
    let temp_dir = TempDir::new().unwrap();
    let llm = Arc::new(LlmClient::new("test", "test", "test"));
    let generator = MentalModelGenerator::new(llm, temp_dir.path().to_path_buf());

    let workflow = WorkflowPatterns {
        patterns: vec!["Pattern A".to_string(), "Pattern B".to_string()],
        updated_at: chrono::Utc::now().timestamp(),
    };

    generator.save_workflow_patterns(&workflow).unwrap();

    let loaded = generator.load_workflow_patterns().unwrap().unwrap();
    assert_eq!(loaded.patterns.len(), 2);
    assert_eq!(loaded.patterns[0], "Pattern A");
    assert_eq!(loaded.patterns[1], "Pattern B");
}

/// J10: `updated_at` survives the render/parse round trip via the
/// `Updated: …` line (fixed second-granularity value, so a mismatch is
/// unambiguous — the mtime fallback would drift).
#[test]
fn test_list_md_roundtrips_updated_at() {
    let temp_dir = TempDir::new().unwrap();
    let llm = Arc::new(LlmClient::new("test", "test", "test"));
    let generator = MentalModelGenerator::new(llm, temp_dir.path().to_path_buf());

    let workflow = WorkflowPatterns {
        patterns: vec!["P1".to_string()],
        updated_at: 1_700_000_000,
    };
    generator.save_workflow_patterns(&workflow).unwrap();
    let loaded = generator.load_workflow_patterns().unwrap().unwrap();
    assert_eq!(loaded.updated_at, 1_700_000_000);

    let framework = DecisionFramework {
        criteria: vec!["C1".to_string()],
        updated_at: 1_600_000_000,
    };
    generator.save_decision_framework(&framework).unwrap();
    let loaded = generator.load_decision_framework().unwrap().unwrap();
    assert_eq!(loaded.updated_at, 1_600_000_000);

    let style = CommunicationStyle {
        preferences: vec!["S1".to_string()],
        updated_at: 1_500_000_000,
    };
    generator.save_communication_style(&style).unwrap();
    let loaded = generator.load_communication_style().unwrap().unwrap();
    assert_eq!(loaded.updated_at, 1_500_000_000);
}

/// S14c(7d): the pure-file `load_*_from` free loaders (the /recall L4
/// read path) and the generator-bound instance loaders — which the
/// generator's own pipeline context uses — must return exactly the same
/// documents, present or absent.
#[test]
fn free_loaders_match_instance_loaders() {
    let temp_dir = TempDir::new().unwrap();
    let llm = Arc::new(LlmClient::new("test", "test", "test"));
    let generator = MentalModelGenerator::new(llm, temp_dir.path().to_path_buf());
    let dir = temp_dir.path();

    generator
        .save_workflow_patterns(&WorkflowPatterns {
            patterns: vec!["P1".to_string(), "P2".to_string()],
            updated_at: 1_700_000_000,
        })
        .unwrap();
    generator
        .save_decision_framework(&DecisionFramework {
            criteria: vec!["C1".to_string()],
            updated_at: 1_600_000_000,
        })
        .unwrap();
    // communication-style.md deliberately absent → both must agree on None.
    assert_eq!(
        load_workflow_patterns_from(dir).unwrap(),
        generator.load_workflow_patterns().unwrap()
    );
    assert_eq!(
        load_decision_framework_from(dir).unwrap(),
        generator.load_decision_framework().unwrap()
    );
    assert_eq!(
        load_communication_style_from(dir).unwrap(),
        generator.load_communication_style().unwrap()
    );
    assert_eq!(
        load_communication_style_from(dir).unwrap(),
        None,
        "absent doc loads as None"
    );
    assert_eq!(
        load_workflow_patterns_from(dir).unwrap().unwrap().patterns,
        vec!["P1".to_string(), "P2".to_string()]
    );
}

#[test]
fn save_list_md_collapses_embedded_newlines_in_items() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("list.md");
    save_list_md(
        &path,
        "Test List",
        1_700_000_000,
        &[
            "First line\n- Forged second bullet\r\n# Forged header".to_string(),
            "   \n  ".to_string(),
            "Clean second item".to_string(),
        ],
    )
    .unwrap();
    let (items, _) = load_list_md(&path).unwrap().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0],
        "First line - Forged second bullet  # Forged header"
    );
    assert_eq!(items[1], "Clean second item");
}

#[test]
fn test_generate_all_single_call_and_parse_fallback() {
    let tmp = TempDir::new().unwrap();
    let scenarios = vec![Scenario {
        title: "t".into(),
        atom_ids: vec![],
        summary: "s".into(),
        created_at: 0,
        updated_at: 0,
    }];

    let (url1, calls1) = crate::memory::llm::spawn_test_llm_stub(vec![
        r#"{"workflow_patterns":["wf1"],"decision_framework":["df1"],"communication_style":["cs1"]}"#,
    ]);
    let gen1 = MentalModelGenerator::new(
        Arc::new(LlmClient::new(&url1, "k", "m")),
        tmp.path().to_path_buf(),
    );
    let all = gen1.generate_all(&scenarios, None).unwrap();
    assert_eq!(calls1.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(all.workflow.unwrap().patterns, vec!["wf1"]);
    assert_eq!(all.decision.unwrap().criteria, vec!["df1"]);
    assert_eq!(all.communication.unwrap().preferences, vec!["cs1"]);

    let (url2, calls2) = crate::memory::llm::spawn_test_llm_stub(vec![
        "invalid json",
        r#"["wf_fb"]"#,
        "bad",
        r#"["cs_fb"]"#,
    ]);
    let gen2 = MentalModelGenerator::new(
        Arc::new(LlmClient::new(&url2, "k", "m")),
        tmp.path().to_path_buf(),
    );
    let fb = gen2.generate_all(&scenarios, None).unwrap();
    assert_eq!(calls2.load(std::sync::atomic::Ordering::SeqCst), 4);
    assert_eq!(fb.workflow.unwrap().patterns, vec!["wf_fb"]);
    assert!(fb.decision.is_err());
    assert_eq!(fb.communication.unwrap().preferences, vec!["cs_fb"]);
}

/// 折叠回归锚：三档 prompt 必须与折叠前逐字一致（维度表/模板改错即失败）。
#[test]
fn dimension_prompts_match_pre_fold_literals() {
    let ctx = "<user_data>CTX</user_data>";
    let guard = crate::memory::UNTRUSTED_DATA_GUARD;

    let (system, user) = dimension_prompt(ctx, &WORKFLOW_DIM);
    assert_eq!(
        system,
        format!("You are a workflow analyst. Extract the user's typical work patterns from their scenarios and persona inside <user_data>. {guard}")
    );
    assert_eq!(
        user,
        format!("{ctx}\n\nExtract 3-5 key work patterns. Return ONLY a JSON array of strings, no markdown.")
    );

    let (system, user) = dimension_prompt(ctx, &DECISION_DIM);
    assert_eq!(
        system,
        format!("You are a decision analyst. Extract how the user makes decisions from their scenarios and persona inside <user_data>. {guard}")
    );
    assert_eq!(
        user,
        format!("{ctx}\n\nExtract 3-5 key decision criteria. Return ONLY a JSON array of strings, no markdown.")
    );

    let (system, user) = dimension_prompt(ctx, &COMMUNICATION_DIM);
    assert_eq!(
        system,
        format!("You are a communication analyst. Extract the user's communication preferences from their scenarios and persona inside <user_data>. {guard}")
    );
    assert_eq!(
        user,
        format!("{ctx}\n\nExtract 3-5 key communication preferences. Return ONLY a JSON array of strings, no markdown.")
    );
}
