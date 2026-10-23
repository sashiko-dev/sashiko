#!/usr/bin/env bash
set -e

DB_FILE=${1:-sashiko.db}

if [ ! -f "$DB_FILE" ]; then
    echo "No database file $DB_FILE found. Skipping invariant checks."
    exit 0
fi

echo "Checking DB invariants on $DB_FILE..."

FAILED=0

# Reports a violation and records that the run must fail. Takes a description
# and a query listing the offending rows.
report() {
    echo "ERROR: DB Invariant Violation: $1"
    sqlite3 "$DB_FILE" "$2"
    FAILED=1
}

count() {
    sqlite3 "$DB_FILE" "$1"
}

# Invariant 1: referential integrity across every table.
#
# Foreign keys went unenforced for most of this database's life, because the
# pragma was never switched on, so rows pointing at deleted parents can exist
# in older files. They now cause write errors, which makes finding them worth
# doing before they surface as a failed insert.
ORPHANS=$(sqlite3 "$DB_FILE" "PRAGMA foreign_key_check;" | wc -l)
if [ "$ORPHANS" -gt 0 ]; then
    echo "ERROR: DB Invariant Violation: Found $ORPHANS rows referencing a missing parent!"
    sqlite3 "$DB_FILE" "PRAGMA foreign_key_check;"
    FAILED=1
fi

# Invariant 2: no review should point to a bug that is not yet a real finding.
#
# A bug still awaiting or undergoing analysis has no verdict yet, and a
# duplicate is a tombstone pointing elsewhere. Linking a review to either
# publishes a finding that does not exist.
BAD_LINKS=$(count "SELECT count(*) FROM bug_reviews r JOIN bugs b ON r.bug_id = b.id WHERE b.pipeline_state IN ('pending', 'running') OR b.lifecycle_status = 'duplicate';")
if [ "$BAD_LINKS" -gt 0 ]; then
    report "Found $BAD_LINKS review links pointing to unanalysed or duplicate bugs!" \
        "SELECT r.review_id, r.bug_id, b.lifecycle_status, b.pipeline_state FROM bug_reviews r JOIN bugs b ON r.bug_id = b.id WHERE b.pipeline_state IN ('pending', 'running') OR b.lifecycle_status = 'duplicate';"
fi

# Invariant 3: a bug is marked duplicate if and only if it names a canonical
# bug. A CHECK constraint enforces this going forward; this catches files
# written before it existed.
BAD_DUPES=$(count "SELECT count(*) FROM bugs WHERE (lifecycle_status = 'duplicate') != (duplicate_of_id IS NOT NULL);")
if [ "$BAD_DUPES" -gt 0 ]; then
    report "Found $BAD_DUPES bugs whose duplicate status and duplicate_of_id disagree!" \
        "SELECT id, bugid, lifecycle_status, duplicate_of_id FROM bugs WHERE (lifecycle_status = 'duplicate') != (duplicate_of_id IS NOT NULL);"
fi

# Invariant 4: an assignment timestamp without an assignee is meaningless.
BAD_ASSIGN=$(count "SELECT count(*) FROM bugs WHERE assignee IS NULL AND assigned_at IS NOT NULL;")
if [ "$BAD_ASSIGN" -gt 0 ]; then
    report "Found $BAD_ASSIGN bugs with an assignment time but no assignee!" \
        "SELECT id, bugid, assignee, assigned_at FROM bugs WHERE assignee IS NULL AND assigned_at IS NOT NULL;"
fi

# Invariant 5: a bug being analysed must hold a lease, otherwise nothing will
# ever reclaim it and it stays running forever.
UNLEASED=$(count "SELECT count(*) FROM bugs WHERE pipeline_state = 'running' AND lease_expires_at IS NULL;")
if [ "$UNLEASED" -gt 0 ]; then
    report "Found $UNLEASED bugs stuck running without a lease!" \
        "SELECT id, bugid, locked_by, attempt_count FROM bugs WHERE pipeline_state = 'running' AND lease_expires_at IS NULL;"
fi

# Invariant 6: a bug folded into another has finished with the pipeline.
#
# Folding is a terminal outcome, so a duplicate still sitting in pending or
# running means the fold did not retire the analysis. That row is invisible to
# the recovery queries once its lease is gone, and it would otherwise be
# reclaimed and analysed again to rediscover a finding already recorded on the
# canonical bug.
LIVE_DUPES=$(count "SELECT count(*) FROM bugs WHERE lifecycle_status = 'duplicate' AND pipeline_state NOT IN ('succeeded', 'abandoned');")
if [ "$LIVE_DUPES" -gt 0 ]; then
    report "Found $LIVE_DUPES duplicate bugs that never left the analysis pipeline!" \
        "SELECT id, bugid, pipeline_state, duplicate_of_id, lease_expires_at FROM bugs WHERE lifecycle_status = 'duplicate' AND pipeline_state NOT IN ('succeeded', 'abandoned');"
fi

# Invariant 7: every write path compresses strings longer than 1024 bytes on
# insert or update, so no compressed column should contain uncompressed text
# exceeding 1024 bytes.
check_compressed_column() {
    local table="$1"
    local column="$2"
    local bad
    bad=$(count "SELECT count(*) FROM $table WHERE typeof($column) = 'text' AND length($column) > 1024;")
    if [ "$bad" -gt 0 ]; then
        report "Found $bad uncompressed rows > 1024 bytes in $table.$column!" \
            "SELECT id, length($column) FROM $table WHERE typeof($column) = 'text' AND length($column) > 1024 LIMIT 20;"
    fi
}

check_compressed_column messages body
check_compressed_column patches diff
check_compressed_column patchsets baseline_logs
check_compressed_column reviews logs
check_compressed_column reviews inline_review
check_compressed_column ai_interactions input_context
check_compressed_column ai_interactions output_raw
check_compressed_column bug_enrichments content
check_compressed_column bug_enrichments logs

if [ "$FAILED" -ne 0 ]; then
    exit 1
fi

echo "All database invariants passed!"
