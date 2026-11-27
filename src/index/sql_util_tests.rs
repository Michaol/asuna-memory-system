use super::{for_each_in_chunk, numbered_placeholders, SQL_IN_CHUNK_SIZE};

#[test]
fn test_numbered_placeholders_formatting() {
    assert_eq!(numbered_placeholders(0), "");
    assert_eq!(numbered_placeholders(1), "?1");
    assert_eq!(numbered_placeholders(3), "?1, ?2, ?3");
}

#[test]
fn test_for_each_in_chunk_boundaries() {
    let empty: Vec<i64> = Vec::new();
    let mut empty_calls = 0usize;
    for_each_in_chunk::<i64, anyhow::Error, _>(&empty, |_, _| {
        empty_calls += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(empty_calls, 0);

    let items: Vec<i64> = (1..=(SQL_IN_CHUNK_SIZE as i64 + 2)).collect();
    let mut seen: Vec<(usize, String, i64)> = Vec::new();
    for_each_in_chunk::<i64, anyhow::Error, _>(&items, |chunk, placeholders| {
        seen.push((chunk.len(), placeholders.to_string(), chunk[0]));
        Ok(())
    })
    .unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].0, SQL_IN_CHUNK_SIZE);
    assert!(seen[0].1.starts_with("?1, ?2, "));
    assert!(seen[0].1.ends_with(", ?500"));
    assert_eq!(seen[0].2, 1);
    assert_eq!(seen[1].0, 2);
    assert_eq!(seen[1].1, "?1, ?2");
    assert_eq!(seen[1].2, 501);
}
