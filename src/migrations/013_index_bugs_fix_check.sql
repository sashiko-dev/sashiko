CREATE INDEX IF NOT EXISTS idx_bugs_fix_check
    ON bugs(lifecycle_status, pipeline_state, updated_at ASC, id ASC);
