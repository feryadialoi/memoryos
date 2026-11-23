-- SQLite equivalent of PostgreSQL migration 0006: insights (docs/SPEC.md section 8, Insights;
-- ADR 0008). Conventions follow 0001: UUIDs are canonical lowercase hyphenated TEXT, timestamps are
-- Unix microseconds, tenant-aware foreign keys reject cross-tenant references, and every query
-- carries an explicit tenant predicate.

-- row_id is an INTEGER PRIMARY KEY alias so vec0 and FTS5 rows stay attached across VACUUM, as for
-- facts in 0001. Base and vector rows are written or removed in one transaction.
CREATE TABLE insights (
    row_id        INTEGER PRIMARY KEY,
    id            TEXT NOT NULL UNIQUE CHECK (length(id) = 36),
    tenant        TEXT NOT NULL,
    scope         TEXT NOT NULL,
    level         TEXT NOT NULL CHECK (level IN ('context', 'understanding', 'guidance')),
    statement     TEXT NOT NULL CHECK (length(statement) BETWEEN 1 AND 1000),
    trust         TEXT NOT NULL CHECK (trust IN ('external', 'agent', 'user', 'system')),
    status        TEXT NOT NULL
        CHECK (status IN ('active', 'stale', 'quarantined', 'rejected', 'retired', 'superseded')),
    -- JSON: which sources changed and how, while status is 'stale'.
    stale_reason  TEXT,
    valid_from    INTEGER NOT NULL,
    valid_to      INTEGER,
    superseded_by TEXT,
    pending_supersede_of TEXT,
    written_by    TEXT NOT NULL,
    recorded_at   INTEGER NOT NULL,
    UNIQUE (tenant, id),
    CHECK ((status = 'stale') = (stale_reason IS NOT NULL))
);
CREATE INDEX insights_scope_idx ON insights (tenant, scope, status, level);
CREATE INDEX insights_quarantined_idx ON insights (tenant, recorded_at) WHERE status = 'quarantined';

CREATE VIRTUAL TABLE insight_embeddings USING vec0(embedding float[384]);

-- Cascading deletes skip vec0, so this trigger removes an insight's vector with it.
CREATE TRIGGER insights_vec_delete AFTER DELETE ON insights BEGIN
    DELETE FROM insight_embeddings WHERE rowid = old.row_id;
END;

CREATE VIRTUAL TABLE insights_fts USING fts5(
    statement,
    content='insights',
    content_rowid='row_id',
    tokenize='unicode61'
);

CREATE TRIGGER insights_fts_insert AFTER INSERT ON insights BEGIN
    INSERT INTO insights_fts (rowid, statement) VALUES (new.row_id, new.statement);
END;
CREATE TRIGGER insights_fts_delete AFTER DELETE ON insights BEGIN
    INSERT INTO insights_fts (insights_fts, rowid, statement) VALUES ('delete', old.row_id, old.statement);
END;

-- No foreign key to facts: a purged fact leaves its row here, which is how the insight reports the
-- source as purged.
CREATE TABLE insight_sources (
    insight_id  TEXT NOT NULL,
    tenant      TEXT NOT NULL,
    source_kind TEXT NOT NULL CHECK (source_kind IN ('fact', 'insight')),
    source_id   TEXT NOT NULL CHECK (length(source_id) = 36),
    PRIMARY KEY (insight_id, source_kind, source_id),
    FOREIGN KEY (tenant, insight_id) REFERENCES insights (tenant, id) ON DELETE CASCADE
);
CREATE INDEX insight_sources_source_idx ON insight_sources (tenant, source_kind, source_id);

CREATE TABLE reflect_declines (
    tenant      TEXT NOT NULL,
    scope       TEXT NOT NULL,
    topic_key   TEXT NOT NULL CHECK (length(topic_key) = 36),
    fact_set    TEXT NOT NULL CHECK (length(fact_set) = 64 AND fact_set NOT GLOB '*[^0-9a-f]*'),
    declined_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, scope, topic_key)
);
