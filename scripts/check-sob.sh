#!/bin/bash
# check-sob.sh: Validates that all commits in a range have a matching Signed-off-by tag.
# Usage: ./scripts/check-sob.sh [range]

set -e

RANGE=$1

if [ -z "$RANGE" ]; then
    # Default to checking only the HEAD commit if no range is provided
    RANGE="HEAD~1..HEAD"
fi

echo "Checking Developer Certificate of Origin (S-O-B) in range: $RANGE"

for commit in $(git rev-list "$RANGE" --no-merges); do
    AUTHOR_NAME=$(git log -1 --format="%an" "$commit")
    AUTHOR_EMAIL=$(git log -1 --format="%ae" "$commit")
    
    # We allow the SOB to match either the author's name or the author's email.
    # This handles cases where a developer might use different emails for different
    # environments but the same name, or vice-versa.
    if ! git log -1 --format="%b" "$commit" | grep -qEi "^Signed-off-by: ($AUTHOR_NAME <|.* <$AUTHOR_EMAIL>)"; then
        echo "ERROR: DCO mismatch in commit $commit"
        echo "Author: $AUTHOR_NAME <$AUTHOR_EMAIL>"
        echo "Commit message must contain a Signed-off-by tag matching the author's name or email."
        exit 1
    fi
done

echo "All commits in $RANGE have valid matching sign-offs!"
