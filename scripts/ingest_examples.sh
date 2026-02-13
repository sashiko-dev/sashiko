#!/usr/bin/env bash
#
# Copyright 2026 The Sashiko Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

set -e

# Resolve paths
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel)"
STAT_FILE="$REPO_ROOT/third_party/prompts/kernel/examples/review-stat.txt"

if [[ ! -f "$STAT_FILE" ]]; then
    echo "Error: File $STAT_FILE not found!"
    exit 1
fi

# Extract Message IDs
# Format in file: "msgid: <...>"
# We use grep to find lines starting with 'msgid:' and sed to extract the ID inside <>
mapfile -t MSG_IDS < <(grep "^msgid:" "$STAT_FILE" | sed -E 's/^msgid:[[:space:]]*<([^>]+)>/\1/')

if [[ ${#MSG_IDS[@]} -eq 0 ]]; then
    echo "No message IDs found in $STAT_FILE"
    exit 0
fi

echo "Found ${#MSG_IDS[@]} message IDs in $(basename "$STAT_FILE")."


# Construct arguments
CMD_ARGS=("--ingest-only")
for id in "${MSG_IDS[@]}"; do
    CMD_ARGS+=("--thread" "$id")
done

# Run sashiko
echo "Running sashiko ingestion..."
cd "$REPO_ROOT"
# Use cargo run to ensure we run the latest code in the repo
cargo run -- "${CMD_ARGS[@]}"
