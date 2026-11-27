use super::*;

/// U1: the mirror's frontmatter must be valid YAML, `supersedes_id:
/// None` must serialize as a real YAML null (the old `{:?}` emitted the
/// string "None"), and the body sections must stay human-readable.
#[test]
fn test_save_persona_writes_valid_yaml_frontmatter() {
    let tmp = tempfile::TempDir::new().unwrap();
    let llm = LlmClient::new("test", "test", "test");
    let generator = PersonaGenerator::new(&llm, tmp.path());

    let persona = Persona {
        preferences: "喜欢 Rust: 所有权系统".to_string(),
        identity: "开发者".to_string(),
        workflow: "白天编码".to_string(),
        tech_stack: "Rust, SQLite".to_string(),
        communication_style: "简洁".to_string(),
        created_at: 1000,
        updated_at: 2000,
        supersedes_id: None,
    };
    let path = generator.save_persona(&persona).unwrap();

    let content = std::fs::read_to_string(&path).unwrap();
    // 闭合分隔符必须独占一行（防 trim_end 粘连回归，同 scenario.rs）
    assert!(content.starts_with("---\n"));
    assert!(
        content.get(4..).is_some_and(|s| s.contains("\n---\n")),
        "closing --- must start its own line: {:?}",
        content
    );
    let fm = crate::util::frontmatter::extract_block(&content)
        .expect("frontmatter must have balanced fences");
    assert_eq!(
        crate::util::frontmatter::parse_i64(fm, "created_at"),
        Some(1000)
    );
    assert_eq!(
        crate::util::frontmatter::parse_i64(fm, "updated_at"),
        Some(2000)
    );
    assert_eq!(
        crate::util::frontmatter::parse_optional_i64(fm, "supersedes_id"),
        Some(None),
        "None must be YAML null"
    );
    assert!(fm.contains("supersedes_id: null"));
    // Some(id) round-trips as a number, not a Debug string.
    let content2 = std::fs::read_to_string(
        generator
            .save_persona(&Persona {
                supersedes_id: Some(42),
                ..persona
            })
            .unwrap(),
    )
    .unwrap();
    let fm2 = crate::util::frontmatter::extract_block(&content2).unwrap();
    assert_eq!(
        crate::util::frontmatter::parse_optional_i64(fm2, "supersedes_id"),
        Some(Some(42))
    );
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    assert_eq!(parts.len(), 3);
    assert!(parts[2].contains("## Preferences"));
    assert!(parts[2].contains("喜欢 Rust: 所有权系统"));
}

/// W3 回归：LLM 返回 5 个全空白字段时，generate 必须报错拒绝，不得覆盖已有 persona.md。
#[test]
fn test_w3_generate_rejects_all_blank_persona_without_overwriting() {
    let tmp = tempfile::TempDir::new().unwrap();
    let (url, _calls) = crate::memory::llm::spawn_test_llm_stub(vec![
        r#"{"preferences":"  ","identity":"\n","workflow":"","tech_stack":"\t","communication_style":"   "}"#,
    ]);
    let llm = LlmClient::new(&url, "k", "m");
    let generator = PersonaGenerator::new(&llm, tmp.path());
    let existing_path = tmp.path().join("persona.md");
    std::fs::write(&existing_path, "keep existing persona").unwrap();
    let scenarios = [Scenario {
        title: "t".to_string(),
        atom_ids: vec![],
        summary: "some scenario summary".to_string(),
        created_at: 0,
        updated_at: 0,
    }];

    let err = generator.generate(&scenarios).unwrap_err();
    assert!(
        err.to_string().contains("empty persona"),
        "all-blank persona must bail cleanly: {err}"
    );
    assert_eq!(
        std::fs::read_to_string(&existing_path).unwrap(),
        "keep existing persona"
    );
}

/// W13 回归：部分空白的响应不得销毁既有富内容——空白节回填上一版 persona.md
/// 的对应节。旧实现的守卫是"五档全空才拒"，一档非空即通过并整文档覆盖。
#[test]
fn test_w13_partial_blank_persona_preserves_prior_sections() {
    let tmp = tempfile::TempDir::new().unwrap();
    let seed_llm = LlmClient::new("test", "test", "test");
    let seed = PersonaGenerator::new(&seed_llm, tmp.path());
    seed.save_persona(&Persona {
        preferences: "旧偏好".to_string(),
        identity: "旧身份".to_string(),
        workflow: "旧流程".to_string(),
        tech_stack: "旧技术栈".to_string(),
        communication_style: "旧风格".to_string(),
        created_at: 1000,
        updated_at: 1000,
        supersedes_id: None,
    })
    .unwrap();

    let (url, _calls) = crate::memory::llm::spawn_test_llm_stub(vec![
        r#"{"preferences":"新偏好","identity":"","workflow":"","tech_stack":"","communication_style":""}"#,
    ]);
    let llm = LlmClient::new(&url, "k", "m");
    let generator = PersonaGenerator::new(&llm, tmp.path());
    let scenarios = [Scenario {
        title: "t".to_string(),
        atom_ids: vec![],
        summary: "some scenario summary".to_string(),
        created_at: 0,
        updated_at: 0,
    }];

    let p = generator.generate(&scenarios).unwrap();
    assert_eq!(p.preferences, "新偏好", "非空节用新内容");
    assert_eq!(p.identity, "旧身份", "W13: 空白节必须回填旧内容");
    assert_eq!(p.workflow, "旧流程");
    assert_eq!(p.tech_stack, "旧技术栈");
    assert_eq!(p.communication_style, "旧风格");
}

/// W16 回归：Windows CRLF（及 lone CR）persona.md 不得破坏 frontmatter
/// 剥离与节解析——空白节仍须回填旧内容（旧 split_once("\n---\n") 命不中
/// CRLF 闭合 fence，纯 \r 文件 lines() 不分行 → 回填全丢）。
#[test]
fn test_w16_crlf_and_cr_persona_md_backfill_sections() {
    for (label, eol) in [("crlf", "\r\n"), ("cr", "\r")] {
        let tmp = tempfile::TempDir::new().unwrap();
        let seed_llm = LlmClient::new("test", "test", "test");
        let seed = PersonaGenerator::new(&seed_llm, tmp.path());
        seed.save_persona(&Persona {
            preferences: "旧偏好".to_string(),
            identity: "旧身份".to_string(),
            workflow: "旧流程".to_string(),
            tech_stack: "旧技术栈".to_string(),
            communication_style: "旧风格".to_string(),
            created_at: 1000,
            updated_at: 1000,
            supersedes_id: None,
        })
        .unwrap();
        let path = tmp.path().join("persona.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, raw.replace('\n', eol)).unwrap();

        // 节解析直接钉住：五节齐全，frontmatter 不得漏进正文。
        let sections = prior_persona_sections(&path);
        assert_eq!(sections.len(), 5, "{label}: all five sections must parse");
        assert_eq!(sections.get("Identity").map(String::as_str), Some("旧身份"));
        assert!(
            !sections.values().any(|v| v.contains("created_at")),
            "{label}: frontmatter must be stripped, not scanned as body"
        );

        let (url, _calls) = crate::memory::llm::spawn_test_llm_stub(vec![
            r#"{"preferences":"新偏好","identity":"","workflow":"","tech_stack":"","communication_style":""}"#,
        ]);
        let llm = LlmClient::new(&url, "k", "m");
        let generator = PersonaGenerator::new(&llm, tmp.path());
        let scenarios = [Scenario {
            title: "t".to_string(),
            atom_ids: vec![],
            summary: "some scenario summary".to_string(),
            created_at: 0,
            updated_at: 0,
        }];
        let p = generator.generate(&scenarios).unwrap();
        assert_eq!(p.preferences, "新偏好", "{label}");
        assert_eq!(p.identity, "旧身份", "{label}: blank section backfill");
        assert_eq!(p.workflow, "旧流程", "{label}");
        assert_eq!(p.tech_stack, "旧技术栈", "{label}");
        assert_eq!(p.communication_style, "旧风格", "{label}");
    }
}

/// W16: load_persona_mirror 读回 persona.md 供 L4 整合输入——字段/时间戳
/// 往返、CRLF 兼容、缺失/全空 → None。
#[test]
fn test_w16_load_persona_mirror_roundtrip_and_absence() {
    let tmp = tempfile::TempDir::new().unwrap();
    assert!(load_persona_mirror(tmp.path()).is_none(), "missing file");

    let llm = LlmClient::new("test", "test", "test");
    let gen = PersonaGenerator::new(&llm, tmp.path());
    gen.save_persona(&Persona {
        preferences: "偏好".to_string(),
        identity: "身份".to_string(),
        workflow: "流程".to_string(),
        tech_stack: "技术栈".to_string(),
        communication_style: "风格".to_string(),
        created_at: 1000,
        updated_at: 2500,
        supersedes_id: None,
    })
    .unwrap();
    let p = load_persona_mirror(tmp.path()).expect("mirror must load");
    assert_eq!(p.identity, "身份");
    assert_eq!(p.workflow, "流程");
    assert_eq!(p.tech_stack, "技术栈");
    assert_eq!(p.communication_style, "风格");
    assert_eq!(p.preferences, "偏好");
    assert_eq!((p.created_at, p.updated_at), (1000, 2500));

    // CRLF 版本同样读回。
    let path = tmp.path().join("persona.md");
    let raw = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, raw.replace('\n', "\r\n")).unwrap();
    let p = load_persona_mirror(tmp.path()).expect("CRLF mirror must load");
    assert_eq!(p.identity, "身份");
    assert_eq!(p.updated_at, 2500);

    // 无节正文（手写散文）→ None。
    std::fs::write(&path, "# 手写画像\n没有节标题的散文。\n").unwrap();
    assert!(
        load_persona_mirror(tmp.path()).is_none(),
        "sectionless file"
    );
}
