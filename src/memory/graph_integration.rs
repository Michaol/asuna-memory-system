//! Graph integration for memory atoms
//!
//! When L1 atoms are stored, automatically create graph entities and relations:
//! - Each atom gets a corresponding entity (with memory_atom_id)
//! - `mentions` relations: atom -> extracted entities
//! - `supersedes` relations: new_atom -> old_atom (for conflicts)
//! - `from_session` relations: atom -> session
//! - `related_to` relations: atom -> similar_atom (high vector similarity)

use crate::graph::canonical::canonicalize;
use crate::index::db::Db;
use crate::util::time;
use anyhow::Result;
use rusqlite::params;

/// Result of graph integration for a single atom
#[derive(Debug, Clone)]
pub struct GraphIntegrationResult {
    pub atom_entity_canonical: String,
    pub mentions_created: u32,
    pub supersedes_created: u32,
    pub from_session_created: bool,
    pub related_to_created: u32,
}

/// Integrate an L1 atom with the knowledge graph
///
/// Creates an entity for the atom and derives relations based on:
/// - Extracted entities from atom content
/// - Supersedes chain (if atom.supersedes_id is set)
/// - Source session
/// - Vector similarity to other atoms
pub fn integrate_atom_with_graph(
    db: &Db,
    atom_id: i64,
    atom_content: &str,
    supersedes_id: Option<i64>,
    session_id: Option<&str>,
    extracted_entities: &[String],
    similar_atom_ids: &[i64],
) -> Result<GraphIntegrationResult> {
    let conn = db.conn();
    let now = time::now_unix_ms();

    // 1. Create entity for this atom
    let atom_canonical = canonicalize(&format!("atom_{}", atom_id));
    conn.execute(
        "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen, memory_atom_id)
         VALUES (?1, ?2, 'memory_atom', ?3, ?3, ?4)",
        params![atom_canonical, atom_content, now, atom_id],
    )?;

    let mut mentions_created = 0u32;
    let mut supersedes_created = 0u32;
    let mut from_session_created = false;
    let mut related_to_created = 0u32;

    // 2. Create `mentions` relations for extracted entities
    for entity_name in extracted_entities {
        let entity_canonical = canonicalize(entity_name);

        // Ensure entity exists
        conn.execute(
            "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen)
             VALUES (?1, ?2, 'extracted', ?3, ?3)",
            params![entity_canonical, entity_name, now],
        )?;

        // Create mentions relation (atom -> entity)
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'mentions', ?2, 0.8, NULL, 'derived', ?3)",
            params![atom_canonical, entity_canonical, now],
        )?;

        if inserted > 0 {
            mentions_created += 1;
        }
    }

    // 3. Create `supersedes` relation if this atom supersedes another
    if let Some(old_atom_id) = supersedes_id {
        let old_atom_canonical = canonicalize(&format!("atom_{}", old_atom_id));

        // Ensure old atom entity exists (should already exist, but just in case)
        conn.execute(
            "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen, memory_atom_id)
             SELECT ?1, content, 'memory_atom', created_at, created_at, id
             FROM bounded_memory WHERE id = ?2",
            params![old_atom_canonical, old_atom_id],
        )?;

        // Create supersedes relation
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'supersedes', ?2, 1.0, NULL, 'derived', ?3)",
            params![atom_canonical, old_atom_canonical, now],
        )?;

        if inserted > 0 {
            supersedes_created += 1;
        }
    }

    // 4. Create `from_session` relation
    if let Some(session_id) = session_id {
        let session_canonical = canonicalize(&format!("session_{}", session_id));

        // Ensure session entity exists
        conn.execute(
            "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen)
             VALUES (?1, ?2, 'session', ?3, ?3)",
            params![session_canonical, session_id, now],
        )?;

        // Create from_session relation
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'from_session', ?2, 1.0, NULL, 'derived', ?3)",
            params![atom_canonical, session_canonical, now],
        )?;

        if inserted > 0 {
            from_session_created = true;
        }
    }

    // 5. Create `related_to` relations for similar atoms
    for similar_id in similar_atom_ids {
        let similar_canonical = canonicalize(&format!("atom_{}", similar_id));

        // Ensure similar atom entity exists
        conn.execute(
            "INSERT OR IGNORE INTO entities (canonical, name, entity_type, first_seen, last_seen, memory_atom_id)
             SELECT ?1, content, 'memory_atom', created_at, created_at, id
             FROM bounded_memory WHERE id = ?2",
            params![similar_canonical, similar_id],
        )?;

        // Create related_to relation (bidirectional)
        let inserted1 = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'related_to', ?2, 0.7, NULL, 'derived', ?3)",
            params![atom_canonical, similar_canonical, now],
        )?;

        let inserted2 = conn.execute(
            "INSERT OR IGNORE INTO relations (src_canonical, rel_type, dst_canonical, confidence, source_turn, relation_kind, created_at)
             VALUES (?1, 'related_to', ?2, 0.7, NULL, 'derived', ?3)",
            params![similar_canonical, atom_canonical, now],
        )?;

        if inserted1 > 0 || inserted2 > 0 {
            related_to_created += 1;
        }
    }

    Ok(GraphIntegrationResult {
        atom_entity_canonical: atom_canonical,
        mentions_created,
        supersedes_created,
        from_session_created,
        related_to_created,
    })
}

/// Multi-hop query: find all atoms connected to a given entity
///
/// Traverses the graph up to `max_hops` hops from the starting entity,
/// collecting all memory atoms encountered along the way.
pub fn multi_hop_query(
    db: &Db,
    start_entity: &str,
    max_hops: u32,
    relation_filter: Option<&str>,
) -> Result<Vec<i64>> {
    let conn = db.conn();
    let start_canonical = canonicalize(start_entity);

    let mut visited = std::collections::HashSet::new();
    let mut current_frontier = vec![start_canonical];
    let mut atom_ids = Vec::new();

    for _hop in 0..max_hops {
        let mut next_frontier = Vec::new();

        for canonical in &current_frontier {
            if visited.contains(canonical) {
                continue;
            }
            visited.insert(canonical.clone());

            // Query relations from this entity
            let neighbors: Vec<String> = if let Some(rel_type) = relation_filter {
                let sql = "SELECT dst_canonical FROM relations
                     WHERE src_canonical = ?1 AND rel_type = ?2 AND relation_kind IN ('asserted', 'derived')
                     UNION
                     SELECT src_canonical FROM relations
                     WHERE dst_canonical = ?1 AND rel_type = ?2 AND relation_kind IN ('asserted', 'derived')";
                let mut stmt = conn.prepare(sql)?;
                let result = stmt.query_map(params![canonical, rel_type], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                result
            } else {
                let sql = "SELECT dst_canonical FROM relations
                     WHERE src_canonical = ?1 AND relation_kind IN ('asserted', 'derived')
                     UNION
                     SELECT src_canonical FROM relations
                     WHERE dst_canonical = ?1 AND relation_kind IN ('asserted', 'derived')";
                let mut stmt = conn.prepare(sql)?;
                let result = stmt.query_map(params![canonical], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                result
            };

            for neighbor in neighbors {

                // Check if this neighbor is a memory atom
                let atom_id: Option<i64> = conn
                    .query_row(
                        "SELECT memory_atom_id FROM entities WHERE canonical = ?1 AND memory_atom_id IS NOT NULL",
                        params![neighbor],
                        |row| row.get(0),
                    )
                    .ok();

                if let Some(id) = atom_id {
                    atom_ids.push(id);
                }

                if !visited.contains(&neighbor) {
                    next_frontier.push(neighbor);
                }
            }
        }

        current_frontier = next_frontier;

        if current_frontier.is_empty() {
            break;
        }
    }

    // Deduplicate atom IDs
    atom_ids.sort_unstable();
    atom_ids.dedup();

    Ok(atom_ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::db::Db;

    #[test]
    fn test_integrate_atom_with_graph() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // Insert a test atom
        db.conn().execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'User prefers Rust', 0, 0, 'medium', 'atom')",
            [],
        ).unwrap();
        let atom_id = db.conn().last_insert_rowid();

        let result = integrate_atom_with_graph(
            &db,
            atom_id,
            "User prefers Rust",
            None,
            Some("session_123"),
            &["Rust".to_string(), "programming".to_string()],
            &[],
        ).unwrap();

        assert_eq!(result.mentions_created, 2);
        // from_session_created depends on whether session_id was provided
        assert_eq!(result.supersedes_created, 0);
        assert_eq!(result.related_to_created, 0);

        // Verify entities were created
        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM entities WHERE entity_type = 'memory_atom'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 1);

        let count: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM entities WHERE entity_type = 'extracted'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_supersedes_relation() {
        let db = Db::open_memory().unwrap();
        db.init_schema().unwrap();

        // Insert old atom
        db.conn().execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type)
             VALUES ('memory', 'User likes Python', 0, 0, 'medium', 'atom')",
            [],
        ).unwrap();
        let old_id = db.conn().last_insert_rowid();

        // Insert new atom that supersedes old
        db.conn().execute(
            "INSERT INTO bounded_memory (target, content, created_at, updated_at, confidence, memory_type, supersedes_id)
             VALUES ('memory', 'User prefers Rust', 0, 0, 'medium', 'atom', ?1)",
            params![old_id],
        ).unwrap();
        let new_id = db.conn().last_insert_rowid();

        let result = integrate_atom_with_graph(
            &db,
            new_id,
            "User prefers Rust",
            Some(old_id),
            None,
            &[],
            &[],
        ).unwrap();

        assert_eq!(result.supersedes_created, 1);
    }
}
