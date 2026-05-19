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
    tokenize='unicode61 remove_diacritics 2'
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
    confidence    TEXT    DEFAULT 'medium'
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
-- 注意: 此表在 sqlite-vec 扩展加载后通过 db.rs 单独创建
-- CREATE VIRTUAL TABLE vec_turns USING vec0(embedding int8[768]);

-- ════════════════════════════════════════════════
-- 图谱实体表 (entities) — v1.3.0
-- ════════════════════════════════════════════════
CREATE TABLE IF NOT EXISTS entities (
    canonical    TEXT    PRIMARY KEY,
    name         TEXT    NOT NULL,
    entity_type  TEXT    NOT NULL DEFAULT 'unknown',
    first_seen   INTEGER NOT NULL,
    last_seen    INTEGER NOT NULL,
    source_turn  INTEGER
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
    source_turn   INTEGER,
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (src_canonical, rel_type, dst_canonical)
);
CREATE INDEX IF NOT EXISTS idx_relations_dst ON relations(dst_canonical, rel_type);
CREATE INDEX IF NOT EXISTS idx_relations_src_turn ON relations(source_turn);
"#;

/// FTS5 同步触发器：turns 插入时自动同步到 turns_fts
pub const FTS_TRIGGERS_SQL: &str = r#"
DROP TRIGGER IF EXISTS turns_ai;
CREATE TRIGGER turns_ai AFTER INSERT ON turns BEGIN
    INSERT INTO turns_fts(rowid, preview) VALUES (new.id, tokenize_zh(new.preview));
END;

DROP TRIGGER IF EXISTS turns_ad;
CREATE TRIGGER turns_ad AFTER DELETE ON turns BEGIN
    INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, tokenize_zh(old.preview));
END;

DROP TRIGGER IF EXISTS turns_au;
CREATE TRIGGER turns_au AFTER UPDATE ON turns BEGIN
    INSERT INTO turns_fts(turns_fts, rowid, preview) VALUES ('delete', old.id, tokenize_zh(old.preview));
    INSERT INTO turns_fts(rowid, preview) VALUES (new.id, tokenize_zh(new.preview));
END;
"#;
