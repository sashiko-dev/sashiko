CREATE TABLE IF NOT EXISTS fetch_queue (
    id INTEGER PRIMARY KEY,
    repo_url TEXT NOT NULL DEFAULT '',
    commit_range TEXT NOT NULL,
    article_id TEXT NOT NULL,
    mr_url TEXT,
    mr_title TEXT,
    mr_number INTEGER,
    status TEXT NOT NULL DEFAULT 'Pending',
    locked_at INTEGER,
    retry_count INTEGER NOT NULL DEFAULT 0,
    next_retry_at INTEGER,
    error_log TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE(repo_url, article_id)
);

CREATE INDEX IF NOT EXISTS idx_fetch_queue_status
    ON fetch_queue(status, next_retry_at);

CREATE INDEX IF NOT EXISTS idx_fetch_queue_article_id
    ON fetch_queue(article_id, status);
