-- The shared catalog has no foreign keys into a daemon's libSQL database.
-- Every external reference carries the producing daemon's stable instance ID.
CREATE TABLE IF NOT EXISTS bugs (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    bugid TEXT NOT NULL UNIQUE,
    project TEXT NOT NULL CHECK (project IN ('linux', 'sashiko')),
    title TEXT NOT NULL,
    lifecycle_status TEXT NOT NULL DEFAULT 'new'
        CHECK (lifecycle_status IN
            ('new', 'open', 'fixed', 'dismissed', 'duplicate', 'closed')),
    pipeline_state TEXT NOT NULL DEFAULT 'pending'
        CHECK (pipeline_state IN
            ('pending', 'running', 'succeeded', 'failed', 'abandoned')),
    reporter TEXT NOT NULL,
    reported_at BIGINT NOT NULL,
    assignee TEXT,
    assigned_at BIGINT,
    discovered_by_instance TEXT,
    discovered_in_patchset_ref TEXT,
    discovered_in_patch_ref TEXT,
    discovered_in_commit TEXT,
    source_ref TEXT,
    duplicate_of_id BIGINT REFERENCES bugs(id) ON DELETE RESTRICT,
    locked_by TEXT,
    lease_expires_at BIGINT,
    attempt_count BIGINT NOT NULL DEFAULT 0,
    last_error TEXT,
    severity_int INTEGER NOT NULL DEFAULT 0 CHECK (severity_int BETWEEN 0 AND 4),
    verified_on_sha TEXT,
    introducing_commit_sha TEXT,
    fixed_in_commit_sha TEXT,
    audit_author TEXT NOT NULL DEFAULT 'system',
    audit_tool TEXT NOT NULL DEFAULT 'system',
    audit_model TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    CHECK (bugid LIKE project || '-%'),
    CHECK (duplicate_of_id IS NULL OR duplicate_of_id != id),
    CHECK ((lifecycle_status = 'duplicate') = (duplicate_of_id IS NOT NULL)),
    CHECK (assignee IS NULL OR length(trim(assignee)) > 0),
    CHECK (assignee IS NOT NULL OR assigned_at IS NULL)
);

CREATE INDEX IF NOT EXISTS idx_pg_bugs_lifecycle
    ON bugs(project, lifecycle_status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_pg_bugs_pipeline
    ON bugs(pipeline_state, created_at);
CREATE INDEX IF NOT EXISTS idx_pg_bugs_assignee
    ON bugs(assignee, lifecycle_status);
CREATE INDEX IF NOT EXISTS idx_pg_bugs_severity
    ON bugs(project, severity_int DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_pg_bugs_lease
    ON bugs(pipeline_state, lease_expires_at);

CREATE TABLE IF NOT EXISTS bug_enrichments (
    id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    bug_id BIGINT NOT NULL REFERENCES bugs(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    tool TEXT NOT NULL,
    model TEXT,
    author TEXT,
    created_at BIGINT NOT NULL,
    content TEXT,
    data_json JSONB,
    tokens_in BIGINT,
    tokens_out BIGINT,
    tokens_cached BIGINT,
    logs TEXT
);

CREATE INDEX IF NOT EXISTS idx_pg_bug_enrichments_bug_id
    ON bug_enrichments(bug_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_pg_bug_enrichments_kind
    ON bug_enrichments(kind, bug_id);

CREATE TABLE IF NOT EXISTS bug_subsystems (
    bug_id BIGINT NOT NULL REFERENCES bugs(id) ON DELETE CASCADE,
    subsystem TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'caller_supplied'
        CHECK (source IN ('maintainers_section', 'path_prefix', 'caller_supplied')),
    PRIMARY KEY (bug_id, subsystem)
);

CREATE INDEX IF NOT EXISTS idx_pg_bug_subsystems_subsystem
    ON bug_subsystems(subsystem, bug_id);
CREATE INDEX IF NOT EXISTS idx_pg_bug_subsystems_authorizing
    ON bug_subsystems(bug_id, subsystem)
    WHERE source = 'maintainers_section';

CREATE TABLE IF NOT EXISTS bug_vectors (
    bug_id BIGINT NOT NULL REFERENCES bugs(id) ON DELETE CASCADE,
    model TEXT NOT NULL DEFAULT '',
    vector_json TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (bug_id, model)
);

CREATE TABLE IF NOT EXISTS bug_discoveries (
    instance_id TEXT NOT NULL,
    review_ref TEXT NOT NULL,
    bug_id BIGINT NOT NULL REFERENCES bugs(id) ON DELETE CASCADE,
    is_newly_discovered BOOLEAN NOT NULL DEFAULT TRUE,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (instance_id, review_ref, bug_id)
);

CREATE INDEX IF NOT EXISTS idx_pg_bug_discoveries_bug
    ON bug_discoveries(bug_id);
