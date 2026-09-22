CREATE INDEX IF NOT EXISTS idx_patchsets_mr_number ON patchsets(mr_number) WHERE mr_number IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_patchsets_author_date ON patchsets(author, date);
