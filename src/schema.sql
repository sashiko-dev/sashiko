DROP TABLE IF EXISTS patches;
DROP TABLE IF EXISTS reviews;
DROP TABLE IF EXISTS comments;
DROP TABLE IF EXISTS patchsets;
DROP TABLE IF EXISTS messages;
DROP TABLE IF EXISTS threads;
-- We can keep mailing_lists, baselines, ai_interactions if they haven't changed much, but safer to drop all if we are redefining relationships.
DROP TABLE IF EXISTS mailing_lists;
DROP TABLE IF EXISTS baselines;
DROP TABLE IF EXISTS ai_interactions;

CREATE TABLE IF NOT EXISTS mailing_lists (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    nntp_group TEXT NOT NULL UNIQUE,
    last_article_num INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS threads (
    id INTEGER PRIMARY KEY,
    root_message_id TEXT,
    subject TEXT,
    last_updated INTEGER
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    message_id TEXT NOT NULL UNIQUE,
    thread_id INTEGER,
    in_reply_to TEXT,
    author TEXT,
    subject TEXT,
    date INTEGER,
    body TEXT,
    FOREIGN KEY(thread_id) REFERENCES threads(id)
);

CREATE TABLE IF NOT EXISTS baselines (
    id INTEGER PRIMARY KEY,
    repo_url TEXT,
    branch TEXT,
    last_known_commit TEXT
);

CREATE TABLE IF NOT EXISTS patchsets (
    id INTEGER PRIMARY KEY,
    thread_id INTEGER,
    cover_letter_message_id TEXT,
    subject TEXT,
    author TEXT,
    date INTEGER,
    status TEXT DEFAULT 'Pending', -- Pending, Assembled, Applied, Failed, Reviewed
    total_parts INTEGER,
    received_parts INTEGER,
    subject_index INTEGER DEFAULT 9999,
    baseline_id INTEGER,
    parser_version INTEGER DEFAULT 0,
    to_recipients TEXT,
    cc_recipients TEXT,
    FOREIGN KEY(thread_id) REFERENCES threads(id),
    FOREIGN KEY(cover_letter_message_id) REFERENCES messages(message_id),
    FOREIGN KEY(baseline_id) REFERENCES baselines(id)
);

CREATE INDEX IF NOT EXISTS idx_patchsets_status ON patchsets(status);

CREATE TABLE IF NOT EXISTS patches (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    message_id TEXT NOT NULL UNIQUE,
    part_index INTEGER,
    diff TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(message_id) REFERENCES messages(message_id)
);

CREATE TABLE IF NOT EXISTS reviews (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    model_name TEXT,
    summary TEXT,
    created_at INTEGER,
    interaction_id TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(interaction_id) REFERENCES ai_interactions(id)
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
