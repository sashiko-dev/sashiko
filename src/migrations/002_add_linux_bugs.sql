CREATE TABLE IF NOT EXISTS bugs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bugid TEXT NOT NULL UNIQUE,
    title TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'raw',
    reporter TEXT NOT NULL,
    reported_at INTEGER NOT NULL,
    discovered_in_patchset_id INTEGER,
    discovered_in_patch_id INTEGER,
    discovered_in_commit TEXT,
    source_ref TEXT,
    vector_json TEXT,
    duplicate_of_id INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY(discovered_in_patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(discovered_in_patch_id) REFERENCES patches(id),
    FOREIGN KEY(duplicate_of_id) REFERENCES bugs(id)
);

CREATE INDEX IF NOT EXISTS idx_bugs_bugid ON bugs(bugid);
CREATE INDEX IF NOT EXISTS idx_bugs_status ON bugs(status);
CREATE INDEX IF NOT EXISTS idx_bugs_reporter ON bugs(reporter);
CREATE INDEX IF NOT EXISTS idx_bugs_reported_at ON bugs(reported_at);
CREATE INDEX IF NOT EXISTS idx_bugs_duplicate_of_id ON bugs(duplicate_of_id);

CREATE TABLE IF NOT EXISTS bug_enrichments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    bug_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    tool TEXT NOT NULL,
    model TEXT,
    author TEXT,
    created_at INTEGER NOT NULL,
    content TEXT,
    data_json TEXT,
    tokens_in INTEGER,
    tokens_out INTEGER,
    tokens_cached INTEGER,
    logs TEXT,
    FOREIGN KEY(bug_id) REFERENCES bugs(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_bug_enrichments_bug_id ON bug_enrichments(bug_id, created_at);
CREATE INDEX IF NOT EXISTS idx_bug_enrichments_kind ON bug_enrichments(kind, bug_id);
CREATE INDEX IF NOT EXISTS idx_bug_enrichments_tool ON bug_enrichments(tool);

CREATE TABLE IF NOT EXISTS review_bugs (
    review_id INTEGER NOT NULL,
    bug_id INTEGER NOT NULL,
    is_newly_discovered INTEGER NOT NULL DEFAULT 1, -- 1 = newly discovered, 0 = matched existing
    PRIMARY KEY(review_id, bug_id),
    FOREIGN KEY(review_id) REFERENCES reviews(id),
    FOREIGN KEY(bug_id) REFERENCES bugs(id)
);

CREATE INDEX IF NOT EXISTS idx_review_bugs_review ON review_bugs(review_id);
CREATE INDEX IF NOT EXISTS idx_review_bugs_bug ON review_bugs(bug_id);

CREATE TABLE IF NOT EXISTS bugs_subsystems (
    bug_id INTEGER NOT NULL,
    subsystem TEXT NOT NULL,
    PRIMARY KEY (bug_id, subsystem),
    FOREIGN KEY(bug_id) REFERENCES bugs(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_bugs_subsystems_subsystem ON bugs_subsystems(subsystem, bug_id);
CREATE INDEX IF NOT EXISTS idx_bugs_subsystems_bug_id ON bugs_subsystems(bug_id);


CREATE TRIGGER IF NOT EXISTS trg_bugs_audit_status
AFTER UPDATE OF status ON bugs
FOR EACH ROW
WHEN (old.status != new.status) OR (old.status IS NULL AND new.status IS NOT NULL) OR (old.status IS NOT NULL AND new.status IS NULL)
BEGIN
    INSERT INTO bug_enrichments (
        bug_id, kind, tool, created_at, content, data_json
    ) VALUES (
        new.id, 'audit', 'system', strftime('%s', 'now'),
        'Field "status" changed from "' || substr(IFNULL(CAST(old.status AS TEXT), 'null'), 1, 50) || '" to "' || substr(IFNULL(CAST(new.status AS TEXT), 'null'), 1, 50) || '"',
        json_object('field', 'status', 'old', old.status, 'new', new.status)
    );
END;

CREATE TRIGGER IF NOT EXISTS trg_bugs_audit_title
AFTER UPDATE OF title ON bugs
FOR EACH ROW
WHEN (old.title != new.title) OR (old.title IS NULL AND new.title IS NOT NULL) OR (old.title IS NOT NULL AND new.title IS NULL)
BEGIN
    INSERT INTO bug_enrichments (
        bug_id, kind, tool, created_at, content, data_json
    ) VALUES (
        new.id, 'audit', 'system', strftime('%s', 'now'),
        'Field "title" changed from "' || substr(IFNULL(CAST(old.title AS TEXT), 'null'), 1, 50) || '" to "' || substr(IFNULL(CAST(new.title AS TEXT), 'null'), 1, 50) || '"',
        json_object('field', 'title', 'old', old.title, 'new', new.title)
    );
END;

CREATE TRIGGER IF NOT EXISTS trg_bugs_audit_dup_of_id
AFTER UPDATE OF duplicate_of_id ON bugs
FOR EACH ROW
WHEN (old.duplicate_of_id != new.duplicate_of_id) OR (old.duplicate_of_id IS NULL AND new.duplicate_of_id IS NOT NULL) OR (old.duplicate_of_id IS NOT NULL AND new.duplicate_of_id IS NULL)
BEGIN
    INSERT INTO bug_enrichments (
        bug_id, kind, tool, created_at, content, data_json
    ) VALUES (
        new.id, 'audit', 'system', strftime('%s', 'now'),
        'Field "duplicate_of_id" changed from "' || substr(IFNULL(CAST(old.duplicate_of_id AS TEXT), 'null'), 1, 50) || '" to "' || substr(IFNULL(CAST(new.duplicate_of_id AS TEXT), 'null'), 1, 50) || '"',
        json_object('field', 'duplicate_of_id', 'old', old.duplicate_of_id, 'new', new.duplicate_of_id)
    );
END;
