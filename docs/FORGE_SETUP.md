# Forge Integration Setup

This guide explains how to integrate Sashiko with Git forges (GitHub, GitLab, etc.) for automatic code review.

## Overview

Sashiko can integrate with Git forges to automatically review Pull Requests (PRs) or Merge Requests (MRs) when they are opened or updated. This enables automated code review workflows for your development teams.

## Supported Forges

Currently supported:
- **GitHub** - Full webhook integration ([GitHub Setup Guide](GITHUB_SETUP.md))
- **GitLab** - Full webhook integration ([GitLab Setup Guide](GITLAB_SETUP.md))

## Forge Requirements

For a Git forge to be compatible with Sashiko, it must support:

### 1. Webhook Delivery

**Required capabilities:**
- HTTP/HTTPS webhook delivery to external endpoints
- JSON payload format
- Custom headers (for event identification)
- Configurable event triggers (PR/MR events)

**Event triggers needed:**
- Pull/Merge request opened
- Pull/Merge request updated (new commits)
- Pull/Merge request reopened

**Optional but recommended:**
- Webhook secret/signature validation
- Webhook delivery retry logic
- Webhook delivery status/logs

### 2. API Access

**Required endpoints:**
- Fetch PR/MR details by ID
- Retrieve commit information
- Access diff/patch data

**Authentication:**
- Public repository access (no auth for public repos)
- Token-based authentication (for private repos)
- Rate limiting information

**Optional but useful:**
- Post comments on PRs/MRs
- Update PR/MR status
- Create review summaries

### 3. Git Repository Access

**Required:**
- HTTP(S) clone URLs accessible from Sashiko server
- Support for standard Git protocol
- Ability to fetch specific commit ranges

**Authentication options:**
- Anonymous access for public repos
- SSH key authentication
- HTTP(S) token authentication
- Deploy keys

### 4. Webhook Payload Requirements

The webhook must provide:

**Minimal required fields:**
```json
{
  "event_type": "pull_request|merge_request",
  "action": "opened|updated|reopened",
  "pr_or_mr": {
    "id": "<unique identifier>",
    "title": "<PR/MR title>",
    "base_commit": "<base SHA>",
    "head_commit": "<head SHA>",
    "url": "<web URL to PR/MR>"
  },
  "repository": {
    "clone_url": "<git clone URL>"
  }
}
```

**Recommended additional fields:**
- Source branch name
- Target branch name
- Author information
- Commit count
- Changed files list
- PR/MR state (open, closed, merged)

## General Configuration

### Enable Forge Integration

Edit `Settings.toml`:

```toml
[forge]
enabled = true

# Optional: Configure subsystem mapping for targeted reviews
[subsystems]
mapping = [
    { pattern = ".*drivers/.*", name = "Drivers" },
    { pattern = ".*net/.*", name = "Networking" },
    { pattern = ".*fs/.*", name = "Filesystems" },
    { pattern = ".*mm/.*", name = "Memory Management" },
]
```

### Server Configuration

Ensure your server is configured to accept webhooks:

```toml
[server]
host = "127.0.0.1"  # Listen address
port = 8080          # Port for webhook endpoint
```

**Security considerations:**
- By default, Sashiko only accepts webhooks from localhost
- For production, use `--enable-unsafe-all-submit` flag (behind firewall/proxy)
- Always use HTTPS in production with valid certificates
- Implement webhook signature validation when available

### Git Configuration

Configure the reference repository:

```toml
[git]
repository_path = "/path/to/kernel/repo"
```

**Note:** The repository must be accessible and contain the commits referenced in webhooks.

### Review Configuration

```toml
[review]
concurrency = 20                    # Parallel review capacity
worktree_dir = "review_trees"      # Temporary worktrees
timeout_seconds = 7200              # 2 hours per review
```

## Webhook Endpoints

Sashiko provides the following webhook endpoints:

- **GitHub**: `http://your-server:8080/api/webhook/github`
- **GitLab**: `http://your-server:8080/api/webhook/gitlab`

### Expected HTTP Headers

**GitHub:**
```
X-GitHub-Event: pull_request
Content-Type: application/json
```

**GitLab:**
```
X-Gitlab-Event: Merge Request Hook
Content-Type: application/json
```

### Response Codes

- `200 OK` - Webhook accepted, review queued
- `400 Bad Request` - Invalid payload or missing required fields
- `403 Forbidden` - Forge integration disabled
- `500 Internal Server Error` - Server error processing webhook

## Testing Integration

### 1. Test with Synthetic Webhook

Use the provided test scripts to verify the endpoint:

**GitHub:**
```bash
./scripts/test_github_webhook.sh
```

**GitLab:**
```bash
./scripts/test_gitlab_webhook.sh
```

### 2. Test with Real PR/MR

Trigger a review for a specific PR/MR:

**GitHub:**
```bash
./scripts/trigger_github_pr_review.sh owner/repo 123
```

**GitLab:**
```bash
./scripts/trigger_gitlab_mr_review.sh 123
```

### 3. Verify Review Queue

Check the web UI to confirm the review was queued:
```
http://localhost:8080/
```

Look for:
- Review in "Pending" or "In Progress" state
- Correct PR/MR number and title
- Expected commit range

## Implementing Support for New Forges

To add support for a new Git forge, you need to:

### 1. Understand the Forge's Webhook Format

Document:
- Webhook payload structure
- HTTP headers used for event identification
- Available event types and actions
- Authentication/signature mechanism

### 2. Create a ForgeProvider Implementation

Implement the `ForgeProvider` trait in Rust:

```rust
pub trait ForgeProvider {
    /// Parse the webhook payload and extract review information
    fn parse_webhook(&self, headers: &HeaderMap, body: &[u8])
        -> Result<WebhookEvent, Error>;

    /// Optional: Post review results back to the forge
    fn post_review(&self, pr_id: &str, review: &Review)
        -> Result<(), Error>;
}
```

### 3. Register the Webhook Handler

Add a new endpoint in the API router:

```rust
// In src/api.rs or equivalent
app.route("/api/webhook/yourforge", post(handle_yourforge_webhook))
```

### 4. Add Configuration

Extend `Settings.toml` if forge-specific config is needed:

```toml
[forge.yourforge]
api_token = "optional-token"
api_endpoint = "https://api.yourforge.com"
```

### 5. Create Documentation and Scripts

- Write setup guide (e.g., `docs/YOURFORGE_SETUP.md`)
- Create test script (`scripts/test_yourforge_webhook.sh`)
- Create trigger script (`scripts/trigger_yourforge_review.sh`)

## Troubleshooting

### Webhook Not Received

**Symptoms:** No log entries when PR/MR is opened

**Check:**
- Server is running: `curl http://localhost:8080/`
- Firewall allows inbound on configured port
- Webhook URL is accessible from forge servers
- Forge shows successful delivery in webhook logs

**Solutions:**
- Use ngrok/tunneling for local testing
- Check server logs: `RUST_LOG=debug cargo run`
- Verify webhook URL in forge configuration

### Webhook Rejected (403 Forbidden)

**Symptoms:** Forge shows 403 response

**Cause:** Forge integration disabled or security restrictions

**Solutions:**
- Set `forge.enabled = true` in Settings.toml
- Restart Sashiko daemon
- Use `--enable-unsafe-all-submit` for testing (not production)

### Review Not Starting

**Symptoms:** Webhook accepted but review doesn't begin

**Check:**
- LLM API key configured: `echo $LLM_API_KEY`
- Git repository contains referenced commits
- Sashiko has network access to clone repository
- Disk space available in worktree directory

**Solutions:**
- Check logs for specific errors
- Verify git repository accessibility
- Test LLM provider connection
- Ensure commits exist in configured repository

### Commits Not Found

**Symptoms:** Review fails with "commit not found" error

**Causes:**
- Repository mismatch (webhook references different repo)
- Commits not yet fetched to local repository
- Shallow clone missing history

**Solutions:**
- Ensure `git.repository_path` points to correct repo
- Run `git fetch --all` in repository
- Use full clone (not shallow) for reference repository

## Security Best Practices

### Production Deployment

1. **Use HTTPS with valid certificates**
   - Set up reverse proxy (nginx, Apache, Caddy)
   - Obtain SSL/TLS certificates (Let's Encrypt)
   - Terminate TLS at proxy, forward to Sashiko

2. **Implement authentication**
   - Use webhook secrets when available
   - Validate webhook signatures
   - Restrict by source IP (if forge IPs are known)

3. **Network isolation**
   - Run Sashiko on private network
   - Use VPN or SSH tunneling for access
   - Firewall rules to limit exposure

4. **Rate limiting**
   - Configure at reverse proxy level
   - Prevent abuse and DoS attempts
   - Monitor webhook delivery rates

5. **Audit logging**
   - Log all webhook deliveries
   - Track review queue metrics
   - Monitor for unusual patterns

### Example Nginx Configuration

```nginx
server {
    listen 443 ssl http2;
    server_name sashiko.example.com;

    ssl_certificate /etc/letsencrypt/live/sashiko.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sashiko.example.com/privkey.pem;

    # Security headers
    add_header Strict-Transport-Security "max-age=31536000" always;
    add_header X-Frame-Options DENY;
    add_header X-Content-Type-Options nosniff;

    # Rate limiting
    limit_req_zone $binary_remote_addr zone=webhook:10m rate=10r/m;
    limit_req zone=webhook burst=5;

    location /api/webhook/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

## Forge Comparison

| Feature | GitHub | GitLab | Requirements |
|---------|--------|--------|--------------|
| Webhook delivery | ✅ | ✅ | **Required** |
| JSON payloads | ✅ | ✅ | **Required** |
| Event filtering | ✅ | ✅ | **Required** |
| Webhook secrets | ✅ | ✅ | Recommended |
| Signature validation | ✅ | ✅ | Recommended |
| Delivery logs | ✅ | ✅ | Recommended |
| Public API | ✅ | ✅ | **Required** |
| Token auth | ✅ | ✅ | **Required** |
| SSH clone | ✅ | ✅ | **Required** |
| HTTPS clone | ✅ | ✅ | **Required** |
| Comment API | ✅ | ✅ | Optional |
| Status API | ✅ | ✅ | Optional |

## Related Documentation

- [GitHub Setup Guide](GITHUB_SETUP.md) - Detailed GitHub integration
- [GitLab Setup Guide](GITLAB_SETUP.md) - Detailed GitLab integration
- [README.md](../README.md) - Main project documentation
- [Settings.toml](../Settings.toml) - Configuration reference

## Getting Help

- **Mailing List:** sashiko@lists.linux.dev ([archive](https://lore.kernel.org/sashiko))
- **GitHub Issues:** [Report bugs or request features](https://github.com/sashiko-dev/sashiko/issues)
- **Community:** Join discussions about forge integration and feature requests
