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

-- Persist structured review feedback on external Forge pull/merge requests
-- (PR review summaries, inline diff comments, and top-level PR issue comments)
-- so prior-version context is transport-agnostic across mailing lists and forges.
CREATE TABLE IF NOT EXISTS forge_comments (
    id INTEGER PRIMARY KEY,
    provider TEXT NOT NULL,
    repo TEXT NOT NULL,
    pr_number INTEGER NOT NULL,
    comment_source TEXT NOT NULL,
    remote_comment_id INTEGER NOT NULL,
    in_reply_to_id INTEGER,
    author TEXT NOT NULL,
    author_association TEXT,
    commit_id TEXT,
    file_path TEXT,
    line INTEGER,
    body TEXT NOT NULL,
    html_url TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    UNIQUE(provider, repo, comment_source, remote_comment_id)
);

CREATE INDEX IF NOT EXISTS idx_forge_comments_repo_pr_created
    ON forge_comments(provider, repo, pr_number, created_at ASC);

CREATE INDEX IF NOT EXISTS idx_forge_comments_commit_id
    ON forge_comments(commit_id) WHERE commit_id IS NOT NULL;

-- Persist per-item prior-version feedback audit verdicts (`PF*` / `HM*`)
-- with explicit origin, author, resolution verdict, and whether Sashiko's
-- fresh analysis stages also found the issue, enabling direct SQL queries
-- for human-reported issues missed by Sashiko.
CREATE TABLE IF NOT EXISTS prior_feedback_resolutions (
    id INTEGER PRIMARY KEY,
    review_id INTEGER NOT NULL REFERENCES reviews(id) ON DELETE CASCADE,
    patchset_id INTEGER NOT NULL REFERENCES patchsets(id) ON DELETE CASCADE,
    patch_id INTEGER REFERENCES patches(id) ON DELETE CASCADE,
    prior_id TEXT NOT NULL,
    origin_kind TEXT NOT NULL,
    version INTEGER,
    author TEXT,
    message_id TEXT,
    reference_url TEXT,
    verdict TEXT NOT NULL,
    summary TEXT,
    reasoning TEXT,
    quote TEXT,
    also_found_by_sashiko INTEGER NOT NULL DEFAULT 0,
    v_n_outcome TEXT
);

CREATE INDEX IF NOT EXISTS idx_prior_feedback_resolutions_review_id
    ON prior_feedback_resolutions(review_id);

CREATE INDEX IF NOT EXISTS idx_prior_feedback_resolutions_patchset_id
    ON prior_feedback_resolutions(patchset_id);

CREATE INDEX IF NOT EXISTS idx_prior_feedback_resolutions_patch_id
    ON prior_feedback_resolutions(patch_id) WHERE patch_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_prior_feedback_resolutions_lookup
    ON prior_feedback_resolutions(origin_kind, verdict, also_found_by_sashiko);
