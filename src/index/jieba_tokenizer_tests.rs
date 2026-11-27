use super::{register, JiebaTokenizer};
use crate::util::text::shared_jieba;
use rusqlite::Connection;

fn collect_tokens(tokenizer: &JiebaTokenizer, input: &[u8]) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    tokenizer
        .tokenize_with(input, |tok, range, _colocated| {
            out.push((
                String::from_utf8_lossy(tok).into_owned(),
                range.start,
                range.end,
            ));
            Ok(())
        })
        .unwrap();
    out
}

fn words(tokenizer: &JiebaTokenizer, input: &str) -> Vec<String> {
    collect_tokens(tokenizer, input.as_bytes())
        .into_iter()
        .map(|(w, _, _)| w)
        .collect()
}

#[test]
fn test_tokenizer_and_text_share_single_jieba_instance() {
    let p1 = shared_jieba() as *const _;
    let p2 = shared_jieba() as *const _;
    assert!(std::ptr::eq(p1, p2), "shared_jieba() must be a singleton");

    let conn = Connection::open_in_memory().unwrap();
    register(&conn).unwrap();
    conn.execute(
        "CREATE VIRTUAL TABLE t_shared USING fts5(text, tokenize='jieba')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO t_shared(text) VALUES ('中华人民共和国国歌'), ('rust 测试')",
        [],
    )
    .unwrap();
    let hits: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM t_shared WHERE t_shared MATCH '国歌'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);

    let raw = b"rust\xffasuna-memory";
    let tok = JiebaTokenizer::default();
    let emitted = collect_tokens(&tok, raw);
    for (_, start, end) in &emitted {
        assert!(
            *start < *end && *end <= raw.len(),
            "byte range {start}..{end} out of bounds for len {}",
            raw.len()
        );
    }
    let emitted_words: Vec<&str> = emitted.iter().map(|(w, _, _)| w.as_str()).collect();
    assert!(
        emitted_words.contains(&"rust") && emitted_words.contains(&"asuna-memori"),
        "expected 'rust' and 'asuna-memori' across invalid UTF-8 boundary, got {emitted_words:?}"
    );
}

#[test]
fn test_full_pipeline_011_connector_stopword_and_stemmer_semantics() {
    let default_tok = JiebaTokenizer::default();
    let no_stop_tok = JiebaTokenizer {
        enable_stopword: false,
    };

    // 1. Pure Chinese segmentation invariant under 0.11.0
    assert_eq!(
        words(&default_tok, "中华人民共和国国歌"),
        vec!["中华人民共和国", "国歌"]
    );
    assert_eq!(words(&default_tok, "单关键词"), vec!["单", "关键词"]);

    // 2. `.` / `_` / `-` connectors remain intact single tokens under 0.11.0
    assert_eq!(words(&default_tok, "asuna-memory"), vec!["asuna-memori"]);
    assert_eq!(
        words(&default_tok, "bounded_memory_fts"),
        vec!["bounded_memory_ft"]
    );
    assert_eq!(words(&default_tok, "v3.0.0"), vec!["v3.0.0"]);
    assert_eq!(words(&default_tok, "1.2.3"), vec!["1.2.3"]);
    assert_eq!(words(&default_tok, "config.json"), vec!["config.json"]);
    assert_eq!(
        words(&default_tok, "www.example.com"),
        vec!["www.example.com"]
    );
    assert_eq!(
        words(&default_tok, "state-of-the-art"),
        vec!["state-of-the-art"]
    );
    assert_eq!(words(&default_tok, "a-b"), vec!["a-b"]);

    // 3. Stemmer behavior: standalone vs underscore-joined words
    assert_eq!(words(&default_tok, "running tests"), vec!["run", "test"]);
    assert_eq!(words(&default_tok, "running_tests"), vec!["running_test"]);

    // 4. PR #158 `+` splitting & stopword interaction vs intact `c++` / `c#`
    assert_eq!(words(&default_tok, "foo+123"), vec!["foo", "123"]);
    assert!(words(&default_tok, "a+1").is_empty());
    assert_eq!(words(&no_stop_tok, "a+1"), vec!["a", "1"]);
    assert_eq!(words(&default_tok, "c++"), vec!["c++"]);
    assert_eq!(words(&default_tok, "c#"), vec!["c#"]);
    assert_eq!(words(&default_tok, "foo#bar"), vec!["foo", "bar"]);

    // 5. Byte ranges remain exact across an invalid UTF-8 byte boundary
    let raw = b"alpha \xff v3.0.0+foo";
    let tokens = collect_tokens(&default_tok, raw);
    assert_eq!(
        tokens,
        vec![
            ("alpha".to_string(), 0, 5),
            ("v3.0.0".to_string(), 8, 14),
            ("foo".to_string(), 15, 18),
        ]
    );
    for (_, start, end) in &tokens {
        assert!(*start < *end && *end <= raw.len());
        assert!(std::str::from_utf8(&raw[*start..*end]).is_ok());
    }
}
