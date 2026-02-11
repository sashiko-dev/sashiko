#!/bin/bash
set -e

# Default paths (can be overridden)
BASE_DIR="${1:-./tmp-run}"
mkdir -p "$BASE_DIR"
BASE_DIR=$(realpath "$BASE_DIR")
REMOTE_DIR="$BASE_DIR/mock-kernel"
CACHE_DIR="$BASE_DIR/third_party/linux"
WORKTREE_DIR="$BASE_DIR/review_trees"
ROOT_SRC_DIR="$PWD"

# Default params
PORT="${2:-8080}"

# Clean up previous run
rm -rf "$BASE_DIR"
mkdir -p "$BASE_DIR"

# 1. Create Mock Remote
echo "Creating mock remote at $REMOTE_DIR..."
mkdir -p "$REMOTE_DIR"
cd "$REMOTE_DIR"
git init -q -b master
# Configure git locally for this repo
git config user.email "test@example.com"
git config user.name "Test User"

# Create a dummy file and commit
echo "int main() { return 0; }" > main.c
git add main.c
GIT_AUTHOR_DATE="2026-02-06T12:00:00Z" GIT_COMMITTER_DATE="2026-02-06T12:00:00Z" git commit -q -m "Initial commit"
HEAD_SHA=$(git rev-parse HEAD)
REMOTE_PATH=$(pwd)
echo "Mock remote initialized. Head: $HEAD_SHA"

# 2. Create Cache (Bare Repo)
echo "Creating mock cache (bare) at $CACHE_DIR..."
mkdir -p "$CACHE_DIR"
cd "$CACHE_DIR"
git init -q --bare -b master
CACHE_PATH=$(pwd)

# 3. Create Worktree Directory
mkdir -p "$WORKTREE_DIR"
WORKTREE_PATH=$(realpath "$WORKTREE_DIR")

# 4. Generate Git Config for Safety
# This config allows git operations in these directories regardless of ownership
MOCK_GIT_CONFIG="$BASE_DIR/gitconfig"
echo "[safe]" > "$MOCK_GIT_CONFIG"
echo "    directory = *" >> "$MOCK_GIT_CONFIG"
echo "    bareRepository = all" >> "$MOCK_GIT_CONFIG"
echo "[user]" >> "$MOCK_GIT_CONFIG"
echo "    name = Sashiko Test" >> "$MOCK_GIT_CONFIG"
echo "    email = sashiko@test.local" >> "$MOCK_GIT_CONFIG"

# 5. Generate Settings.toml Snippet
SETTINGS_FILE="$BASE_DIR/Settings.toml"
PROMPTS_PATH=$(realpath "$ROOT_SRC_DIR/third_party/review-prompts/kernel")
STATIC_PATH=$(realpath "$ROOT_SRC_DIR/static")
ARCHIVES_PATH=$(realpath "$BASE_DIR/archives")

cat <<EOF > "$SETTINGS_FILE"
log_level = "info"

[database]
url = "sashiko.db"
token = ""

[mailing_lists]
track = ["linux-kernel"]

[nntp]
server = "nntp.lore.kernel.org"
port = 119

[ai]
provider = "gemini"
model = "gemini-3-pro-preview"
max_input_tokens = 200000
max_interactions = 100
temperature = 1.0
explicit_prompts_caching = true

[server]
host = "0.0.0.0"
port = $PORT
static_dir = "$STATIC_PATH"

[git]
repository_path = "$CACHE_PATH"
archives_dir = "$ARCHIVES_PATH"

[review]
worktree_dir = "$WORKTREE_PATH"
prompts_dir = "$PROMPTS_PATH"
concurrency = 1
timeout_seconds = 300
max_retries = 3
EOF

# 6. Creating missing directories
mkdir -p "$BASE_DIR/archives"
# No need to copy static anymore as we use absolute path in settings
# cp -r "$ROOT_SRC_DIR/static" "$BASE_DIR/static"

# 7. Final Output
echo "=================================================="
echo "Mock Environment Setup Complete"
echo "Remote SHA: $HEAD_SHA"
echo "Remote URL: file://$REMOTE_PATH"
echo "Cache Path: $CACHE_PATH"
echo "Settings snippet: $SETTINGS_FILE"
echo ""
echo "To run sashiko with this environment:"
echo "export GIT_CONFIG_GLOBAL=$(realpath "$MOCK_GIT_CONFIG")"
echo "=================================================="
