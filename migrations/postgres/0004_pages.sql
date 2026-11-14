-- Knowledge pages (docs/SPEC.md section 8, ADR 0007). A page is identified by scope and slug and
-- holds versions. Exactly one version, `current_version`, is what readers and search see; others are
-- superseded, quarantined (waiting for review) or rejected. Sections are kept for the current
-- version and for quarantined versions, so approval needs no embedder; search joins on
-- `current_version` so a pending version's sections are never returned.

CREATE TABLE pages (
    id              uuid PRIMARY KEY,
    tenant          text NOT NULL,
    scope           text NOT NULL,
    slug            text NOT NULL CHECK (slug ~ '^[a-z0-9][a-z0-9._-]{0,127}$'),
    current_version bigint,
    created_at      timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant, scope, slug)
);

CREATE TABLE page_versions (
    page_id      uuid NOT NULL REFERENCES pages (id) ON DELETE CASCADE,
    tenant       text NOT NULL,
    version      bigint NOT NULL CHECK (version > 0),
    base_version bigint,
    title        text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 200),
    body         text NOT NULL CHECK (char_length(body) <= 100000),
    summary      text CHECK (summary IS NULL OR char_length(summary) <= 500),
    trust        text NOT NULL CHECK (trust IN ('external', 'agent', 'user', 'system')),
    status       text NOT NULL CHECK (status IN ('active', 'quarantined', 'rejected', 'superseded')),
    edited_by    text NOT NULL,
    edited_at    timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (page_id, version)
);
CREATE INDEX page_versions_quarantined_idx ON page_versions (tenant, edited_at)
    WHERE status = 'quarantined';
-- At most one active version per page, so `current_version` and `status` cannot disagree twice.
CREATE UNIQUE INDEX page_versions_one_active_idx ON page_versions (page_id) WHERE status = 'active';

CREATE TABLE page_sections (
    id        uuid PRIMARY KEY,
    tenant    text NOT NULL,
    page_id   uuid NOT NULL,
    version   bigint NOT NULL,
    scope     text NOT NULL,
    ordinal   integer NOT NULL CHECK (ordinal >= 0),
    heading   text NOT NULL,
    text      text NOT NULL CHECK (char_length(text) <= 1000),
    embedding vector(384) NOT NULL,
    text_tsv  tsvector GENERATED ALWAYS AS (to_tsvector('simple', heading || ' ' || text)) STORED,
    FOREIGN KEY (page_id, version) REFERENCES page_versions (page_id, version) ON DELETE CASCADE,
    UNIQUE (page_id, version, ordinal)
);
CREATE INDEX page_sections_scope_idx ON page_sections (tenant, scope);
CREATE INDEX page_sections_tsv_idx ON page_sections USING gin (text_tsv);
CREATE INDEX page_sections_embedding_idx ON page_sections USING hnsw (embedding vector_cosine_ops);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['pages', 'page_versions', 'page_sections']
    LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
        EXECUTE format(
            'CREATE POLICY tenant_isolation ON %I TO memoryos_app
             USING (tenant = current_setting(''app.tenant'', true))
             WITH CHECK (tenant = current_setting(''app.tenant'', true))', t);
        EXECUTE format('GRANT SELECT, INSERT, UPDATE, DELETE ON %I TO memoryos_app', t);
    END LOOP;
END
$$;

-- Text search for the same reason as search_fact_ids_text in 0001: GIN is unusable under RLS. The
-- function runs as the owner, takes the tenant from app.tenant, and returns ids only. FORCE is off on
-- both tables it reads so a non-superuser owner can run it.
ALTER TABLE pages NO FORCE ROW LEVEL SECURITY;
ALTER TABLE page_sections NO FORCE ROW LEVEL SECURITY;

CREATE FUNCTION search_page_section_ids_text(p_scopes text[], p_query text, p_limit bigint)
RETURNS TABLE (id uuid, rank real)
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = public
AS $$
DECLARE
    q tsquery := websearch_to_tsquery('simple', p_query);
BEGIN
    RETURN QUERY EXECUTE $sql$
        SELECT c.id, ts_rank(c.text_tsv, $1) AS rank
        FROM (
            SELECT s.id, s.text_tsv FROM page_sections s
            JOIN pages p ON p.id = s.page_id AND p.current_version = s.version
            WHERE s.tenant = current_setting('app.tenant', true)
              AND p.tenant = current_setting('app.tenant', true)
              AND s.scope = ANY ($2)
              AND s.text_tsv @@ $1
            ORDER BY s.id DESC
            LIMIT GREATEST(1000, $3)
        ) c
        ORDER BY rank DESC, c.id
        LIMIT $3
    $sql$ USING q, p_scopes, p_limit;
END;
$$;

REVOKE ALL ON FUNCTION search_page_section_ids_text(text[], text, bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION search_page_section_ids_text(text[], text, bigint) TO memoryos_app;
