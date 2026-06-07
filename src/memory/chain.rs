//! Evolution Chain: supersedes pointer chain for memory versioning
//!
//! When a memory entry is updated or contradicted, the new entry
//! points to the old one via `supersedes_id`. This preserves the
//! complete history of how a fact evolved over time.

use crate::index::db::Db;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Maximum chain depth to prevent infinite loops from circular supersedes references
const MAX_CHAIN_DEPTH: usize = 1000;

/// A single entry in the evolution chain
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainEntry {
    pub id: i64,
    pub target: String,
    pub content: String,
    pub memory_type: String,
    pub confidence_score: f64,
    pub created_at: i64,
    pub supersedes_id: Option<i64>,
}

/// Get the complete evolution chain for a memory entry (newest → oldest)
pub fn get_chain(db: &Db, entry_id: i64) -> anyhow::Result<Vec<ChainEntry>> {
    let mut chain = Vec::new();
    let mut current_id = Some(entry_id);
    let mut visited = HashSet::new();

    while let Some(id) = current_id {
        if !visited.insert(id) {
            tracing::warn!("Circular supersedes reference detected at id={}, breaking chain", id);
            break;
        }
        if chain.len() >= MAX_CHAIN_DEPTH {
            tracing::warn!("Evolution chain depth exceeded {}, truncating", MAX_CHAIN_DEPTH);
            break;
        }

        let entry = db.conn().query_row(
            "SELECT id, target, content, COALESCE(memory_type, 'manual'),
                    CASE confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END,
                    created_at, supersedes_id
             FROM bounded_memory WHERE id = ?1",
            rusqlite::params![id],
            |row| {
                Ok(ChainEntry {
                    id: row.get(0)?,
                    target: row.get(1)?,
                    content: row.get(2)?,
                    memory_type: row.get(3)?,
                    confidence_score: row.get(4)?,
                    created_at: row.get(5)?,
                    supersedes_id: row.get(6)?,
                })
            },
        )?;

        current_id = entry.supersedes_id;
        chain.push(entry);
    }

    Ok(chain)
}

/// Get only the latest version of a memory entry (follow supersedes chain to head)
///
/// Given any entry in the chain, returns the newest entry that supersedes it.
/// Uses iteration with cycle detection to prevent infinite loops.
pub fn get_latest_version(db: &Db, entry_id: i64) -> anyhow::Result<i64> {
    let mut current_id = entry_id;
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(current_id) {
            tracing::warn!("Circular supersedes reference detected at id={}, breaking", current_id);
            return Ok(current_id);
        }
        if visited.len() > MAX_CHAIN_DEPTH {
            tracing::warn!("Supersedes chain depth exceeded {}, truncating", MAX_CHAIN_DEPTH);
            return Ok(current_id);
        }

        // Find entry that supersedes the current entry.
        // Only QueryReturnedNoRows means "end of chain"; real DB errors must
        // propagate, otherwise a transient failure would return a stale/superseded
        // version as if it were the head.
        let result: Option<i64> = match db.conn().query_row(
            "SELECT id FROM bounded_memory WHERE supersedes_id = ?1",
            rusqlite::params![current_id],
            |row| row.get(0),
        ) {
            Ok(id) => Some(id),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };

        match result {
            Some(newer_id) => current_id = newer_id,
            None => return Ok(current_id),
        }
    }
}

/// Create a new memory entry that supersedes an existing one.
/// Returns the new entry's ID.
pub fn create_superseding(
    db: &Db,
    target: &str,
    content: &str,
    memory_type: &str,
    confidence_score: f64,
    source_turn_ids: Option<&str>,
    supersedes_id: i64,
) -> anyhow::Result<i64> {
    let now = crate::util::time::now_unix_ms();

    db.conn().execute(
        "INSERT INTO bounded_memory
         (target, content, created_at, updated_at, confidence,
          memory_type, supersedes_id, source_turn_ids)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            target,
            content,
            now,
            crate::memory::confidence_text(confidence_score),
            memory_type,
            supersedes_id,
            source_turn_ids,
        ],
    )?;

    Ok(db.conn().last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::db::Db;

    fn setup() -> Db {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn test_get_chain_single_entry() {
        let db = setup();
        let now = crate::util::time::now_unix_ms();

        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
                 VALUES ('memory', 'entry A', ?1, ?1, 'high')",
                rusqlite::params![now],
            )
            .unwrap();

        let id = db.conn().last_insert_rowid();
        let chain = get_chain(&db, id).unwrap();

        assert_eq!(chain.len(), 1);
        assert_eq!(chain[0].content, "entry A");
        assert!(chain[0].supersedes_id.is_none());
    }

    #[test]
    fn test_get_chain_with_supersedes() {
        let db = setup();
        let now = crate::util::time::now_unix_ms();

        // Create v1
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
                 VALUES ('memory', 'I like Rust', ?1, ?1, 'high')",
                rusqlite::params![now],
            )
            .unwrap();
        let v1_id = db.conn().last_insert_rowid();

        // Create v2 that supersedes v1
        db.conn()
            .execute(
                "INSERT INTO bounded_memory
                 (target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
                 VALUES ('memory', 'I prefer Python', ?1, ?1, 'high', 'atom', ?2)",
                rusqlite::params![now + 1000, v1_id],
            )
            .unwrap();
        let v2_id = db.conn().last_insert_rowid();

        // Chain from v2 should include both
        let chain = get_chain(&db, v2_id).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].content, "I prefer Python");
        assert_eq!(chain[1].content, "I like Rust");
    }

    #[test]
    fn test_get_latest_version() {
        let db = setup();
        let now = crate::util::time::now_unix_ms();

        // Create v1
        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
                 VALUES ('memory', 'v1', ?1, ?1, 'high')",
                rusqlite::params![now],
            )
            .unwrap();
        let v1_id = db.conn().last_insert_rowid();

        // Create v2 supersedes v1
        db.conn()
            .execute(
                "INSERT INTO bounded_memory
                 (target, content, created_at, updated_at, confidence, supersedes_id)
                 VALUES ('memory', 'v2', ?1, ?1, 'high', ?2)",
                rusqlite::params![now + 1000, v1_id],
            )
            .unwrap();
        let v2_id = db.conn().last_insert_rowid();

        // Create v3 supersedes v2
        db.conn()
            .execute(
                "INSERT INTO bounded_memory
                 (target, content, created_at, updated_at, confidence, supersedes_id)
                 VALUES ('memory', 'v3', ?1, ?1, 'high', ?2)",
                rusqlite::params![now + 2000, v2_id],
            )
            .unwrap();
        let v3_id = db.conn().last_insert_rowid();

        // From v1, latest should be v3
        assert_eq!(get_latest_version(&db, v1_id).unwrap(), v3_id);
        // From v3, latest is itself
        assert_eq!(get_latest_version(&db, v3_id).unwrap(), v3_id);
    }

    #[test]
    fn test_create_superseding() {
        let db = setup();
        let now = crate::util::time::now_unix_ms();

        db.conn()
            .execute(
                "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence)
                 VALUES ('memory', 'old fact', ?1, ?1, 'high')",
                rusqlite::params![now],
            )
            .unwrap();
        let old_id = db.conn().last_insert_rowid();

        let new_id = create_superseding(
            &db,
            "memory",
            "new fact",
            "atom",
            0.9,
            Some("[1,2,3]"),
            old_id,
        )
        .unwrap();

        let chain = get_chain(&db, new_id).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].content, "new fact");
        assert_eq!(chain[0].memory_type, "atom");
        assert_eq!(chain[1].content, "old fact");
    }
}
