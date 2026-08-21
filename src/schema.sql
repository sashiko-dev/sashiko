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
    references_hdr TEXT,
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
    baseline_part_index INTEGER, -- part that supplied baseline_id; NULL if unknown
    model_name TEXT,
    mr_url TEXT,
    mr_title TEXT,
    mr_number INTEGER,
    prompts_git_hash TEXT,
    baseline_logs TEXT,
    failed_reason TEXT,
    skip_filters TEXT,
    only_filters TEXT,
    target_review_count INTEGER DEFAULT 1,
    provider TEXT,
    embargo_until INTEGER,
    embargo_release_started_at INTEGER,
    slug TEXT, -- URL-friendly slug like "reponame-725" (repo-mrnum)
    base_priority INTEGER DEFAULT 500,
    priority_cap INTEGER,
    priority INTEGER DEFAULT 500,
    repo_url TEXT, -- fetch source URL, persisted for Fetching placeholder recovery
    review_context TEXT, -- JSON-encoded ReviewKind selecting an alternate review pipeline; NULL = default patch review
    FOREIGN KEY(thread_id) REFERENCES threads(id),
    FOREIGN KEY(cover_letter_message_id) REFERENCES messages(message_id),
    FOREIGN KEY(baseline_id) REFERENCES baselines(id)
);

CREATE INDEX IF NOT EXISTS idx_patchsets_status ON patchsets(status);
CREATE INDEX IF NOT EXISTS idx_patchsets_status_priority_date ON patchsets(status, priority DESC, date ASC);


CREATE TABLE IF NOT EXISTS patches (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    part_index INTEGER,
    diff TEXT,
    status TEXT,
    apply_error TEXT,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id),
    FOREIGN KEY(message_id) REFERENCES messages(message_id),
    UNIQUE(patchset_id, message_id)
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
    preexisting INTEGER, -- 0 = false, 1 = true
    locations TEXT,
    FOREIGN KEY(review_id) REFERENCES reviews(id)
);
CREATE INDEX IF NOT EXISTS idx_findings_review_id ON findings(review_id);
CREATE INDEX IF NOT EXISTS idx_findings_severity ON findings(severity);
CREATE INDEX IF NOT EXISTS idx_findings_review_severity ON findings(review_id, severity, preexisting);

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
CREATE INDEX IF NOT EXISTS idx_patches_message_id ON patches(message_id);
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

CREATE INDEX IF NOT EXISTS idx_patchsets_date ON patchsets(date DESC);
CREATE INDEX IF NOT EXISTS idx_reviews_patchset_status ON reviews(patchset_id, status);
CREATE INDEX IF NOT EXISTS idx_reviews_day ON reviews(strftime('%Y-%m-%d', created_at, 'unixepoch'), status);
CREATE INDEX IF NOT EXISTS idx_reviews_created_at ON reviews(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_email_outbox_patch_id ON email_outbox(patch_id);

CREATE TABLE IF NOT EXISTS patchwork_outbox (
    id INTEGER PRIMARY KEY,
    patch_msg_id TEXT NOT NULL,
    api_url TEXT NOT NULL,
    check_state TEXT NOT NULL,
    description TEXT NOT NULL,
    target_url TEXT NOT NULL,
    context TEXT NOT NULL DEFAULT 'sashiko',
    status TEXT DEFAULT 'Pending',
    retry_count INTEGER DEFAULT 0,
    next_retry_at INTEGER,
    locked_at INTEGER,
    error_log TEXT,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_patchwork_outbox_status ON patchwork_outbox(status);

CREATE INDEX IF NOT EXISTS idx_reviews_patch_status ON reviews(patch_id, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_patchsets_slug ON patchsets(slug) WHERE slug IS NOT NULL;

-- Durable queue for outstanding git fetches. Replaces the previous in-memory
-- FetchAgent channel so that fetch requests survive restarts, request
-- cancellation, and worker crashes. Modeled on patchwork_outbox: a row is
-- claimed under a lease (locked_at), retried with backoff (next_retry_at), and
-- reclaimed by a ghost sweep if its worker dies mid-flight.
CREATE TABLE IF NOT EXISTS fetch_queue (
    id INTEGER PRIMARY KEY,
    patchset_id INTEGER,
    cover_letter_message_id TEXT,
    repo_url TEXT,
    commit_hash TEXT NOT NULL,
    mr_url TEXT,
    mr_title TEXT,
    mr_number INTEGER,
    status TEXT NOT NULL DEFAULT 'Pending' CHECK(status IN ('Pending', 'Fetching', 'Failed')),
    attempts INTEGER DEFAULT 0,
    first_attempt_at INTEGER,
    next_retry_at INTEGER,
    locked_at INTEGER,
    last_error TEXT,
    priority INTEGER DEFAULT 500,
    created_at INTEGER NOT NULL,
    FOREIGN KEY(patchset_id) REFERENCES patchsets(id)
);
CREATE INDEX IF NOT EXISTS idx_fetch_queue_claim
    ON fetch_queue(status, next_retry_at, priority, created_at);
CREATE INDEX IF NOT EXISTS idx_fetch_queue_commit ON fetch_queue(commit_hash);

-- Supporting commits for a fetch (e.g. a cherry-pick's original and base
-- commits). These are ensured present locally so a review can hydrate its
-- context from git, but are never ingested as patches. One row per commit so
-- an arbitrary number can be stored.
CREATE TABLE IF NOT EXISTS fetch_supporting_commits (
    id INTEGER PRIMARY KEY,
    fetch_id INTEGER NOT NULL,
    commit_hash TEXT NOT NULL,
    FOREIGN KEY(fetch_id) REFERENCES fetch_queue(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_fetch_supporting_fetch_id
    ON fetch_supporting_commits(fetch_id);
