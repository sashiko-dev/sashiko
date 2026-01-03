CREATE TABLE IF NOT EXISTS mailing_lists (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    nntp_group TEXT NOT NULL UNIQUE,
    last_article_num INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS patchsets (
    id INTEGER PRIMARY KEY,
    message_id TEXT NOT NULL UNIQUE,
    subject TEXT,
    author TEXT,
    date INTEGER,
    total_parts INTEGER,
    received_parts INTEGER,
    status TEXT DEFAULT 'Pending' -- Pending, Assembled, Applied, Failed, Reviewed
);

CREATE INDEX IF NOT EXISTS idx_patchsets_date ON patchsets(date);

CREATE TABLE IF NOT EXISTS patches (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    part_index INTEGER,
    body TEXT,
    diff TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id)
);

CREATE TABLE IF NOT EXISTS baselines (
    id INTEGER PRIMARY KEY,
    repo_url TEXT NOT NULL,
    branch TEXT,
    last_known_commit TEXT
);

CREATE TABLE IF NOT EXISTS reviews (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    model_name TEXT,
    summary TEXT,
    created_at INTEGER,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id)
);

CREATE TABLE IF NOT EXISTS comments (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    file_path TEXT,
    line_number INTEGER,
    content TEXT,
    severity TEXT, -- Info, Warning, Error
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);

CREATE TABLE IF NOT EXISTS ai_interactions (
    id TEXT PRIMARY KEY,
    parent_interaction_id TEXT,
    workflow_id TEXT,
    provider TEXT,
    model TEXT,
    input_context TEXT,
    output_raw TEXT,
    tokens_in INTEGER,
    tokens_out INTEGER,
    created_at INTEGER
);
