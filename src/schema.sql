-- Copyright 2026 The Sashiko Authors
--
-- Licensed under the Apache License, Version 2.0 (the "License");
-- you may not use this file except in compliance with the License.
-- You may obtain a copy of the License at
--
--     https://www.apache.org/licenses/LICENSE-2.0
--
-- Unless required by applicable law or agreed to in writing, software
-- distributed under the License is distributed on an "AS IS" BASIS,
-- WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
-- See the License for the specific language governing permissions and
-- limitations under the License.

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
    status TEXT DEFAULT 'Incomplete', -- Incomplete, Pending, In Review, Cancelled, Reviewed, Failed
    total_parts INTEGER,
    received_parts INTEGER,
    subject_index INTEGER DEFAULT 9999,
    parser_version INTEGER DEFAULT 0,
    to_recipients TEXT,
    cc_recipients TEXT,
    baseline_id INTEGER,
    model_name TEXT,
    prompts_git_hash TEXT,
    baseline_logs TEXT,
    failed_reason TEXT,
    skip_filters TEXT,
    only_filters TEXT,
    target_review_count INTEGER DEFAULT 1,
    provider TEXT,
    embargo_until INTEGER,
    summary TEXT,
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
    patch_id INTEGER, -- Optional link to specific patch
    summary TEXT,
    result_description TEXT,
    created_at INTEGER,
    interaction_id TEXT,
    status TEXT DEFAULT 'Pending', -- Pending, In Review, Cancelled, Reviewed, Failed
    logs TEXT,
    inline_review TEXT,
    baseline_id INTEGER,
    model TEXT,
    prompts_hash TEXT,
    provider TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(patch_id) REFERENCES patches(id),
    FOREIGN KEY(interaction_id) REFERENCES ai_interactions(id),
    FOREIGN KEY(baseline_id) REFERENCES baselines(id)
);

CREATE TABLE IF NOT EXISTS findings (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL,
    severity INTEGER NOT NULL, -- 1: Low, 2: Medium, 3: High, 4: Critical
    severity_explanation TEXT,
    problem TEXT,
    suggestion TEXT,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_findings_review_id ON findings(review_id);
CREATE INDEX IF NOT EXISTS idx_findings_severity ON findings(severity);

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

CREATE INDEX IF NOT EXISTS idx_patchsets_cover_message_id ON patchsets(cover_letter_message_id);

CREATE INDEX IF NOT EXISTS idx_messages_thread_id ON messages(thread_id);
CREATE INDEX IF NOT EXISTS idx_patches_patchset_id ON patches(patchset_id);
CREATE INDEX IF NOT EXISTS idx_messages_date ON messages(date);

CREATE INDEX IF NOT EXISTS idx_messages_day ON messages(strftime('%Y-%m-%d', date, 'unixepoch'));
CREATE INDEX IF NOT EXISTS idx_patchsets_day ON patchsets(strftime('%Y-%m-%d', date, 'unixepoch'));
CREATE INDEX IF NOT EXISTS idx_messages_subsystems_sid ON messages_subsystems(subsystem_id);
CREATE INDEX IF NOT EXISTS idx_patchsets_subsystems_sid ON patchsets_subsystems(subsystem_id);

CREATE TABLE IF NOT EXISTS people (
    id INTEGER PRIMARY KEY,
    name TEXT,
    email TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS messages_recipients (
    message_id INTEGER NOT NULL,
    person_id INTEGER NOT NULL,
    recipient_type TEXT NOT NULL, -- 'To', 'Cc'
    PRIMARY KEY (message_id, person_id),
    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
    FOREIGN KEY(person_id) REFERENCES people(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS messages_mailing_lists (
    message_id INTEGER NOT NULL,
    mailing_list_id INTEGER NOT NULL,
    PRIMARY KEY (message_id, mailing_list_id),
    FOREIGN KEY(message_id) REFERENCES messages(id) ON DELETE CASCADE,
    FOREIGN KEY(mailing_list_id) REFERENCES mailing_lists(id) ON DELETE CASCADE
);

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

CREATE TABLE IF NOT EXISTS email_outbox (
    id INTEGER PRIMARY KEY,
    patch_id INTEGER,
    status TEXT DEFAULT 'Pending',
    to_addresses TEXT,
    cc_addresses TEXT,
    subject TEXT,
    in_reply_to TEXT,
    references_hdr TEXT,
    body TEXT,
    locked_at INTEGER,
    error_log TEXT,
    created_at INTEGER,
    FOREIGN KEY(patch_id) REFERENCES patches(id)
);
CREATE INDEX IF NOT EXISTS idx_email_outbox_status ON email_outbox(status);

CREATE INDEX IF NOT EXISTS idx_ai_interactions_tokens ON ai_interactions(id, tokens_in, tokens_out, tokens_cached);
CREATE INDEX IF NOT EXISTS idx_reviews_grouping ON reviews(provider, model, status, interaction_id);
CREATE INDEX IF NOT EXISTS idx_tool_usages_stats ON tool_usages(provider, model, tool_name, output_length);

