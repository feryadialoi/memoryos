-- Insights (docs/SPEC.md section 8, Insights; ADR 0008): what several facts mean together, written
-- by the agent, with every source kept so a conclusion is traceable and goes stale when its
-- evidence changes.

CREATE TABLE insights (
    id            uuid PRIMARY KEY,
    tenant        text NOT NULL,
    scope         text NOT NULL,
    level         text NOT NULL CHECK (level IN ('context', 'understanding', 'guidance')),
    statement     text NOT NULL CHECK (char_length(statement) BETWEEN 1 AND 1000),
    trust         text NOT NULL CHECK (trust IN ('external', 'agent', 'user', 'system')),
    status        text NOT NULL
        CHECK (status IN ('active', 'stale', 'quarantined', 'rejected', 'retired', 'superseded')),
    -- Which sources changed and how, while status is 'stale'.
    stale_reason  jsonb,
    valid_from    timestamptz NOT NULL,
    valid_to      timestamptz,
    superseded_by uuid,
    -- Set on a quarantined insight: the insight approval will close.
    pending_supersede_of uuid,
    written_by    text NOT NULL,
    recorded_at   timestamptz NOT NULL DEFAULT now(),
    embedding     vector(384) NOT NULL,
    statement_tsv tsvector GENERATED ALWAYS AS (to_tsvector('simple', statement)) STORED,
    CHECK ((status = 'stale') = (stale_reason IS NOT NULL))
);
CREATE INDEX insights_scope_idx ON insights (tenant, scope, status, level);
CREATE INDEX insights_tsv_idx ON insights USING gin (statement_tsv);
CREATE INDEX insights_embedding_idx ON insights USING hnsw (embedding vector_cosine_ops);
CREATE INDEX insights_quarantined_idx ON insights (tenant, recorded_at) WHERE status = 'quarantined';

-- The evidence. A source is a fact or a lower-level insight of the same scope. No foreign key to
-- facts: a purged fact leaves its row here, which is how the insight reports the source as purged.
CREATE TABLE insight_sources (
    insight_id  uuid NOT NULL REFERENCES insights (id) ON DELETE CASCADE,
    tenant      text NOT NULL,
    source_kind text NOT NULL CHECK (source_kind IN ('fact', 'insight')),
    source_id   uuid NOT NULL,
    PRIMARY KEY (insight_id, source_kind, source_id)
);
-- Finds the insights to mark stale when a source changes.
CREATE INDEX insight_sources_source_idx ON insight_sources (tenant, source_kind, source_id);

-- Topics the agent chose not to synthesize, until their fact set changes.
CREATE TABLE reflect_declines (
    tenant      text NOT NULL,
    scope       text NOT NULL,
    topic_key   uuid NOT NULL,
    fact_set    text NOT NULL CHECK (fact_set ~ '^[0-9a-f]{64}$'),
    declined_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant, scope, topic_key)
);

DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['insights', 'insight_sources', 'reflect_declines']
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
-- function runs as the owner, takes the tenant from app.tenant, and returns ids of active insights
-- only. FORCE is off on the table it reads so a non-superuser owner can run it.
ALTER TABLE insights NO FORCE ROW LEVEL SECURITY;

CREATE FUNCTION search_insight_ids_text(p_scopes text[], p_query text, p_limit bigint)
RETURNS TABLE (id uuid, rank real)
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = public
AS $$
DECLARE
    q tsquery := websearch_to_tsquery('simple', p_query);
BEGIN
    RETURN QUERY EXECUTE $sql$
        SELECT c.id, ts_rank(c.statement_tsv, $1) AS rank
        FROM (
            SELECT i.id, i.statement_tsv FROM insights i
            WHERE i.tenant = current_setting('app.tenant', true)
              AND i.scope = ANY ($2)
              AND i.status = 'active'
              AND i.statement_tsv @@ $1
            ORDER BY i.id DESC
            LIMIT GREATEST(1000, $3)
        ) c
        ORDER BY rank DESC, c.id
        LIMIT $3
    $sql$ USING q, p_scopes, p_limit;
END;
$$;

REVOKE ALL ON FUNCTION search_insight_ids_text(text[], text, bigint) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION search_insight_ids_text(text[], text, bigint) TO memoryos_app;
