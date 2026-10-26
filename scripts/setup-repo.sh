#!/bin/bash
# Clones, or updates, the upstream tree that a project reviews.
#
# Usage: scripts/setup-repo.sh [PROJECT] [DEST]
#
# PROJECT defaults to "linux" and DEST to third_party/<PROJECT>, which
# matches the default [git] repository_path in Settings.toml. The clone
# keeps full history: reviews resolve baselines, blame and Fixes: tags
# against it, so a shallow clone is not sufficient.
set -euo pipefail

# Git honours these before the working directory; clear them so the
# commands below always operate on DEST, even when run from a git hook.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_COMMON_DIR

PROJECT="${1:-linux}"

case "$PROJECT" in
linux)
    URL="https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"
    BRANCH="master"
    ;;
*)
    echo "error: unknown project '$PROJECT' (known: linux)" >&2
    exit 1
    ;;
esac

DEST="${2:-third_party/$PROJECT}"

# A gitfile (left behind by a former submodule checkout) is as valid as a
# .git directory, so test for existence rather than for a directory.
if [ -e "$DEST/.git" ]; then
    echo "Updating $PROJECT tree in $DEST..."
    if ! git -C "$DEST" remote get-url origin >/dev/null 2>&1; then
        git -C "$DEST" remote add origin "$URL"
    fi
    git -C "$DEST" fetch origin "$BRANCH"
else
    echo "Cloning $PROJECT tree from $URL into $DEST..."
    mkdir -p "$(dirname "$DEST")"
    git clone --branch "$BRANCH" "$URL" "$DEST"
fi
