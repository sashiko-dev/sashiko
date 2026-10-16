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
