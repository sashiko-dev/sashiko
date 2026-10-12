-- Series-level version and lineage columns on patchsets
ALTER TABLE patchsets ADD COLUMN version INTEGER;
ALTER TABLE patchsets ADD COLUMN change_id TEXT;
ALTER TABLE patchsets ADD COLUMN previous_patchset_id INTEGER REFERENCES patchsets(id);
ALTER TABLE patchsets ADD COLUMN version_link_signal TEXT;

CREATE INDEX IF NOT EXISTS idx_patchsets_change_id
    ON patchsets(change_id) WHERE change_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_patchsets_previous_patchset_id
    ON patchsets(previous_patchset_id);

-- Patch-level lineage columns on patches
ALTER TABLE patches ADD COLUMN previous_patch_id INTEGER REFERENCES patches(id);
ALTER TABLE patches ADD COLUMN previous_patch_status TEXT;

CREATE INDEX IF NOT EXISTS idx_patches_previous_patch_id
    ON patches(previous_patch_id);

-- Cover-letter review email outbox linkage
ALTER TABLE email_outbox ADD COLUMN patchset_id INTEGER REFERENCES patchsets(id);
ALTER TABLE email_outbox ADD COLUMN review_id INTEGER REFERENCES reviews(id);

CREATE INDEX IF NOT EXISTS idx_email_outbox_patchset_id
    ON email_outbox(patchset_id);

CREATE INDEX IF NOT EXISTS idx_email_outbox_review_id
    ON email_outbox(review_id);

CREATE UNIQUE INDEX IF NOT EXISTS idx_email_outbox_cover_reply_unique
    ON email_outbox(patchset_id)
    WHERE patchset_id IS NOT NULL AND patch_id IS NULL AND kind = 'review_reply';
