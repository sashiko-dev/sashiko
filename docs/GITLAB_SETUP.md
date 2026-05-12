# GitLab Webhook Setup for Sashiko

This guide explains how to configure GitLab to automatically trigger Sashiko reviews for merge requests.

> **Note:** For general forge integration requirements and architecture, see [FORGE_SETUP.md](FORGE_SETUP.md). This guide covers GitLab-specific configuration.

## Quick Start (5 Minutes)

### Step 1: Enable Forge Mode

Edit `Settings.toml`:

```toml
[forge]
enabled = true
```

Restart Sashiko.

### Step 2: Verify Server is Ready

```bash
./scripts/check_server_config.sh
```

Expected output: "✓ Server is ready for GitLab webhooks!"

### Step 3: Test with a Specific MR

```bash
# Replace 123 with an actual MR number
./scripts/trigger_gitlab_mr_review.sh 123
```

This script will:
1. Fetch MR details from GitLab API
2. Send a simulated webhook to Sashiko
3. Show the response

### Step 4: Monitor Reviews

View reviews at: http://localhost:9080/

Reviews for GitLab MRs will display as:
- **Subject**: `!<MR_NUMBER>: <MR_TITLE>`
- **Author**: Original commit author
- **Parts**: Number of commits in the MR

## Prerequisites

1. Sashiko server running and accessible
2. `forge.enabled = true` in `Settings.toml`
3. Admin/Maintainer access to your GitLab project

## Configuration Steps

### 1. Enable Forge Mode in Sashiko

Edit `Settings.toml`:

```toml
[forge]
enabled = true

[subsystems]
# Optional: Map file paths to subsystems
mapping = [
    { pattern = ".*drivers/.*", name = "Drivers" },
    { pattern = ".*net/.*", name = "Networking" },
    { pattern = ".*fs/.*", name = "Filesystems" },
]
```

Restart Sashiko after changing the configuration.

### 2. Configure GitLab Webhook

1. Go to your GitLab project:
   ```
   https://gitlab.com/your-org/your-project/-/settings/integrations
   ```

2. Click **Add new webhook**

3. Configure the webhook:

   **URL:**
   ```
   http://localhost:9080/api/webhook/gitlab
   ```

   **Secret token:** (Optional - currently not validated, see security note below)

   **Trigger:**
   - ✓ Merge request events

   **SSL verification:**
   - If using HTTPS with valid cert: Enable
   - If using HTTP or self-signed: Disable

4. Click **Add webhook**

### 3. Test the Webhook

#### Option A: Use GitLab's Test Feature

1. Scroll down to "Project Hooks"
2. Find your webhook
3. Click **Test** → **Merge Request events**
4. Check the response (should be 200 OK)

#### Option B: Use the Test Script

```bash
# Make the script executable
chmod +x test_gitlab_webhook.sh

# Run the test
./test_gitlab_webhook.sh
```

#### Option C: Trigger Review for Specific MR

```bash
# Make the script executable
chmod +x trigger_gitlab_mr_review.sh

# Trigger review for MR !123
./trigger_gitlab_mr_review.sh 123
```

### 4. Verify Reviews are Running

1. Open a new MR or update an existing one
2. Check Sashiko web UI: `http://localhost:9080/`
3. Check Sashiko logs for:
   ```
   GitLab MR !123 open. Base: abc123..., Head: def456...
   ```

## Webhook Payload Details

GitLab sends webhooks with:

**Headers:**
- `X-Gitlab-Event: Merge Request Hook`
- `Content-Type: application/json`

**Actions Handled:**
- `open` - New MR created
- `update` - MR updated with new commits
- `reopen` - Closed MR reopened

**Actions Ignored:**
- `close` - MR closed
- `merge` - MR merged
- `approved`/`unapproved` - Approval state changes

## Troubleshooting

### Webhook Returns 403 Forbidden

**Cause:** Forge integration is disabled

**Fix:** Set `forge.enabled = true` in `Settings.toml` and restart Sashiko

### Webhook Returns 400 Bad Request

**Cause:** Invalid payload structure

**Check:**
1. Verify webhook is configured for "Merge request events"
2. Check Sashiko logs for parsing errors
3. Ensure `X-Gitlab-Event` header is set correctly

### Reviews Not Appearing

**Check:**
1. Sashiko logs for errors
2. Git repository is accessible from Sashiko server
3. Base and head SHAs are valid
4. FetchAgent is running (check logs)

### Network Connectivity Issues

If GitLab cannot reach your server:

1. Ensure the server is publicly accessible (or use a tunnel)
2. Check firewall rules allow inbound on port 9080
3. Consider using ngrok for testing:
   ```bash
   ngrok http 9080
   # Use the ngrok URL in webhook config
   ```

## Security Notes

⚠️ **IMPORTANT:** The current implementation does NOT validate webhook signatures.

This means anyone who knows your webhook URL can trigger reviews.

**Recommended for production:**
1. Implement webhook signature validation (see issue in code review)
2. Use HTTPS with valid certificates
3. Restrict network access to known GitLab IPs
4. Set a strong webhook secret token in GitLab

## Manual Testing

You can manually trigger reviews without webhooks:

```bash
# Using the CLI tool (if implemented)
sashiko review \
  --repo https://gitlab.com/your-org/your-project.git \
  --range abc123..def456

# Or by directly calling the API
curl -X POST http://localhost:9080/api/submit \
  -H "Content-Type: application/json" \
  -d '{
    "repo_url": "https://...",
    "commit_hash": "abc123..def456"
  }'
```

## Example MRs to Test

Test with a specific MR from your project:

```bash
# Test with a specific MR (use your URL-encoded project path)
./trigger_gitlab_mr_review.sh 1234 your-org%2Fyour-project

# Or manually find MRs at:
# https://gitlab.com/your-org/your-project/-/merge_requests
```

## See Also

- [FORGE_SETUP.md](FORGE_SETUP.md) - General forge integration guide
- [GITHUB_SETUP.md](GITHUB_SETUP.md) - GitHub integration setup
- [README.md](../README.md) - Main project documentation
- [Settings.toml](../Settings.toml) - Configuration reference
