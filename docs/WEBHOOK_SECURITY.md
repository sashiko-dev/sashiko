# Webhook Security Guide

This guide covers how to authenticate incoming webhook requests from GitHub
and GitLab to your Sashiko instance. Configuring webhook authentication
mitigates forged requests that could trigger unauthorized AI reviews, inject
arbitrary repository URLs, or pollute the review database.

## Quick start

1. Choose your [deployment topology](#deployment-topologies)
2. Generate a signing token or shared secret
3. Configure it in your forge's webhook settings (GitLab or GitHub UI)
4. Add the same value as `webhook_secret` in Sashiko's `Settings.toml`
5. Restart Sashiko and test with a real PR/MR event

## Deployment topologies

### Public server with reverse proxy

```
GitLab.com ──HTTPS──▶ nginx (TLS) ──HTTP──▶ Sashiko :8080 (localhost)
```

Sashiko binds to localhost; a reverse proxy (nginx, Caddy) terminates TLS.
Configure `webhook_secret` in `Settings.toml` — non-localhost requests from
the proxy are authenticated via the signature check.

- No `--enable-unsafe-all-submit` flag needed
- Example config: `docs/examples/Settings.forge-gitlab-production.toml`
- See [Reverse proxy examples](#reverse-proxy-examples) for nginx/Caddy setup

### Tunnel-based (ngrok, Cloudflare Tunnel, SSH)

```
GitLab.com ──HTTPS──▶ tunnel ──HTTP──▶ Sashiko :8080 (localhost)
```

The tunnel terminates locally, so requests appear as localhost. A
`webhook_secret` is still recommended because tunnel URLs can be discovered.

- Uses the same config as the public server topology
- No `--enable-unsafe-all-submit` flag needed

### Self-hosted forge on same LAN

```
GitLab (internal) ──HTTP──▶ Sashiko :8080 (LAN-accessible)
```

Sashiko binds to all interfaces (`host = "::"`). Configure `webhook_secret`
— requests from the GitLab server's IP are authenticated via the signature
check.

- No `--enable-unsafe-all-submit` flag needed
- Example config: `docs/examples/Settings.forge-selfhosted.toml`

### Script-based polling (cronjob + curl)

```
cron ──▶ poll GitLab API ──▶ curl POST ──▶ Sashiko :8080 (localhost)
```

No forge webhook — a script polls the API and posts results to Sashiko via
curl. The plain secret token method works well here (simple `-H` flag in
curl). Requests are localhost, so the secret provides defense-in-depth.

- Example config: `docs/examples/Settings.forge-gitlab-simple.toml`
- See [Script-based setup](#script-based--cronjob-setup) for curl examples

## Authentication methods

| Method | Headers | Verifies | Providers | Recommendation |
|--------|---------|----------|-----------|----------------|
| HMAC signing token | `webhook-signature`, `webhook-id`, `webhook-timestamp` | Identity + integrity | GitLab 19.0+ | Recommended |
| HMAC secret | `X-Hub-Signature-256` | Identity + integrity | GitHub | Recommended |
| Plain secret token | `X-Gitlab-Token` | Identity only | GitLab (all versions) | Acceptable with HTTPS |
| None | — | Nothing | — | Development/localhost only |

**Signing tokens and HMAC secrets** verify both that the sender holds the
configured key and that the payload has not been modified in transit.

**Plain secret tokens** verify that the sender holds the configured key but
do not independently verify payload integrity. Combine with HTTPS for
transport protection.

## GitLab signing token setup

> Requires GitLab 19.0 or later.

1. In GitLab, go to your project's **Settings > Webhooks > Add new webhook**
2. Enter the **URL**: `https://sashiko.example.com/api/webhook/gitlab`
3. Select **Generate signing token** — copy the token now (it is shown only once)
4. Under **Trigger**, check **Merge request events**
5. Ensure **Enable SSL verification** is checked
6. Select **Add webhook**

In Sashiko's `Settings.toml`:

```toml
[forge]
enabled = true
provider = "gitlab"
webhook_secret = "whsec_YOUR_COPIED_TOKEN_HERE"
```

Restart Sashiko. Open or update a merge request to trigger a test delivery.
Check Sashiko's logs for `"GitLab merge_request: ..."` to confirm receipt.

For more details, see
[GitLab's signing token documentation](https://docs.gitlab.com/ee/user/project/integrations/webhooks/#signing-tokens).

## GitLab legacy secret token setup

For GitLab versions before 19.0, or for simpler setups where HMAC signing
is not needed:

1. In GitLab, go to your project's **Settings > Webhooks > Add new webhook**
2. Enter the **URL**: `https://sashiko.example.com/api/webhook/gitlab`
3. In the **Secret token** field, enter a strong random value
4. Under **Trigger**, check **Merge request events**
5. Select **Add webhook**

In Sashiko's `Settings.toml`, use the same value:

```toml
[forge]
enabled = true
provider = "gitlab"
webhook_secret = "YOUR_SECRET_TOKEN_HERE"
```

> **Note:** The plain secret token verifies the sender's identity but does
> not independently verify payload integrity. Use HTTPS between GitLab and
> Sashiko for transport protection.

**Migrating to a signing token:** Configure both a signing token and a
secret token on the same webhook during migration. Sashiko checks for the
signing token headers first and falls back to the legacy token. Once
verified, remove the secret token from the webhook settings.

## GitHub HMAC setup

1. Generate a strong random secret:

   ```bash
   openssl rand -hex 32
   ```

2. In GitHub, go to your repository's **Settings > Webhooks > Add webhook**
3. Enter the **Payload URL**: `https://sashiko.example.com/api/webhook/github`
4. Set **Content type** to `application/json`
5. Paste the generated secret into the **Secret** field
6. Under **Which events**, select **Let me select individual events** and
   check **Pull requests**
7. Select **Add webhook**

In Sashiko's `Settings.toml`, use the same secret:

```toml
[forge]
enabled = true
provider = "github"
webhook_secret = "YOUR_HEX_SECRET_HERE"
```

For more details, see
[GitHub's webhook validation documentation](https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries).

## Script-based / cronjob setup

For setups that poll the GitLab API via a script and post results to Sashiko
via curl, the plain secret token is the simplest approach:

```bash
curl -X POST http://localhost:8080/api/webhook/gitlab \
  -H "Content-Type: application/json" \
  -H "X-Gitlab-Event: Merge Request Hook" \
  -H "X-Gitlab-Token: your-secret-here" \
  -d @payload.json
```

For HMAC signing from a shell script (stronger, but more complex):

```bash
SECRET="your-signing-key"
BODY=$(cat payload.json)
MSG_ID=$(uuidgen)
TIMESTAMP=$(date +%s)

SIG=$(printf '%s.%s.%s' "$MSG_ID" "$TIMESTAMP" "$BODY" \
  | openssl dgst -sha256 -hmac "$SECRET" -binary | base64)

curl -X POST http://localhost:8080/api/webhook/gitlab \
  -H "Content-Type: application/json" \
  -H "X-Gitlab-Event: Merge Request Hook" \
  -H "webhook-id: $MSG_ID" \
  -H "webhook-timestamp: $TIMESTAMP" \
  -H "webhook-signature: v1,$SIG" \
  -d "$BODY"
```

> **Note:** When using a `whsec_`-prefixed token, you must strip the prefix
> and base64-decode the remainder to get the raw HMAC key. For shell scripts,
> using a plain secret string is simpler.

## Production deployment checklist

- [ ] HTTPS with a valid TLS certificate (Let's Encrypt or CA-issued)
- [ ] Reverse proxy (nginx/Caddy/Traefik) terminates TLS and forwards to
      Sashiko on localhost
- [ ] `webhook_secret` configured in `Settings.toml`
- [ ] Signing token (not plain secret) for GitLab 19.0+
- [ ] `Settings.toml` file permissions restricted (`chmod 600`)
- [ ] Rate limiting configured at the reverse proxy level
- [ ] Log rotation configured (logrotate or journald)
- [ ] Firewall rules: only allow inbound on the HTTPS port from expected
      source IPs
- [ ] For GitLab: enable **SSL verification** in webhook settings
- [ ] For GitLab administrators: consider enabling **Block requests to the
      local network from webhooks** in Admin > Settings > Network

## Reverse proxy examples

### nginx

```nginx
server {
    listen 443 ssl http2;
    server_name sashiko.example.com;

    ssl_certificate /etc/letsencrypt/live/sashiko.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/sashiko.example.com/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;

    # Security headers
    add_header Strict-Transport-Security "max-age=31536000" always;
    add_header X-Frame-Options DENY;
    add_header X-Content-Type-Options nosniff;

    # Rate limiting for webhook endpoint
    limit_req_zone $binary_remote_addr zone=webhook:10m rate=10r/m;

    location /api/webhook/ {
        limit_req zone=webhook burst=5;
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

> **Note:** Do not configure the proxy to modify the request body.
> HMAC signatures are computed over the exact bytes sent by the forge.

### Caddy

```
sashiko.example.com {
    reverse_proxy localhost:8080
}
```

Caddy automatically obtains and renews TLS certificates via Let's Encrypt.

## Network security

**GitLab.com IP ranges:** See
[GitLab's IP range documentation](https://docs.gitlab.com/ee/user/gitlab_com/#ip-range)
for the current list of IP addresses used by GitLab.com webhooks.

**GitHub webhook IPs:** Query the
[GitHub meta API](https://api.github.com/meta) and use the `hooks` array
for the current webhook delivery IP ranges.

**Self-hosted forges:** Configure your firewall to allow inbound traffic
only from the forge server's IP address.

**Private deployments:** Consider using a VPN or SSH tunnel instead of
exposing Sashiko to the public internet. See
[Tunnel-based topology](#tunnel-based-ngrok-cloudflare-tunnel-ssh) above.

## Secret management

**Environment variable override:** Set `SASHIKO__FORGE__WEBHOOK_SECRET` to
override the value in `Settings.toml` without storing the secret on disk.

**File permissions:** Restrict access to `Settings.toml` when it contains
secrets:

```bash
chmod 600 Settings.toml
```

Run Sashiko as a dedicated service user with minimal privileges.

**Token rotation:**

1. Generate a new token in your forge's webhook settings
2. Update `webhook_secret` in `Settings.toml` (or the environment variable)
3. Restart Sashiko
4. Verify a test webhook delivery succeeds
5. Remove the old token from the forge webhook settings

**Secret managers:** For production deployments, consider using a secret
manager such as HashiCorp Vault, SOPS, or systemd's `LoadCredential` to
inject the secret at runtime rather than storing it in a configuration file.

## Troubleshooting

### 401 Unauthorized

The webhook secret is configured in Sashiko but the signature check failed.

**Common causes:**

- Secret mismatch — the value in `Settings.toml` does not match the value
  configured in the forge's webhook settings
- Wrong token type — using a `whsec_`-prefixed signing token with a forge
  that sends a plain `X-Gitlab-Token` header, or vice versa
- Trailing whitespace or newline in the TOML value
- Base64 encoding error in a `whsec_` token (Sashiko falls back to using
  the raw string as the key, which will not match)

**To debug:** Run Sashiko with `RUST_LOG=debug` and check the logs for
the specific verification failure. Also check the webhook delivery logs in
the GitLab or GitHub UI for the request headers and response status.

### 403 Forbidden

The request was rejected because Sashiko is not configured to accept
non-localhost requests.

**Common causes:**

- `webhook_secret` is not set in `Settings.toml`, and
  `--enable-unsafe-all-submit` is not passed
- The `[forge]` section is missing or `enabled` is `false`

**Fix:** Configure `webhook_secret` (recommended) or pass
`--enable-unsafe-all-submit` (not recommended for production).

### 400 Bad Request

The webhook payload failed validation.

**Common causes:**

- Invalid commit SHA format (must be 40 or 64 hex characters)
- Repository URL uses an unrecognized scheme or targets a blocked address
- PR/MR number is zero or negative
- Wrong event type header (expected `pull_request` for GitHub or
  `Merge Request Hook` for GitLab)

### 413 Payload Too Large

The request body exceeds the 25 MiB limit.

## FAQ

**Do I need webhook authentication if Sashiko only listens on localhost?**

It is not required, but it provides defense-in-depth. Other processes
running on the same host could send forged requests to the webhook endpoint.

**Can I use the same secret for both GitHub and GitLab?**

It is technically possible with a plain (non-`whsec_`-prefixed) secret, but
using separate tokens per provider is recommended for isolation.

**What happens if I configure a secret but the forge does not send one?**

The request is rejected with 401 Unauthorized. Both sides must be configured
with the same secret.

**Will existing webhooks break when I add `webhook_secret` to Settings.toml?**

Only if the forge is not configured to send the corresponding secret. Add
the secret to both the forge settings and Sashiko's `Settings.toml` at the
same time.

**Do I still need `--enable-unsafe-all-submit`?**

Not when `webhook_secret` is configured. The signature check authenticates
non-localhost requests. The flag is only needed for unauthenticated setups.

**What about replay attacks?**

The current implementation does not validate the `webhook-timestamp` header
for freshness. Use HTTPS to protect against network-level replay. Timestamp
validation is planned as a future enhancement.

**Port 8080 or 9080?**

The default listening port is 8080 (`server.port` in Settings.toml).
Configure it to any available port as needed.

## See also

- [Forge Setup Guide](FORGE_SETUP.md) — general forge integration architecture
- [GitHub Setup Guide](GITHUB_SETUP.md) — GitHub-specific webhook configuration
- [GitLab Setup Guide](GITLAB_SETUP.md) — GitLab-specific webhook configuration
- [Configuration Reference](configuration.md) — all Settings.toml options
