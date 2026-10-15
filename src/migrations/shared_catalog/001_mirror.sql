CREATE TABLE IF NOT EXISTS shared_bug_sync (
    bug_id INTEGER PRIMARY KEY REFERENCES bugs(id) ON DELETE CASCADE,
    origin TEXT NOT NULL,
    local_version INTEGER NOT NULL DEFAULT 0,
    dirty INTEGER NOT NULL DEFAULT 1,
    base_json TEXT
);
CREATE INDEX IF NOT EXISTS shared_bug_sync_dirty ON shared_bug_sync(dirty,bug_id);
CREATE TABLE IF NOT EXISTS shared_bug_events (
    enrichment_id INTEGER PRIMARY KEY REFERENCES bug_enrichments(id) ON DELETE CASCADE,
    event_key TEXT NOT NULL UNIQUE
);

DROP TRIGGER IF EXISTS shared_bugs_insert;
CREATE TRIGGER shared_bugs_insert
AFTER INSERT ON bugs
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bugs_update;
CREATE TRIGGER shared_bugs_update
AFTER UPDATE OF bugid, title, lifecycle_status, pipeline_state, assignee, assigned_at, reporter, reported_at, discovered_in_commit, source_ref, created_at, severity_int, verified_on_sha, introducing_commit_sha, fixed_in_commit_sha ON bugs
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
    AND (old.bugid IS NOT new.bugid
        OR old.title IS NOT new.title
        OR old.lifecycle_status IS NOT new.lifecycle_status
        OR old.pipeline_state IS NOT new.pipeline_state
        OR old.assignee IS NOT new.assignee
        OR old.assigned_at IS NOT new.assigned_at
        OR old.reporter IS NOT new.reporter
        OR old.reported_at IS NOT new.reported_at
        OR old.discovered_in_commit IS NOT new.discovered_in_commit
        OR old.source_ref IS NOT new.source_ref
        OR old.created_at IS NOT new.created_at
        OR old.severity_int IS NOT new.severity_int
        OR old.verified_on_sha IS NOT new.verified_on_sha
        OR old.introducing_commit_sha IS NOT new.introducing_commit_sha
        OR old.fixed_in_commit_sha IS NOT new.fixed_in_commit_sha)
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_enrichments_insert;
CREATE TRIGGER shared_bug_enrichments_insert
AFTER INSERT ON bug_enrichments
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_enrichments_update;
CREATE TRIGGER shared_bug_enrichments_update
AFTER UPDATE ON bug_enrichments
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_subsystems_insert;
CREATE TRIGGER shared_bug_subsystems_insert
AFTER INSERT ON bug_subsystems
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_subsystems_update;
CREATE TRIGGER shared_bug_subsystems_update
AFTER UPDATE ON bug_subsystems
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_subsystems_delete;
CREATE TRIGGER shared_bug_subsystems_delete
AFTER DELETE ON bug_subsystems
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (old.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_vectors_insert;
CREATE TRIGGER shared_bug_vectors_insert
AFTER INSERT ON bug_vectors
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_vectors_update;
CREATE TRIGGER shared_bug_vectors_update
AFTER UPDATE ON bug_vectors
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

DROP TRIGGER IF EXISTS shared_bug_vectors_delete;
CREATE TRIGGER shared_bug_vectors_delete
AFTER DELETE ON bug_vectors
FOR EACH ROW WHEN EXISTS (SELECT 1 FROM meta WHERE key='shared_enabled' AND value='1')
    AND NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (old.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;
