//! Evolution Chain: supersedes pointer chain for memory versioning
//!
//! When a memory entry is updated or contradicted, the new entry
//! points to the old one via `supersedes_id`. This preserves the
//! complete history of how a fact evolved over time.

use crate::index::db::Db;
#[cfg(test)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::HashSet;

/// Maximum chain depth to prevent infinite loops from circular supersedes references
#[cfg(test)]
const MAX_CHAIN_DEPTH: usize = 1000;

/// A single entry in the evolution chain
#[cfg(test)]
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
#[cfg(test)]
pub fn get_chain(db: &Db, entry_id: i64) -> anyhow::Result<Vec<ChainEntry>> {
    let mut chain = Vec::new();
    let mut current_id = Some(entry_id);
    let mut visited = HashSet::new();

    while let Some(id) = current_id {
        if !visited.insert(id) {
            tracing::warn!(
                "Circular supersedes reference detected at id={}, breaking chain",
                id
            );
            break;
        }
        if chain.len() >= MAX_CHAIN_DEPTH {
            tracing::warn!(
                "Evolution chain depth exceeded {}, truncating",
                MAX_CHAIN_DEPTH
            );
            break;
        }

        // C14-a: read the persisted REAL score column, not the TEXT bucket
        // re-derived via CASE; the bucket expression only backs NULL legacy
        // rows (schema default is 1.0, so NULL means hand-written SQL).
        // Only QueryReturnedNoRows means "end of chain" (dangling supersedes
        // pointer after a hard delete) — truncate here, same posture as
        // get_latest_version; real DB errors still propagate.
        let entry = match db.conn().query_row(
            "SELECT id, target, content, COALESCE(memory_type, 'manual'),
                    COALESCE(confidence_score,
                             CASE confidence WHEN 'high' THEN 1.0 WHEN 'medium' THEN 0.5 ELSE 0.25 END),
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
        ) {
            Ok(entry) => entry,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                tracing::warn!(
                    "Evolution chain truncated at missing row id={} (dangling supersedes pointer)",
                    id
                );
                break;
            }
            Err(e) => return Err(e.into()),
        };

        current_id = entry.supersedes_id;
        chain.push(entry);
    }

    Ok(chain)
}

/// Get only the latest version of a memory entry (follow supersedes chain to head)
///
/// Given any entry in the chain, returns the newest entry that supersedes it.
/// Uses iteration with cycle detection to prevent infinite loops.
#[cfg(test)]
pub fn get_latest_version(db: &Db, entry_id: i64) -> anyhow::Result<i64> {
    let mut current_id = entry_id;
    let mut visited = HashSet::new();

    loop {
        if !visited.insert(current_id) {
            tracing::warn!(
                "Circular supersedes reference detected at id={}, breaking",
                current_id
            );
            return Ok(current_id);
        }
        if visited.len() > MAX_CHAIN_DEPTH {
            tracing::warn!(
                "Supersedes chain depth exceeded {}, truncating",
                MAX_CHAIN_DEPTH
            );
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
         (target, content, created_at, updated_at, confidence, confidence_score,
          memory_type, supersedes_id, source_turn_ids)
         VALUES (?1, ?2, ?3, ?3, ?4, ?8, ?5, ?6, ?7)",
        rusqlite::params![
            target,
            content,
            now,
            crate::memory::confidence_text(confidence_score),
            memory_type,
            supersedes_id,
            source_turn_ids,
            // C14-a: persist the real score, not just the TEXT bucket — this
            // is the column's design purpose (schema default 1.0 otherwise).
            confidence_score,
        ],
    )?;

    Ok(db.conn().last_insert_rowid())
}

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
