#!/bin/bash
# Test GitHub webhook integration locally

SERVER="http://localhost:9080"
WEBHOOK_URL="${SERVER}/api/webhook/github"

# Example GitHub PR webhook payload
read -r -d '' PAYLOAD <<'EOF'
{
  "action": "opened",
  "pull_request": {
    "number": 123,
    "title": "Test PR for webhook",
    "html_url": "https://github.com/owner/repo/pull/123",
    "base": {
      "sha": "6f6d7e7447811dbecc13cc7fbbe9f5e7a3d7c70b"
    },
    "head": {
      "sha": "da1560886d4f094c3e6c9ef40349f7d38b5d27d7"
    }
  },
  "repository": {
    "clone_url": "https://github.com/owner/repo.git"
  }
}
EOF

echo "Testing GitHub webhook at: ${WEBHOOK_URL}"
echo "---"

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
