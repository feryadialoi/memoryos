-- SQLite equivalent of PostgreSQL migration 0004: knowledge pages (docs/SPEC.md section 8,
-- ADR 0007). Conventions follow 0001: UUIDs are canonical lowercase hyphenated TEXT, timestamps are
-- Unix microseconds, tenant-aware foreign keys reject cross-tenant references, and every query
-- carries an explicit tenant predicate.

CREATE TABLE pages (
    id              TEXT PRIMARY KEY CHECK (length(id) = 36),
    tenant          TEXT NOT NULL,
    scope           TEXT NOT NULL,
    slug            TEXT NOT NULL CHECK (
        length(slug) BETWEEN 1 AND 128
        AND slug NOT GLOB '*[^a-z0-9._-]*'
        AND substr(slug, 1, 1) GLOB '[a-z0-9]'
    ),
    current_version INTEGER,
    created_at      INTEGER NOT NULL,
    UNIQUE (tenant, scope, slug),
    UNIQUE (tenant, id)
);

CREATE TABLE page_versions (
    page_id      TEXT NOT NULL,
    tenant       TEXT NOT NULL,
    version      INTEGER NOT NULL CHECK (version > 0),
    base_version INTEGER,
    title        TEXT NOT NULL CHECK (length(title) BETWEEN 1 AND 200),
    body         TEXT NOT NULL CHECK (length(body) <= 100000),
    summary      TEXT CHECK (summary IS NULL OR length(summary) <= 500),
    trust        TEXT NOT NULL CHECK (trust IN ('external', 'agent', 'user', 'system')),
    status       TEXT NOT NULL CHECK (status IN ('active', 'quarantined', 'rejected', 'superseded')),
    edited_by    TEXT NOT NULL,
    edited_at    INTEGER NOT NULL,
    PRIMARY KEY (page_id, version),
    UNIQUE (tenant, page_id, version),
    FOREIGN KEY (tenant, page_id) REFERENCES pages (tenant, id) ON DELETE CASCADE
);
CREATE INDEX page_versions_quarantined_idx ON page_versions (tenant, edited_at)
    WHERE status = 'quarantined';
CREATE UNIQUE INDEX page_versions_one_active_idx ON page_versions (page_id) WHERE status = 'active';

-- row_id is an INTEGER PRIMARY KEY alias so vec0 and FTS5 rows stay attached across VACUUM, as for
-- facts in 0001. Base and vector rows are written or removed in one transaction.
CREATE TABLE page_sections (
    row_id  INTEGER PRIMARY KEY,
    id      TEXT NOT NULL UNIQUE CHECK (length(id) = 36),
    tenant  TEXT NOT NULL,
    page_id TEXT NOT NULL,
    version INTEGER NOT NULL,
    scope   TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    heading TEXT NOT NULL,
    text    TEXT NOT NULL CHECK (length(text) <= 1000),
    UNIQUE (page_id, version, ordinal),
    FOREIGN KEY (tenant, page_id, version)
        REFERENCES page_versions (tenant, page_id, version) ON DELETE CASCADE
);
CREATE INDEX page_sections_scope_idx ON page_sections (tenant, scope);

CREATE VIRTUAL TABLE page_section_embeddings USING vec0(embedding float[384]);

-- Cascading deletes skip vec0, so this trigger removes a section's vector with it.
CREATE TRIGGER page_sections_vec_delete AFTER DELETE ON page_sections BEGIN
    DELETE FROM page_section_embeddings WHERE rowid = old.row_id;
END;

CREATE VIRTUAL TABLE page_sections_fts USING fts5(
    heading,
    text,
    content='page_sections',
    content_rowid='row_id',
    tokenize='unicode61'
);

CREATE TRIGGER page_sections_fts_insert AFTER INSERT ON page_sections BEGIN
    INSERT INTO page_sections_fts (rowid, heading, text) VALUES (new.row_id, new.heading, new.text);
END;
CREATE TRIGGER page_sections_fts_delete AFTER DELETE ON page_sections BEGIN
    INSERT INTO page_sections_fts (page_sections_fts, rowid, heading, text)
        VALUES ('delete', old.row_id, old.heading, old.text);
END;
