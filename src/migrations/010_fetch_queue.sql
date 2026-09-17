-- Durable queue for outstanding git fetches. Replaces the previous in-memory
-- FetchAgent channel so that fetch requests survive restarts, request
-- cancellation, and worker crashes. Modeled on patchwork_outbox: a row is
-- claimed under a lease (locked_at), retried with backoff (next_retry_at), and
-- reclaimed by a ghost sweep if its worker dies mid-flight.

ALTER TABLE patchsets ADD COLUMN repo_url TEXT;

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
