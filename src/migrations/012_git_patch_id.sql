-- Store the stable Git patch ID emitted by git patch-id so prerequisite
-- patches named by b4 trailers can be found without another lore request.
ALTER TABLE patches ADD COLUMN git_patch_id TEXT;

CREATE INDEX IF NOT EXISTS idx_patches_git_patch_id
    ON patches(git_patch_id);
