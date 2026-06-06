/// 完整建表 SQL
pub const SCHEMA_SQL: &str = r#"
-- ════════════════════════════════════════════════
-- 会话索引表 (sessions)
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS sessions (
    session_id    TEXT    PRIMARY KEY,
    start_ts      INTEGER NOT NULL,
    end_ts        INTEGER,
    file_path     TEXT    NOT NULL,
    title         TEXT,
    summary       TEXT,
    profile_id    TEXT    DEFAULT 'default',
    source        TEXT,
    agent_model   TEXT,
    turn_count    INTEGER DEFAULT 0,
    total_tokens  INTEGER DEFAULT 0,
    tags          TEXT,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_start   ON sessions(start_ts);
CREATE INDEX IF NOT EXISTS idx_sessions_profile ON sessions(profile_id, start_ts);

-- ════════════════════════════════════════════════
-- 对话轮次索引表 (turns)
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS turns (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id    TEXT    NOT NULL REFERENCES sessions(session_id),
    seq           INTEGER NOT NULL,
    timestamp_ms  INTEGER NOT NULL,
    role          TEXT    NOT NULL,
    preview       TEXT,
    char_count    INTEGER DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_turns_ts      ON turns(timestamp_ms);
CREATE INDEX IF NOT EXISTS idx_turns_session ON turns(session_id, seq);

-- ════════════════════════════════════════════════
-- FTS5 全文检索虚拟表
-- ════════════════════════════════════════════════
CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
    preview,
    content='',
    content_rowid=id,
    tokenize='jieba'
);

-- ════════════════════════════════════════════════
-- 有界记忆索引表 (bounded_memory)
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS bounded_memory (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    target        TEXT    NOT NULL,
    content       TEXT    NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    source_session TEXT,
    confidence    TEXT    DEFAULT 'medium',
    memory_type   TEXT    DEFAULT 'manual',
    supersedes_id INTEGER REFERENCES bounded_memory(id),
    source_turn_ids TEXT,
    confidence_score REAL DEFAULT 1.0
);
CREATE INDEX IF NOT EXISTS idx_bounded_memory_type ON bounded_memory(memory_type);
CREATE INDEX IF NOT EXISTS idx_bounded_memory_supersedes ON bounded_memory(supersedes_id);

-- ════════════════════════════════════════════════
-- 有界记忆全文检索虚拟表 (bounded_memory_fts)
-- ════════════════════════════════════════════════
CREATE VIRTUAL TABLE IF NOT EXISTS bounded_memory_fts USING fts5(
    content,
    content='bounded_memory',
    content_rowid='id',
    tokenize='jieba'
);

-- ════════════════════════════════════════════════
-- 审计日志表 (audit_log)
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS audit_log (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp_ms  INTEGER NOT NULL,
    action        TEXT    NOT NULL,
    target        TEXT    NOT NULL,
    detail        TEXT,
    session_id    TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_log(timestamp_ms);

-- ════════════════════════════════════════════════
-- 向量检索虚拟表 (sqlite-vec)
-- ════════════════════════════════════════════════
-- 注意: vec0 向量表在 db.rs 中根据 config.embedding.dimensions 动态创建
-- CREATE VIRTUAL TABLE vec_turns USING vec0(embedding int8[{dim}]);

-- ════════════════════════════════════════════════
-- 有界记忆向量索引表 (bounded_memory embeddings)
-- ════════════════════════════════════════════════
-- 注意: vec0 向量表在 db.rs 中根据 config.embedding.dimensions 动态创建
-- CREATE VIRTUAL TABLE vec_bounded_memory USING vec0(
--     id INTEGER PRIMARY KEY,
--     embedding int8[{dim}]
-- );

-- ════════════════════════════════════════════════
-- 图谱实体表 (entities) — v1.3.0
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS entities (
    canonical    TEXT    PRIMARY KEY,
    name         TEXT    NOT NULL,
    entity_type  TEXT    NOT NULL DEFAULT 'unknown',
    first_seen   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    source_turn  INTEGER  -- 软引用 turns(id)：允许 turn 被裁剪后实体/关系仍保留
);
CREATE INDEX IF NOT EXISTS idx_entities_type ON entities(entity_type);

-- ════════════════════════════════════════════════
-- 图谱关系表 (relations) — v1.3.0
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS relations (
    src_canonical TEXT    NOT NULL REFERENCES entities(canonical) ON DELETE CASCADE,
    rel_type      TEXT    NOT NULL,
    dst_canonical TEXT    NOT NULL REFERENCES entities(canonical) ON DELETE CASCADE,
    confidence    REAL    NOT NULL DEFAULT 0.5,
    source_turn   INTEGER,  -- 软引用 turns(id)：允许 turn 被裁剪后关系仍保留
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (src_canonical, rel_type, dst_canonical)
);
CREATE INDEX IF NOT EXISTS idx_relations_dst ON relations(dst_canonical, rel_type);
CREATE INDEX IF NOT EXISTS idx_relations_src_turn ON relations(source_turn);
"#;

/// P3 migration SQL: add memory_type, supersedes_id, source_turn_ids, confidence_score
/// to bounded_memory table. Safe to run multiple times (uses IF NOT EXISTS pattern).
pub const MIGRATION_P3_SQL: &str = r#"
-- Add memory_type column if not exists
ALTER TABLE bounded_memory ADD COLUMN memory_type TEXT DEFAULT 'manual';
-- Add supersedes_id column if not exists
ALTER TABLE bounded_memory ADD COLUMN supersedes_id INTEGER REFERENCES bounded_memory(id);
-- Add source_turn_ids column if not exists
ALTER TABLE bounded_memory ADD COLUMN source_turn_ids TEXT;
-- Add confidence_score column if not exists
ALTER TABLE bounded_memory ADD COLUMN confidence_score REAL DEFAULT 1.0;
-- Create indexes
CREATE INDEX IF NOT EXISTS idx_bounded_memory_type ON bounded_memory(memory_type);
CREATE INDEX IF NOT EXISTS idx_bounded_memory_supersedes ON bounded_memory(supersedes_id);
"#;

/// P8 migration SQL: add memory_atom_id to entities and relation_kind to relations
/// for memory graphification. Safe to run multiple times.
///
/// Note: Migration is executed statement-by-statement in db.rs to ensure
/// ALTER TABLE completes before CREATE INDEX.
pub const MIGRATION_P8_ALTER_SQL: &str = r#"
ALTER TABLE entities ADD COLUMN memory_atom_id INTEGER;
ALTER TABLE relations ADD COLUMN relation_kind TEXT DEFAULT 'asserted';
"#;

pub const MIGRATION_P8_INDEX_SQL: &str = r#"
CREATE INDEX IF NOT EXISTS idx_entities_memory_atom ON entities(memory_atom_id);
CREATE INDEX IF NOT EXISTS idx_relations_kind ON relations(relation_kind);
"#;

/// FTS5 同步触发器：turns 插入时自动同步到 turns_fts
///
/// jieba FTS5 tokenizer 在 FTS5 引擎内部自动分词，触发器无需预处理文本。
/// 外部工具（Python sqlite3、sqlite3 CLI 等）可直接 INSERT/UPDATE/DELETE，
/// 不再依赖 Rust UDF。
pub const FTS_TRIGGERS_SQL: &str = r#"
DROP TRIGGER IF EXISTS turns_ai;
CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN
    INSERT INTO turns_fts(rowid, preview) VALUES (new.id, new.preview);
END;

DROP TRIGGER IF EXISTS turns_ad;
CREATE TRIGGER turns_ad AFTER DELETE ON turns BEGIN
    INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, old.preview);
END;

DROP TRIGGER IF EXISTS turns_au;
CREATE TRIGGER turns_au AFTER UPDATE ON turns BEGIN
    INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, old.preview);
    INSERT INTO turns_fts(rowid, preview) VALUES (new.id, new.preview);
END;

-- ════════════════════════════════════════════════
-- bounded_memory_fts 同步触发器
-- ════════════════════════════════════════════════
-- bounded_memory_fts 使用 external-content 模式（content='bounded_memory'）。
-- jieba tokenizer 在 FTS5 引擎内部自动分词，触发器直接传递原始文本。
DROP TRIGGER IF EXISTS bounded_memory_ai;
CREATE TRIGGER bounded_memory_ai AFTER INSERT ON bounded_memory BEGIN
    INSERT INTO bounded_memory_fts(rowid, content) VALUES (new.id, new.content);
END;

DROP TRIGGER IF EXISTS bounded_memory_ad;
CREATE TRIGGER bounded_memory_ad AFTER DELETE ON bounded_memory BEGIN
    INSERT INTO bounded_memory_fts(bounded_memory_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;

DROP TRIGGER IF EXISTS bounded_memory_au;
CREATE TRIGGER bounded_memory_au AFTER UPDATE ON bounded_memory BEGIN
    INSERT INTO bounded_memory_fts(bounded_memory_fts, rowid, content) VALUES ('delete', old.id, old.content);
    INSERT INTO bounded_memory_fts(rowid, content) VALUES (new.id, new.content);
END;
"#;
