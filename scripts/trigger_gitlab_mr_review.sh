#!/bin/bash
# Manually trigger a review for a specific GitLab MR
# Usage: ./trigger_gitlab_mr_review.sh <MR_NUMBER> <GITLAB_PROJECT_PATH>
#
# GITLAB_PROJECT_PATH is the URL-encoded path to the GitLab project
# (e.g., "my-org%2Fmy-group%2Fmy-project")

set -e

if [ $# -lt 2 ]; then
    echo "Usage: $0 <MR_NUMBER> <GITLAB_PROJECT_PATH>"
    echo ""
    echo "GITLAB_PROJECT_PATH is the URL-encoded project path."
    echo "Example: $0 123 my-org%2Fmy-project"
    echo "Example: $0 456 group%2Fsubgroup%2Fproject"
    exit 1
fi

MR_NUMBER=$1
GITLAB_PROJECT="$2"
SERVER="http://localhost:9080"
WEBHOOK_URL="${SERVER}/api/webhook/gitlab"

GITLAB_API="https://gitlab.com/api/v4"

# Set up authentication if GITLAB_TOKEN is available
AUTH_HEADER=""
if [ -n "$GITLAB_TOKEN" ]; then
    AUTH_HEADER="-H \"PRIVATE-TOKEN: ${GITLAB_TOKEN}\""
    echo "Using GitLab authentication token"
fi

echo "Fetching MR !${MR_NUMBER} details from GitLab..."

# Fetch MR details from GitLab API
if [ -n "$GITLAB_TOKEN" ]; then
    mr_data=$(curl -s -H "PRIVATE-TOKEN: ${GITLAB_TOKEN}" "${GITLAB_API}/projects/${GITLAB_PROJECT}/merge_requests/${MR_NUMBER}")
else
    mr_data=$(curl -s "${GITLAB_API}/projects/${GITLAB_PROJECT}/merge_requests/${MR_NUMBER}")
fi

# Extract required fields
iid=$(echo "$mr_data" | jq -r '.iid // empty')
title=$(echo "$mr_data" | jq -r '.title // empty')
base_sha=$(echo "$mr_data" | jq -r '.diff_refs.base_sha // empty')
head_sha=$(echo "$mr_data" | jq -r '.diff_refs.head_sha // empty')
source_branch=$(echo "$mr_data" | jq -r '.source_branch // empty')
target_branch=$(echo "$mr_data" | jq -r '.target_branch // empty')
state=$(echo "$mr_data" | jq -r '.state // empty')

# Fetch project details to get repository URL
echo "Fetching project details..."
if [ -n "$GITLAB_TOKEN" ]; then
    project_data=$(curl -s -H "PRIVATE-TOKEN: ${GITLAB_TOKEN}" "${GITLAB_API}/projects/${GITLAB_PROJECT}")
else
    project_data=$(curl -s "${GITLAB_API}/projects/${GITLAB_PROJECT}")
fi
git_http_url=$(echo "$project_data" | jq -r '.http_url_to_repo // empty')
web_url=$(echo "$project_data" | jq -r '.web_url // empty')

if [ -z "$iid" ] || [ -z "$base_sha" ] || [ -z "$head_sha" ]; then
    echo "Error: Could not fetch MR details. Check MR number and network connection."
    echo "API Response:"
    echo "$mr_data" | jq . 2>/dev/null || echo "$mr_data"
    if echo "$mr_data" | grep -q "404 Project Not Found" && [ -z "$GITLAB_TOKEN" ]; then
        echo ""
        echo "This appears to be a private repository. Set GITLAB_TOKEN to authenticate:"
        echo "  export GITLAB_TOKEN='your_token_here'"
        echo "Get a token at: https://gitlab.com/-/profile/personal_access_tokens"
    fi
    exit 1
fi

if [ -z "$git_http_url" ]; then
    echo "Error: Could not fetch repository URL from project API."
    echo "Project API Response:"
    echo "$project_data" | jq . 2>/dev/null || echo "$project_data"
    if echo "$project_data" | grep -q "404 Project Not Found" && [ -z "$GITLAB_TOKEN" ]; then
        echo ""
        echo "This appears to be a private repository. Set GITLAB_TOKEN to authenticate:"
        echo "  export GITLAB_TOKEN='your_token_here'"
        echo "Get a token at: https://gitlab.com/-/profile/personal_access_tokens"
    fi
    exit 1
fi

echo "MR !${iid}: ${title}"
echo "Branches: ${source_branch} → ${target_branch} (${state})"
echo "Base SHA: ${base_sha}"
echo "Head SHA: ${head_sha}"
echo "Repo: ${git_http_url}"
echo "Web URL: ${web_url}"
echo "---"

# Build webhook payload
# Construct MR URL from web_url and iid
mr_url="${web_url}/-/merge_requests/${iid}"

read -r -d '' PAYLOAD <<EOF || true
{
  "object_kind": "merge_request",
  "event_type": "merge_request",
  "object_attributes": {
    "iid": ${iid},
    "title": $(echo "$title" | jq -R .),
    "action": "open",
    "source_branch": "${source_branch}",
    "target_branch": "${target_branch}",
    "url": "${mr_url}",
    "last_commit": {
      "id": "${head_sha}"
    },
    "diff_refs": {
      "base_sha": "${base_sha}",
      "head_sha": "${head_sha}"
    }
  },
  "project": {
    "git_http_url": "${git_http_url}",
    "web_url": "${web_url}"
  }
}
EOF

echo "Sending webhook to Sashiko..."

# Send the webhook
response=$(curl -s -w "\nHTTP_CODE:%{http_code}" \
  -X POST \
  -H "Content-Type: application/json" \
  -H "X-Gitlab-Event: Merge Request Hook" \
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
