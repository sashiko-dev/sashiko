#!/bin/bash
# Test GitLab webhook integration locally

SERVER="http://localhost:9080"
WEBHOOK_URL="${SERVER}/api/webhook/gitlab"

# Example GitLab MR webhook payload
# This simulates what GitLab sends when an MR is opened or updated
read -r -d '' PAYLOAD <<'EOF'
{
  "object_kind": "merge_request",
  "event_type": "merge_request",
  "object_attributes": {
    "iid": 123,
    "action": "open",
    "source_branch": "feature-branch",
    "target_branch": "main",
    "last_commit": {
      "id": "da1560886d4f094c3e6c9ef40349f7d38b5d27d7"
    },
    "diff_refs": {
      "base_sha": "6f6d7e7447811dbecc13cc7fbbe9f5e7a3d7c70b",
      "head_sha": "da1560886d4f094c3e6c9ef40349f7d38b5d27d7"
    }
  },
  "project": {
    "git_http_url": "https://gitlab.example.com/org/project.git"
  }
}
EOF

echo "Testing GitLab webhook at: ${WEBHOOK_URL}"
echo "---"

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
    echo "✓ Webhook test successful!"
    echo "Check sashiko logs to see if the review was queued."
else
    echo ""
    echo "✗ Webhook test failed!"
    echo "Make sure:"
    echo "  1. Sashiko server is running"
    echo "  2. forge.enabled = true in Settings.toml"
    echo "  3. Server is accessible at ${SERVER}"
fi
