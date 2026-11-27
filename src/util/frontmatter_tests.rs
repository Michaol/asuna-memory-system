use super::*;

#[test]
fn roundtrips_persona_and_rejects_multi_doc() {
    let doc = format!("---\n{}---\n\nbody\n", format_persona(1000, 2000, None));
    let fm = extract_block(&doc).expect("block");
    assert_eq!(parse_i64(fm, "created_at"), Some(1000));
    assert_eq!(parse_i64(fm, "updated_at"), Some(2000));
    assert_eq!(parse_optional_i64(fm, "supersedes_id"), Some(None));

    let with_id = format_persona(10, 20, Some(42));
    assert_eq!(
        parse_optional_i64(&with_id, "supersedes_id"),
        Some(Some(42))
    );

    let multi = "updated_at: 2500\n---\nbody: true\n";
    assert_eq!(parse_i64(multi, "updated_at"), None);
}

#[test]
fn roundtrips_scenario_and_legacy_yaml_shapes() {
    let fm = format_scenario(
        "Rust: 所有权 \"引号\" & 换行\n第二行 # 标签",
        &[11, 22],
        1000,
        2000,
    );
    assert_eq!(
        parse_string(&fm, "title").as_deref(),
        Some("Rust: 所有权 \"引号\" & 换行\n第二行 # 标签")
    );
    assert_eq!(parse_i64_list(&fm, "atom_ids"), Some(vec![11, 22]));
    assert_eq!(parse_i64(&fm, "created_at"), Some(1000));
    assert_eq!(parse_i64(&fm, "updated_at"), Some(2000));

    // Legacy YAML outputs: single-quoted title + block list
    let legacy =
        "title: '带冒号: ''测试'''\natom_ids:\n  - 7\n  - 8\ncreated_at: 100\nupdated_at: 200\n";
    assert_eq!(
        parse_string(legacy, "title").as_deref(),
        Some("带冒号: '测试'")
    );
    assert_eq!(parse_i64_list(legacy, "atom_ids"), Some(vec![7, 8]));

    // Plain scalar with unquoted ": " is invalid YAML syntax and must be rejected.
    assert_eq!(parse_string("title: invalid: colon\n", "title"), None);
    assert_eq!(parse_i64_list("atom_ids: []\n", "atom_ids"), Some(vec![]));
}

#[test]
fn handles_crlf_blocks_and_standalone_closing_fence() {
    let mixed = "---\r\nupdated_at: 2500\r\n---\r\n\r\n正文\r\n---\r\n后文\r\n";
    let fm = extract_block(mixed).expect("crlf block");
    assert_eq!(parse_i64(fm, "updated_at"), Some(2500));

    assert!(extract_block("---\nupdated_at: 2500\n----\n").is_none());
}

/// W18b: an indented "  ---" line is content, not a multi-doc marker —
/// only a column-0 "---" / "..." line rejects the block.
#[test]
fn indented_dashes_are_not_multi_doc_marker() {
    let fm = "created_at: 100\n  ---\nupdated_at: 200\n";
    assert_eq!(parse_i64(fm, "created_at"), Some(100));
    assert_eq!(parse_i64(fm, "updated_at"), Some(200));
    // extract_block likewise only honors a column-0 closing fence
    let doc = "---\ncreated_at: 100\n  ---\nupdated_at: 200\n---\nbody\n";
    let block = extract_block(doc).expect("block");
    assert_eq!(parse_i64(block, "updated_at"), Some(200));
}

/// W18b: unquoted YAML indicator characters are not plain scalars.
#[test]
fn unquoted_yaml_indicators_rejected() {
    for indicator in ["|", ">", "[", "{", "&", "*", "!", "%", "@", "`"] {
        let fm = format!("title: {indicator}\n");
        assert_eq!(parse_string(&fm, "title"), None, "indicator {indicator:?}");
    }
}

/// W18b: YAML 1.2 double-quoted escapes (\a \v \e \space \x \u \U) decode.
#[test]
fn double_quoted_yaml_12_escapes_decode() {
    let fm = "title: \"\\a\\v\\e\\ \\x41\\u0042\\U0001F600\"\n";
    assert_eq!(
        parse_string(fm, "title").as_deref(),
        Some("\u{0007}\u{000B}\u{001B} AB\u{1F600}")
    );
}

/// W18b: a comment after a quoted scalar requires whitespace before '#'.
#[test]
fn quoted_scalar_comment_requires_whitespace() {
    assert_eq!(parse_string("title: \"foo\"#comment\n", "title"), None);
    assert_eq!(
        parse_string("title: \"foo\" # comment\n", "title").as_deref(),
        Some("foo")
    );
}

/// W18b: block i64 list items require "- " or "-\t"; "-42" (no space) is not
/// a sequence item and must be rejected.
#[test]
fn block_i64_list_requires_space_after_dash() {
    assert_eq!(parse_i64_list("atom_ids:\n-42\n", "atom_ids"), None);
    assert_eq!(
        parse_i64_list("atom_ids:\n- 42\n", "atom_ids"),
        Some(vec![42])
    );
    assert_eq!(
        parse_i64_list("atom_ids:\n-\t42\n", "atom_ids"),
        Some(vec![42])
    );
}
