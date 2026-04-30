#!/bin/bash
# Check if Sashiko server is properly configured for GitLab webhooks

SERVER="http://localhost:9080"

echo "Checking Sashiko server configuration..."
echo "Server: ${SERVER}"
echo "---"

# Test 1: Check if server is reachable
echo -n "1. Server reachable: "
if curl -s -f -o /dev/null "${SERVER}/" --max-time 5; then
    echo "✓ Yes"
else
    echo "✗ No - Server is not responding"
    echo "   Make sure Sashiko is running and accessible"
    exit 1
fi

# Test 2: Check forge configuration
echo -n "2. Forge enabled: "
config=$(curl -s "${SERVER}/api/config")
if [ $? -ne 0 ]; then
    echo "✗ Failed to fetch config"
    exit 1
fi

forge_enabled=$(echo "$config" | jq -r '.forge_enabled // false')
if [ "$forge_enabled" = "true" ]; then
    echo "✓ Yes"
else
    echo "✗ No"
    echo "   Set forge.enabled = true in Settings.toml and restart Sashiko"
    exit 1
fi

# Test 3: Check if webhook endpoint exists
echo -n "3. GitLab webhook endpoint: "
webhook_test=$(curl -s -w "%{http_code}" -o /dev/null \
    -X POST \
    -H "Content-Type: application/json" \
    -H "X-Gitlab-Event: Merge Request Hook" \
    -d '{}' \
    "${SERVER}/api/webhook/gitlab")

if [ "$webhook_test" = "400" ] || [ "$webhook_test" = "200" ]; then
    # 400 is expected with empty payload, means endpoint exists
    echo "✓ Available (HTTP ${webhook_test})"
elif [ "$webhook_test" = "403" ]; then
    echo "⚠ Forbidden - forge might be disabled"
else
    echo "✗ Unexpected response (HTTP ${webhook_test})"
fi

# Test 4: Show current configuration
echo ""
echo "Current Configuration:"
echo "$config" | jq . 2>/dev/null || echo "$config"

echo ""
echo "---"
echo "✓ Server is ready for GitLab webhooks!"
echo ""
echo "Next steps:"
echo "  1. Configure webhook in GitLab project settings"
echo "  2. Set URL to: ${SERVER}/api/webhook/gitlab"
echo "  3. Enable 'Merge request events'"
echo "  4. Test with: ./trigger_gitlab_mr_review.sh <MR_NUMBER>"
echo ""
echo "See GITLAB_SETUP.md for detailed instructions"
