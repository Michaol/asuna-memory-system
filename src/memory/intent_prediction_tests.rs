use super::*;
use tempfile::TempDir;

#[test]
fn test_likely_topics_serialization() {
    let topics = LikelyNextTopics {
        topics: vec!["Topic 1".to_string(), "Topic 2".to_string()],
        updated_at: 1234567890,
    };

    let json = serde_json::to_string(&topics).unwrap();
    let deserialized: LikelyNextTopics = serde_json::from_str(&json).unwrap();

    assert_eq!(deserialized.topics.len(), 2);
    assert_eq!(deserialized.topics[0], "Topic 1");
}

#[test]
fn test_save_and_load_likely_topics() {
    let temp_dir = TempDir::new().unwrap();
    let llm = std::sync::Arc::new(LlmClient::new("test", "test", "test"));
    let predictor = IntentPredictor::new(llm, temp_dir.path().to_path_buf());

    let topics = LikelyNextTopics {
        topics: vec!["Topic X".to_string(), "Topic Y".to_string()],
        updated_at: 1_700_000_000,
    };

    predictor.save_likely_topics(&topics).unwrap();

    let loaded = predictor.load_likely_topics().unwrap().unwrap();
    assert_eq!(loaded.topics.len(), 2);
    assert_eq!(loaded.topics[0], "Topic X");
    assert_eq!(loaded.topics[1], "Topic Y");
    // J9: was hardcoded 0; must now come back from the "Updated:" line.
    assert_eq!(loaded.updated_at, 1_700_000_000);
}

#[test]
fn test_save_and_load_anticipated_needs_parses_updated_at() {
    let temp_dir = TempDir::new().unwrap();
    let llm = std::sync::Arc::new(LlmClient::new("test", "test", "test"));
    let predictor = IntentPredictor::new(llm, temp_dir.path().to_path_buf());

    let needs = AnticipatedNeeds {
        needs: vec!["Need 1".to_string(), "Need 2".to_string()],
        updated_at: 1_600_000_000,
    };

    predictor.save_anticipated_needs(&needs).unwrap();

    let loaded = predictor.load_anticipated_needs().unwrap().unwrap();
    assert_eq!(loaded.needs, needs.needs);
    assert_eq!(loaded.updated_at, 1_600_000_000);
}

/// S14c(7d): the pure-file `load_*_from` free loaders (the /recall L5
/// read path) and the predictor-bound instance loaders must return
/// exactly the same documents, present or absent.
#[test]
fn free_loaders_match_instance_loaders() {
    let temp_dir = TempDir::new().unwrap();
    let llm = std::sync::Arc::new(LlmClient::new("test", "test", "test"));
    let predictor = IntentPredictor::new(llm, temp_dir.path().to_path_buf());
    let dir = temp_dir.path();

    predictor
        .save_likely_topics(&LikelyNextTopics {
            topics: vec!["T1".to_string(), "T2".to_string()],
            updated_at: 1_700_000_000,
        })
        .unwrap();
    // anticipated-needs.md deliberately absent → both must agree on None.
    assert_eq!(
        load_likely_topics_from(dir).unwrap(),
        predictor.load_likely_topics().unwrap()
    );
    assert_eq!(
        load_anticipated_needs_from(dir).unwrap(),
        predictor.load_anticipated_needs().unwrap()
    );
    assert_eq!(load_anticipated_needs_from(dir).unwrap(), None);
}

#[test]
fn test_predict_all_single_call_and_parse_fallback() {
    let tmp = TempDir::new().unwrap();
    let scenarios = vec![Scenario {
        title: "t".into(),
        atom_ids: vec![],
        summary: "s".into(),
        created_at: 0,
        updated_at: 0,
    }];

    let (url1, calls1) = crate::memory::llm::spawn_test_llm_stub(vec![
        r#"{"likely_topics":["t1"],"anticipated_needs":["n1"]}"#,
    ]);
    let llm1 = std::sync::Arc::new(LlmClient::new(&url1, "k", "m"));
    let mm1 = MentalModelGenerator::new(llm1.clone(), tmp.path().to_path_buf());
    let pred1 = IntentPredictor::new(llm1, tmp.path().to_path_buf());
    let all = pred1.predict_all(&scenarios, &mm1).unwrap();
    assert_eq!(calls1.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(all.likely_topics.unwrap().topics, vec!["t1"]);
    assert_eq!(all.anticipated_needs.unwrap().needs, vec!["n1"]);

    let (url2, calls2) =
        crate::memory::llm::spawn_test_llm_stub(vec!["bad", r#"["t_fb"]"#, r#"["n_fb"]"#]);
    let llm2 = std::sync::Arc::new(LlmClient::new(&url2, "k", "m"));
    let mm2 = MentalModelGenerator::new(llm2.clone(), tmp.path().to_path_buf());
    let pred2 = IntentPredictor::new(llm2, tmp.path().to_path_buf());
    let fb = pred2.predict_all(&scenarios, &mm2).unwrap();
    assert_eq!(calls2.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(fb.likely_topics.unwrap().topics, vec!["t_fb"]);
    assert_eq!(fb.anticipated_needs.unwrap().needs, vec!["n_fb"]);
}

/// 折叠回归锚：两档 prompt 必须与折叠前逐字一致（维度表/模板改错即失败）。
#[test]
fn dimension_prompts_match_pre_fold_literals() {
    let ctx = "<user_data>CTX</user_data>";
    let guard = crate::memory::UNTRUSTED_DATA_GUARD;

    let (system, user) = dimension_prompt(ctx, &TOPICS_DIM);
    assert_eq!(
        system,
        format!("You are a topic predictor. Based on the user's recent scenarios and mental models inside <user_data>, predict what topics they are likely to ask about next. {guard}")
    );
    assert_eq!(
        user,
        format!("{ctx}\n\nPredict 3-5 likely next topics. Return ONLY a JSON array of strings, no markdown.")
    );

    let (system, user) = dimension_prompt(ctx, &NEEDS_DIM);
    assert_eq!(
        system,
        format!("You are a needs analyst. Based on the user's recent scenarios and mental models inside <user_data>, predict what needs they might have in future sessions. {guard}")
    );
    assert_eq!(
        user,
        format!("{ctx}\n\nPredict 3-5 anticipated needs. Return ONLY a JSON array of strings, no markdown.")
    );
}
