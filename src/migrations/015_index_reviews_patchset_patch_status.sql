CREATE INDEX IF NOT EXISTS idx_reviews_patchset_patch_status
    ON reviews(patchset_id, patch_id, status);
