-- Series-level version and lineage columns on patchsets
ALTER TABLE patchsets ADD COLUMN version INTEGER;
ALTER TABLE patchsets ADD COLUMN change_id TEXT;
ALTER TABLE patchsets ADD COLUMN previous_patchset_id INTEGER REFERENCES patchsets(id);
ALTER TABLE patchsets ADD COLUMN version_link_signal TEXT;

CREATE INDEX IF NOT EXISTS idx_patchsets_change_id
    ON patchsets(change_id) WHERE change_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_patchsets_previous_patchset_id
    ON patchsets(previous_patchset_id);

CREATE INDEX IF NOT EXISTS idx_patchsets_author_email_date
    ON patchsets(
        LOWER(TRIM(
            CASE
                WHEN INSTR(author, '<') > 0 AND INSTR(author, '>') > INSTR(author, '<')
                THEN SUBSTR(author, INSTR(author, '<') + 1, INSTR(author, '>') - INSTR(author, '<') - 1)
                ELSE author
            END
        )),
        date DESC
    );

-- Patch-level lineage columns on patches
ALTER TABLE patches ADD COLUMN previous_patch_id INTEGER REFERENCES patches(id);
ALTER TABLE patches ADD COLUMN previous_patch_status TEXT;

CREATE INDEX IF NOT EXISTS idx_patches_previous_patch_id
    ON patches(previous_patch_id);
