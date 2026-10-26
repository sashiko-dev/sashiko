#!/bin/bash
#
# Entrypoint of the Sashiko image, for every project it reviews.
#
# The project and the path of its tree come from the environment
# (SASHIKO__PROJECT__KIND, SASHIKO__GIT__REPOSITORY_PATH) or Settings.toml,
# which `sashiko repo sync` and the daemon both read, so this script names
# neither. The tree is provisioned before the server starts because the
# daemon refuses to start without one: a restart on an existing volume only
# fetches, and only the first boot on an empty volume clones.
set -euo pipefail

# The Dockerfile sets SASHIKO__GIT__REPOSITORY_PATH, and the environment wins
# over Settings.toml, so this is the path `sashiko repo sync` and the daemon
# use too. Deployments that move the tree override the variable, never the
# file.
REPO_DIR="${SASHIKO__GIT__REPOSITORY_PATH:-/app/third_party/linux}"

# Handle Cloud Run PORT environment variable mapping early
if [ -n "${PORT:-}" ]; then
    echo "Mapping PORT=$PORT to SASHIKO__SERVER__PORT"
    export SASHIKO__SERVER__PORT="$PORT"
fi

# The volume is owned by the fsGroup while the process runs as root, and git
# refuses to work in a tree it considers owned by someone else. Only commands
# that find the repository from REPO_DIR are checked: git skips the check for
# a git directory named explicitly, as `sashiko repo sync` names it when it
# inspects the tree.
git config --global --add safe.directory "$REPO_DIR"

# The database's directory, wherever SASHIKO__DATABASE__URL puts it; a remote
# database URL has none to create.
DB_URL="${SASHIKO__DATABASE__URL:-/data/db/sashiko.db}"
case "$DB_URL" in
/*) mkdir -p "$(dirname "$DB_URL")" ;;
esac

# Every relative path in the settings resolves against /app.
cd /app

/usr/local/bin/sashiko repo sync

# Sends a known Linux commit for review once the server answers, as a smoke
# test of a fresh deployment. Other projects have no such commit.
delayed_test_query() {
    local port="${SASHIKO__SERVER__PORT:-8080}"
    until curl -sf "http://localhost:$port/health" >/dev/null; do
        sleep 10
    done

    echo "Background task: Server is up. Sending test query to port $port..."
    curl -s -X POST "http://localhost:$port/api/submit" \
        -H "Content-Type: application/json" \
        -d '{"type": "remote", "sha": "1507f51255c9ff07d75909a84e7c0d7f3c4b2f49", "repo": "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"}' || echo "Test query failed"
}

if [ "${SASHIKO__PROJECT__KIND:-linux}" = "linux" ]; then
    delayed_test_query &
fi

echo "Starting Sashiko..."

# Run sashiko. Logging to stdout/stderr is automatically captured by Cloud Run.
exec /usr/local/bin/sashiko "$@"
