/// Default chunk size for `IN (?1, ...)` SQLite queries so parameter counts
/// stay safely below `SQLITE_MAX_VARIABLE_NUMBER`.
pub const SQL_IN_CHUNK_SIZE: usize = 500;

/// Build numbered SQL positional placeholders `"?1, ?2, ..., ?N"`.
///
/// Numbered placeholders (`?1`, `?2`, ...) are required when callers reuse
/// the same `IN ({placeholders})` list across `UNION` branches or append
/// trailing indexed bindings such as `?{count + 1}`.
pub fn numbered_placeholders(count: usize) -> String {
    (1..=count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Split `items` into [`SQL_IN_CHUNK_SIZE`] slices and invoke `f(chunk, placeholders)`
/// for each non-empty chunk with numbered placeholders `?1..=?chunk.len()`.
pub fn for_each_in_chunk<T, E, F>(items: &[T], mut f: F) -> Result<(), E>
where
    F: FnMut(&[T], &str) -> Result<(), E>,
{
    for chunk in items.chunks(SQL_IN_CHUNK_SIZE) {
        let placeholders = numbered_placeholders(chunk.len());
        f(chunk, &placeholders)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "sql_util_tests.rs"]
mod tests;
