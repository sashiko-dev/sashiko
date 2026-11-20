-- Pre-existing bugs that a review mentioned to the patch author.
--
-- A review may point out a pre-existing bug that relates to the patch under
-- review (see designs/DESIGN_RELATED_PREEXISTING_BUGS.md). The reviewer
-- reserves a row for each such bug before mailing it, inside one write
-- transaction that first checks the rows already present in the same files,
-- so concurrent reviews can never report the same bug twice. The row also
-- records when and with which patch the bug was reported.
--
-- Rows are keyed by the Message-ID of the reviewed patch rather than by
-- patch, patchset, review or outbox row ids: patchset merges, reruns and
-- embargo re-queues delete and rewrite those rows, while the Message-ID of
-- the patch stays the same.
CREATE TABLE IF NOT EXISTS preexisting_disclosures (
    id INTEGER PRIMARY KEY,
    patch_message_id TEXT NOT NULL,
    -- Normalized problem and location; identifies the bug within a patch.
    fingerprint TEXT NOT NULL,
    problem TEXT NOT NULL,
    severity INTEGER NOT NULL,
    relation TEXT NOT NULL CHECK (relation IN ('same_code', 'same_pattern', 'other')),
    locations_json TEXT NOT NULL,
    -- JSON key (fingerprint, files, symbols, problem words) the reviewer
    -- compares candidates against to tell whether they are the same bug.
    bug_key_json TEXT NOT NULL,
    -- Self-contained note rendered into the review email.
    report_text TEXT NOT NULL,
    reserved_at INTEGER NOT NULL,
    -- Set when the review email carrying the note is queued; NULL while the
    -- row only reserves the bug (for example during an embargo).
    disclosed_at INTEGER,
    -- Tracked bug, once the bug pipeline has linked the review to one.
    bug_id INTEGER REFERENCES bugs(id),
    UNIQUE (patch_message_id, fingerprint)
);

CREATE INDEX IF NOT EXISTS idx_preexisting_disclosures_bug
    ON preexisting_disclosures(bug_id) WHERE bug_id IS NOT NULL;

-- Files each disclosure points at, so a duplicate check only compares a
-- candidate with disclosures in the same files instead of scanning every row.
CREATE TABLE IF NOT EXISTS preexisting_disclosure_files (
    disclosure_id INTEGER NOT NULL REFERENCES preexisting_disclosures(id) ON DELETE CASCADE,
    file TEXT NOT NULL,
    PRIMARY KEY (disclosure_id, file)
);

CREATE INDEX IF NOT EXISTS idx_preexisting_disclosure_files_file
    ON preexisting_disclosure_files(file);
