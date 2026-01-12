CREATE TABLE IF NOT EXISTS mailing_lists (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    nntp_group TEXT NOT NULL UNIQUE,
    last_article_num INTEGER DEFAULT 0
);

CREATE TABLE IF NOT EXISTS subsystems (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    mailing_list_address TEXT NOT NULL UNIQUE
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
    to_recipients TEXT,
    cc_recipients TEXT,
    git_blob_hash TEXT,
    mailing_list TEXT,
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
    status TEXT DEFAULT 'Incomplete', -- Incomplete, Pending, Applying, In Review, Cancelled, Reviewed, Failed
    total_parts INTEGER,
    received_parts INTEGER,
    subject_index INTEGER DEFAULT 9999,
    parser_version INTEGER DEFAULT 0,
    to_recipients TEXT,
    cc_recipients TEXT,
    FOREIGN KEY(thread_id) REFERENCES threads(id),
    FOREIGN KEY(cover_letter_message_id) REFERENCES messages(message_id)
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
    patch_id INTEGER, -- Optional link to specific patch
    model_name TEXT,
    provider TEXT,
    prompts_git_hash TEXT,
    summary TEXT,
    result_description TEXT,
    created_at INTEGER,
    interaction_id TEXT,
    baseline_id INTEGER,
    status TEXT DEFAULT 'Pending', -- Pending, Applying, In Review, Cancelled, Reviewed, Failed
    logs TEXT,
    inline_review TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(patch_id) REFERENCES patches(id),
    FOREIGN KEY(interaction_id) REFERENCES ai_interactions(id),
    FOREIGN KEY(baseline_id) REFERENCES baselines(id)
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
    tokens_cached INTEGER,
    created_at INTEGER
);

CREATE TABLE IF NOT EXISTS messages_subsystems (
    message_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (message_id, subsystem_id),
    FOREIGN KEY(message_id) REFERENCES messages(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE TABLE IF NOT EXISTS threads_subsystems (
    thread_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (thread_id, subsystem_id),
    FOREIGN KEY(thread_id) REFERENCES threads(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE TABLE IF NOT EXISTS patches_subsystems (
    patch_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (patch_id, subsystem_id),
    FOREIGN KEY(patch_id) REFERENCES patches(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE TABLE IF NOT EXISTS patchsets_subsystems (
    patchset_id INTEGER NOT NULL,
    subsystem_id INTEGER NOT NULL,
    PRIMARY KEY (patchset_id, subsystem_id),
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(subsystem_id) REFERENCES subsystems(id)
);

CREATE TABLE IF NOT EXISTS tags (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS messages_tags (
    message_id INTEGER NOT NULL,
    tag_id INTEGER NOT NULL,
    PRIMARY KEY (message_id, tag_id),
    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
    FOREIGN KEY(tag_id) REFERENCES tags(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS threads_tags (
    thread_id INTEGER NOT NULL,
    tag_id INTEGER NOT NULL,
    PRIMARY KEY (thread_id, tag_id),
    FOREIGN KEY(thread_id) REFERENCES threads(id) ON DELETE CASCADE,
    FOREIGN KEY(tag_id) REFERENCES tags(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS patches_tags (
    patch_id INTEGER NOT NULL,
    tag_id INTEGER NOT NULL,
    PRIMARY KEY (patch_id, tag_id),
    FOREIGN KEY(patch_id) REFERENCES patches(id) ON DELETE CASCADE,
    FOREIGN KEY(tag_id) REFERENCES tags(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS patchsets_tags (
    patchset_id INTEGER NOT NULL,
    tag_id INTEGER NOT NULL,
    PRIMARY KEY (patchset_id, tag_id),
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id) ON DELETE CASCADE,
    FOREIGN KEY(tag_id) REFERENCES tags(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_patchsets_cover_message_id ON patchsets(cover_letter_message_id);

CREATE TABLE IF NOT EXISTS tool_usages (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    provider TEXT,
    model TEXT,
    tool_name TEXT,
    arguments TEXT,
    output_length INTEGER,
    created_at INTEGER,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_tool_usages_review ON tool_usages(review_id);
