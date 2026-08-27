#!/usr/bin/env bash
set -e

DB_FILE=${1:-sashiko.db}

if [ ! -f "$DB_FILE" ]; then
    echo "No database file $DB_FILE found. Skipping invariant checks."
    exit 0
fi

echo "Checking DB invariants on $DB_FILE..."

# Invariant 1: No review should point to an ephemeral or tombstone bug.
BAD_LINKS=$(sqlite3 "$DB_FILE" "SELECT count(*) FROM review_bugs r JOIN bugs b ON r.bug_id = b.id WHERE b.status IN ('raw', 'processing', 'duplicate');")
if [ "$BAD_LINKS" -gt 0 ]; then
    echo "ERROR: DB Invariant Violation: Found $BAD_LINKS review_bugs pointing to ephemeral/tombstone bugs!"
    sqlite3 "$DB_FILE" "SELECT r.review_id, r.bug_id, b.status FROM review_bugs r JOIN bugs b ON r.bug_id = b.id WHERE b.status IN ('raw', 'processing', 'duplicate');"
    exit 1
fi

# Invariant 2: No bugs should have BOTH vector_json missing AND status open/fixed/verified 
# (unless it's an extreme edge case, but our logic generates vectors for all validated bugs)
# We will skip this one for now unless we know it's always true.
# Let's ensure duplicate bugs don't have is_fixed = 1
BAD_DUPS=$(sqlite3 "$DB_FILE" "SELECT count(*) FROM bugs WHERE status = 'duplicate' AND is_fixed = 1;")
if [ "$BAD_DUPS" -gt 0 ]; then
    echo "ERROR: DB Invariant Violation: Found duplicate bugs marked as fixed."
    exit 1
fi

echo "All database invariants passed!"
