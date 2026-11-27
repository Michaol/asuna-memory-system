use super::*;

#[test]
fn test_cluster_atoms_empty() {
    let atoms: Vec<(i64, String, Vec<f32>)> = vec![];
    let clusters = ScenarioAggregator::cluster_atoms(&atoms, 0.8);
    assert!(clusters.is_empty());
}

#[test]
fn test_cluster_atoms_single() {
    let atoms = vec![(1, "test".to_string(), vec![1.0, 0.0, 0.0])];
    let clusters = ScenarioAggregator::cluster_atoms(&atoms, 0.8);
    assert!(clusters.is_empty()); // Single atom doesn't form a cluster
}

#[test]
fn test_cluster_atoms_similar() {
    let atoms = vec![
        (1, "rust programming".to_string(), vec![1.0, 0.0, 0.0]),
        (2, "rust language".to_string(), vec![0.95, 0.0, 0.0]),
        (3, "python code".to_string(), vec![0.0, 1.0, 0.0]),
    ];
    let clusters = ScenarioAggregator::cluster_atoms(&atoms, 0.8);
    assert_eq!(clusters.len(), 1); // Only the rust pair forms a cluster
    assert_eq!(clusters[0].len(), 2);
}

/// U1/J12: the mirror frontmatter must be valid YAML even for a title
/// containing ": " (the old hand-formatted write produced `title: Rust:
/// 所有权` → invalid), and the filename must map back to the DB row id.
#[test]
fn test_save_scenario_writes_valid_yaml_frontmatter() {
    let tmp = tempfile::TempDir::new().unwrap();
    let llm = LlmClient::new("test", "test", "test");
    let aggregator = ScenarioAggregator::new(&llm, tmp.path());

    let scenario = Scenario {
        title: "Rust: 所有权与生命周期".to_string(),
        atom_ids: vec![11, 22],
        summary: "用户在调试 Rust 借用检查。\n跨多行。".to_string(),
        created_at: 1000,
        updated_at: 2000,
    };
    let path = aggregator.save_scenario(&scenario, 7).unwrap();
    assert_eq!(path.file_name().unwrap().to_str().unwrap(), "1000_7.md");

    let content = std::fs::read_to_string(&path).unwrap();
    // 闭合分隔符必须独占一行：splitn("---") 对粘连形态（`2000---`）过于宽容
    assert!(content.starts_with("---\n"));
    assert!(
        content.get(4..).is_some_and(|s| s.contains("\n---\n")),
        "closing --- must start its own line: {:?}",
        content
    );
    let fm = crate::util::frontmatter::extract_block(&content)
        .expect("frontmatter must have balanced fences");
    assert!(
        fm.contains("title: \"Rust: 所有权与生命周期\""),
        "title with ': ' must be quoted: {fm:?}"
    );
    assert_eq!(
        crate::util::frontmatter::parse_string(fm, "title").as_deref(),
        Some("Rust: 所有权与生命周期")
    );
    assert_eq!(
        crate::util::frontmatter::parse_i64_list(fm, "atom_ids"),
        Some(vec![11, 22])
    );
    assert_eq!(
        crate::util::frontmatter::parse_i64(fm, "created_at"),
        Some(1000)
    );
    assert_eq!(
        crate::util::frontmatter::parse_i64(fm, "updated_at"),
        Some(2000)
    );
    let parts: Vec<&str> = content.splitn(3, "---").collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[2].trim(), "用户在调试 Rust 借用检查。\n跨多行。");
}
