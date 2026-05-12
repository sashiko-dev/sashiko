#!/bin/bash
# Manually trigger a review for a specific GitHub PR
# Usage: ./trigger_github_pr_review.sh <OWNER/REPO> <PR_NUMBER>
# Example: ./trigger_github_pr_review.sh torvalds/linux 12345

set -e

if [ $# -lt 2 ]; then
    echo "Usage: $0 <OWNER/REPO> <PR_NUMBER>"
    echo "Example: $0 torvalds/linux 12345"
    exit 1
fi

REPO=$1
PR_NUMBER=$2
SERVER="http://localhost:9080"
WEBHOOK_URL="${SERVER}/api/webhook/github"

echo "Fetching PR #${PR_NUMBER} details from GitHub..."

# GitHub API (no auth needed for public repos, use GITHUB_TOKEN if available)
GITHUB_API="https://api.github.com"
AUTH_HEADER=""
if [ -n "$GITHUB_TOKEN" ]; then
    AUTH_HEADER="-H \"Authorization: Bearer $GITHUB_TOKEN\""
fi

# Fetch PR details
pr_data=$(curl -s $AUTH_HEADER "${GITHUB_API}/repos/${REPO}/pulls/${PR_NUMBER}")

# Extract required fields
number=$(echo "$pr_data" | jq -r '.number // empty')
title=$(echo "$pr_data" | jq -r '.title // empty')
html_url=$(echo "$pr_data" | jq -r '.html_url // empty')
base_sha=$(echo "$pr_data" | jq -r '.base.sha // empty')
head_sha=$(echo "$pr_data" | jq -r '.head.sha // empty')
state=$(echo "$pr_data" | jq -r '.state // empty')

# Fetch repository details
repo_data=$(curl -s $AUTH_HEADER "${GITHUB_API}/repos/${REPO}")
clone_url=$(echo "$repo_data" | jq -r '.clone_url // empty')

if [ -z "$number" ] || [ -z "$base_sha" ] || [ -z "$head_sha" ]; then
    echo "Error: Could not fetch PR details. Check PR number and network connection."
    echo "API Response:"
    echo "$pr_data" | jq . 2>/dev/null || echo "$pr_data"
    exit 1
fi

echo "PR #${number}: ${title}"
echo "State: ${state}"
echo "Base SHA: ${base_sha}"
echo "Head SHA: ${head_sha}"
echo "URL: ${html_url}"
echo "---"

# Build webhook payload
read -r -d '' PAYLOAD <<EOF || true
{
  "action": "opened",
  "pull_request": {
    "number": ${number},
    "title": $(echo "$title" | jq -R .),
    "html_url": "${html_url}",
    "base": {
      "sha": "${base_sha}"
    },
    "head": {
      "sha": "${head_sha}"
    }
  },
  "repository": {
    "clone_url": "${clone_url}"
  }
}
EOF

echo "Sending webhook to Sashiko..."

# Send the webhook
response=$(curl -s -w "\nHTTP_CODE:%{http_code}" \
  -X POST \
  -H "Content-Type: application/json" \
  -H "X-GitHub-Event: pull_request" \
  -d "$PAYLOAD" \
  "${WEBHOOK_URL}")

# Extract HTTP code and body
http_code=$(echo "$response" | grep "HTTP_CODE:" | cut -d: -f2)
body=$(echo "$response" | grep -v "HTTP_CODE:")

echo "Response Code: ${http_code}"
echo "Response Body:"
echo "$body" | jq . 2>/dev/null || echo "$body"

if [ "$http_code" = "200" ]; then
    echo ""
    echo "✓ Review queued successfully!"
    echo "Monitor at: ${SERVER}/"
else
    echo ""
    echo "✗ Failed to queue review"
    exit 1
fi
