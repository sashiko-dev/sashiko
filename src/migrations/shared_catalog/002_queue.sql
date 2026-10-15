ALTER TABLE shared_bug_sync ADD COLUMN queue_order INTEGER NOT NULL DEFAULT 0;
CREATE INDEX shared_bug_sync_queue ON shared_bug_sync(dirty,queue_order,bug_id);
