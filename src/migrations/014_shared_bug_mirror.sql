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

CREATE TRIGGER IF NOT EXISTS shared_bugs_insert
AFTER INSERT ON bugs
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bugs_update
AFTER UPDATE ON bugs
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_enrichments_insert
AFTER INSERT ON bug_enrichments
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_enrichments_update
AFTER UPDATE ON bug_enrichments
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_subsystems_insert
AFTER INSERT ON bug_subsystems
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_subsystems_update
AFTER UPDATE ON bug_subsystems
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_subsystems_delete
AFTER DELETE ON bug_subsystems
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (old.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_vectors_insert
AFTER INSERT ON bug_vectors
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_vectors_update
AFTER UPDATE ON bug_vectors
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (new.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;

CREATE TRIGGER IF NOT EXISTS shared_bug_vectors_delete
AFTER DELETE ON bug_vectors
FOR EACH ROW WHEN NOT EXISTS (SELECT 1 FROM meta WHERE key='shared_importing' AND value='1')
BEGIN
    INSERT INTO shared_bug_sync (bug_id,origin,local_version,dirty)
    VALUES (old.bug_id,(SELECT value FROM meta WHERE key='shared_instance'),1,1)
    ON CONFLICT(bug_id) DO UPDATE SET local_version=local_version+1,dirty=1;
END;
