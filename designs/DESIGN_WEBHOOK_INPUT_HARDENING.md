# Design: Webhook & Ingestion Input Hardening

## Goal
Harden all HTTP ingestion endpoints against untrusted input. Forge
webhook and submit API endpoints accept external data that flows into
git commands, database queries, and the review pipeline. This design
addresses four related security gaps that share the same attack
surface.

## Threat Model

When Sashiko accepts unauthenticated webhook requests, an attacker
who can reach the endpoint can:

1. Forge webhooks to trigger expensive AI reviews (cost amplification)
2. Inject arbitrary repository URLs for Sashiko to clone (SSRF risk)
3. Exhaust server memory via oversized payloads or gzip bombs
4. Pass malformed commit SHAs to git commands
5. Pollute the review database with fake patchsets

## Architecture Changes

### 1. Webhook Signature Verification (`src/forge.rs`)

The `ForgeProvider` trait gains body and secret parameters:

```rust
fn validate_event(
    &self,
    headers: &HeaderMap,
    body: &Bytes,
    secret: Option<&str>,
) -> Result<(), StatusCode>;
```

Three verification methods, selected by header presence:

- **Standard Webhooks HMAC-SHA256** (GitLab 19.0+ signing token):
  verifies `webhook-signature` header over
  `{webhook-id}.{webhook-timestamp}.{body}`.
- **GitHub HMAC-SHA256**: verifies `X-Hub-Signature-256` header.
- **Legacy secret token**: constant-time comparison of
  `X-Gitlab-Token` header.

Secret type auto-detection: `whsec_` prefix indicates a Standard
Webhooks signing token (base64-decoded); plain strings are used as
raw key bytes. A warning is logged if base64 decoding fails for a
`whsec_`-prefixed token.

All comparisons use the `subtle` crate for constant-time operations.

### 2. Access Control Revision (`src/api.rs`)

When `webhook_secret` is configured, non-localhost requests are
permitted because the signature check is the access control. The
`--enable-unsafe-all-submit` flag is only needed for unauthenticated
setups.

```rust
let is_loopback = addr.ip().to_canonical().is_loopback();
let has_secret = webhook_secret.is_some();
if !is_loopback && !has_secret && !state.allow_all_submit {
    return Err(StatusCode::FORBIDDEN);
}
```

### 3. Body Size Limits (`src/api.rs`)

- Explicit `DefaultBodyLimit::max(25 MiB)` on the axum router.
  Sized to accommodate large CLI mbox submissions and GitHub
  webhook payloads (up to 25 MB).
- HTTP download cap (10 MiB) on `fetch_and_inject_thread` for
  lore.kernel.org mbox fetches.
- Decompression cap (50 MiB) via `Read::take()` on the gzip decoder
  to prevent decompression bombs.

### 4. Input Validation (`src/forge.rs`)

Validation functions applied in both forge `parse_payload` methods:

- `is_valid_git_sha(s)`: accepts 40-char SHA-1 or 64-char SHA-256,
  hex digits only.
- `is_safe_repo_url(url)`: requires `https://`, `http://`, or `git@`
  scheme; rejects known SSRF targets (cloud metadata endpoints,
  loopback addresses).
- `pr_number > 0` check on both forge providers.

These validations are applied only in the forge webhook path, not in
the CLI submit path, because the CLI legitimately sends git refs and
local filesystem paths.

Message-ID path separator sanitization (`/` and `\`) is applied in
the Thread submit handler to prevent URL path manipulation when
constructing lore.kernel.org fetch URLs.

### 5. Startup Warnings (`src/main.rs`)

Two warning conditions at startup:

- Forge enabled without `webhook_secret` and without
  `--enable-unsafe-all-submit`: warns that non-localhost requests
  will be rejected.
- Forge enabled without `webhook_secret` but with
  `--enable-unsafe-all-submit`: warns about accepting unauthenticated
  requests.

## Configuration

```toml
[forge]
enabled = true
provider = "gitlab"
webhook_secret = "whsec_..."  # signing token (recommended)
# OR
webhook_secret = "my-secret"  # plain secret token
```

No new required config fields. The existing `webhook_secret` field
(previously dead code) is activated.

## Dependencies

New crate dependencies:
- `hmac = "0.13"` (HMAC-SHA256 computation)
- `base64 = "0.22"` (signing token key decoding)
- `subtle = "2"` (constant-time comparison)

## Deployment Topologies

The design supports four deployment patterns, documented in
`docs/WEBHOOK_SECURITY.md`:

1. Public server with reverse proxy (nginx/Caddy terminates TLS)
2. Tunnel-based (ngrok, Cloudflare Tunnel, SSH)
3. Self-hosted forge on same LAN
4. Script-based polling (cronjob + curl)

When `webhook_secret` is configured, topologies 1-3 do not require
`--enable-unsafe-all-submit`.

## Risks

- SSRF blocklist is best-effort (DNS rebinding can bypass string
  checks). The primary access control is signature verification.
- `subtle::ConstantTimeEq` reveals length differences between
  compared strings (acceptable for webhook secrets with sufficient
  entropy).
- No replay attack prevention via timestamp validation in this
  version (documented as future enhancement).
