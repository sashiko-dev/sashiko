-- The review stages that raised a finding, as the JSON array the review
-- produced. Nullable: every row written before this migration has no
-- provenance to record, and a finding whose model dropped the field has none
-- either, which is a different thing from an empty array.
ALTER TABLE findings ADD COLUMN stages TEXT;
