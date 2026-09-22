# Forge Webhooks and Fetching

Covers `src/forge.rs`, `forge_webhook` in `src/api.rs`, and `src/fetcher.rs`.

This path takes a request from an *unauthenticated stranger* — a forge webhook
— and turns it into a repository URL that the server hands to `git fetch`, and
a commit range it hands to `git rev-list`. Everything here exists because both
of those are dangerous with attacker-chosen input: a forged delivery costs
money (it queues an LLM review), writes rows, and can point the fetcher at a
host of the caller's choosing.

Three lines of defence, in the order the request meets them:

1. **The endpoint gate** (`forge_webhook`): `read_only`, then proof that the
   caller is entitled to reach a secretless endpoint.
2. **Signature verification** (`ForgeProvider::validate_event`): the real
   access control when a secret is configured.
3. **Payload validation** (`ForgeProvider::parse_payload`): `is_valid_git_sha`,
   positive PR number, `is_safe_repo_url`.

A diff that adds a provider, an endpoint, or a new consumer of a repo URL has
to re-establish all three. None of them is enforced by a type.

## 1. The endpoint gate

`forge_webhook` in `src/api.rs`, in order:

```rust
if state.read_only { return Err(StatusCode::FORBIDDEN); }
let webhook_secret = state.settings.forge.webhook_secret.as_deref();
let has_secret = webhook_secret.is_some();
if !has_secret && !presents_local_token(&headers, &state) && !state.allow_all_submit {
    return Err(StatusCode::FORBIDDEN);
}
let forge = state.forge_registry.get(&provider).ok_or(StatusCode::NOT_FOUND)?;
forge.validate_event(&headers, &body, webhook_secret)?;
let (action, metadata) = forge.parse_payload(&body)?;
```

- **When a secret is configured, the signature is the sole access control, for
  every caller including loopback.** That is deliberate: behind the deployed
  reverse proxy every request arrives from loopback.
- **When no secret is configured**, a forge cannot authenticate at all, so the
  endpoint admits only callers that can prove they share the machine
  (`presents_local_token`, i.e. read the `0600`
  `.sashiko-local-token` file — see api-auth.md) or an operator who passed
  `--enable-unsafe-all-submit`. This is `16c5beb7920b`: the earlier code
  accepted any loopback peer, which behind a proxy is the whole internet —
  "the check waved through exactly the requests it was meant to stop."
- `validate_event(…, None)` performs **event-type validation only** and the
  request is unauthenticated. The trait doc says so; the caller must have
  gated it. A diff that calls `validate_event` from a new site without
  reproducing the `has_secret` gate reintroduces the hole.
- Body size is bounded globally by `DefaultBodyLimit::max(25 * 1024 * 1024)` in
  `build_router` (raised from 2 MiB in `c36b518bba15` for large mbox and GitHub
  payloads). There is no per-route limit.

## 2. Signature verification

All three verifiers live in `src/forge.rs` and all three finish with
`subtle::ConstantTimeEq::ct_eq`. A byte-by-byte `==` on a MAC or a shared
secret returns early on the first differing byte, which leaks the correct
prefix over repeated requests; the attacker controls how many requests they
send, so the leak is practical. `ct_eq` on slices also reports unequal lengths
as unequal without timing variation — the comment on `verify_secret_token`
notes that the *length* is still observable, which is acceptable for a
high-entropy secret. Never accept `computed == received` in this file.

**GitHub** — `verify_github_signature`:
- header `x-hub-signature-256`, value `sha256={hex}`; a missing prefix is a
  rejection, not a fallback.
- HMAC-SHA256 over the **raw body bytes**. `forge_webhook` takes
  `body: axum::body::Bytes` and passes the same value to `validate_event` and
  `parse_payload`, so nothing re-serializes the JSON between verification and
  use. A diff that changes the extractor to `Json<T>` and re-encodes for the
  HMAC silently breaks every signature; a diff that verifies the signature over
  a re-serialized body is verifying the wrong bytes.
- received hex is lowercased before comparison (`c36b518bba15`).
- Event must be `pull_request` (`x-github-event`), else 400.
- Key material is `secret.as_bytes()` — **`whsec_` is not decoded here.**

**GitLab** — `validate_event` on `GitLabForge`, event must be
`Merge Request Hook`:
1. If all three of `webhook-id`, `webhook-timestamp`, `webhook-signature` are
   present, Standard Webhooks verification is used and the result is final —
   there is no fall-through to the legacy token on failure.
2. Otherwise, if `x-gitlab-token` is present, `verify_secret_token` compares it
   against the configured secret **verbatim**, again without decoding
   `whsec_`.
3. Otherwise 401. A configured secret always means some header must verify.

`verify_standard_webhook_signature` computes HMAC over
`"{msg_id}.{timestamp}." || body`, base64-encodes it, formats `v1,{b64}`, and
accepts if any space-separated entry of the header matches. Multiple
signatures are legitimate (key rotation);
`test_verify_standard_webhook_multiple_signatures` pins that one valid entry
among garbage is accepted.

**The `whsec_` convention** — `decode_webhook_secret`: a secret beginning
`whsec_` has the prefix stripped and the remainder base64-decoded (Standard
Webhooks). Anything else is used as raw UTF-8 bytes. A `whsec_` value whose
base64 fails to decode logs a warning and **falls back to the raw string
bytes**, which will simply never match — that is the documented cause of a
mystery 401 in `docs/WEBHOOK_SECURITY.md`. Note the asymmetry a reviewer must
keep straight: `decode_webhook_secret` is called by
`verify_standard_webhook_signature` **only**. GitHub HMAC and the GitLab legacy
token both use the configured string as-is. Adding a decode to one of them, or
forgetting it in a new provider, changes which secrets work.

**Not verified:** `webhook-timestamp` freshness. Replay of a captured, validly
signed delivery is possible; the FAQ in `docs/WEBHOOK_SECURITY.md` says so
plainly. If a diff adds freshness checking, it needs a clock-skew window and
must not break the existing tests that use a fixed timestamp.

**Not checked:** `settings.forge.enabled`. The route is registered
unconditionally in `build_router` and `forge_webhook` never reads
`forge.enabled` — it only reaches `/api/config` as a UI hint and gates the NNTP
ingestor in `src/main.rs`. Do not claim in review that disabling forge closes
the endpoint.

**Not filtered:** the PR/MR action. `parse_payload` returns
`payload["action"]` (GitHub) or `payload["object_kind"]` (GitLab) and
`forge_webhook` only logs and echoes it. Every delivery for a watched event
type queues a fetch and a review, including `closed`, `labeled` and
`synchronize`. If a diff claims to add action filtering, check it filters
before `create_fetching_patchset` and `fetch_sender.send`, not after.

## 3. `is_safe_repo_url` — what it does and does not do

State this plainly in review: **it is a host-based, best-effort blocklist, not
an SSRF mitigation.** Its own doc comment says DNS rebinding defeats it and
that "the primary access control is webhook signature verification." Do not
let a diff justify loosening the signature path on the grounds that URL
validation catches it.

Structure:

```rust
if !is_valid_repo_url(url) { return false; }          // https:// | http:// | git@
let host = if let Some(rest) = url.strip_prefix("git@") {
    let before_colon = rest.split(':').next().unwrap_or("");
    let ssh_host = before_colon.rsplit('@').next().unwrap_or(before_colon);
    ssh_host.to_ascii_lowercase()
} else if let Ok(parsed) = url::Url::parse(url) {
    parsed.host_str().unwrap_or("").to_ascii_lowercase()
} else { return false; };
```

The SSH branch takes the portion after the **last** `@`, because that is what
SSH resolves as the host. `git@github.com@127.0.0.1:repo.git` therefore has
host `127.0.0.1`, not `github.com`
(`test_is_safe_repo_url_blocks_ssh_injection`). Taking the first `@` — the
obvious reading — is the injection.

The blocklist, applied to the host only:

| Check | Blocks |
|---|---|
| `starts_with("169.254.")` | link-local / cloud metadata |
| `== "metadata.google.internal"` | GCP metadata by name |
| `starts_with("localhost")` | incl. `localhost.localdomain` |
| `starts_with("127.")` | incl. the short form `127.1` |
| `== "[::1]"` | `url::Url::host_str` keeps the brackets for IPv6 |
| `== "0.0.0.0"` | |
| `2130706432..=2130706687` from `host.parse::<u64>()` | decimal `127.0.0.0/8`, e.g. `2130706433` |
| `== "2852039166"` | decimal `169.254.169.254` |
| `starts_with("0x7f")` | hex loopback |
| `starts_with("0xa9fe")` | hex link-local |
| `starts_with("0177")` | octal loopback |

The whole-URL `contains()` version of this predicate was replaced by the
host-parsed version in `c36b518bba15`, because it rejected legitimate URLs
whose *path or username* contained a blocked string
(`https://github.com/user-127.0.0.1/repo.git`). Both directions are pinned by
tests — a diff that reverts to substring matching on the full URL will pass
`test_is_safe_repo_url_blocks_ssrf` and fail
`test_is_safe_repo_url_no_false_positives_on_path`.

Known gaps, worth stating when a diff leans on this function:
- RFC1918 (`10/8`, `172.16/12`, `192.168/16`), CGNAT and other IPv6 loopback
  spellings (`[::0001]`, `[0:0:0:0:0:0:0:1]`, `[::ffff:127.0.0.1]`) are **not**
  blocked.
- The SSH branch splits on the first `:`, so a bracketed IPv6 literal in
  scp-style syntax yields a nonsense host and is not blocked.
- DNS names resolving to internal addresses are not blocked, and cannot be by
  a host-string check.
- It is called from exactly two places: `GitHubForge::parse_payload` and
  `GitLabForge::parse_payload`. **`submit_patch` does not call it**, and that
  is deliberate (`7f364e9702f4`): the CLI path legitimately sends refs and
  local filesystem paths, and it is gated on `Permission::Ingest` instead.
  A diff must not "fix" `submit_patch` by applying `is_safe_repo_url` without
  accounting for local paths and bare refs.

## 4. `is_valid_git_sha`

```rust
(s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
```

40 hex (SHA-1) or 64 hex (SHA-256), either case. Both providers reject a
payload whose head or base SHA fails it. This matters because the value reaches
a command line: `forge_webhook` builds `format!("{}..{}", base_sha, head_sha)`,
and `FetchAgent` splits that on `..` and passes the halves to
`git rev-list`/`git rev-parse` and `git fetch <remote> <commit>`. Note
`fetcher::is_present` interpolates them into `"{}^{{commit}}"`. The hex
constraint is what keeps a ref-expression, an option-looking string, or a path
traversal out of those arguments — `test_is_valid_git_sha_rejects_non_hex` uses
`../../etc/passwd/...` for exactly that reason.

GitLab's `base_sha` is optional and defaults to `head_sha`
(`attrs["diff_refs"]["base_sha"]` missing ⇒ clone of head); both are validated
after that substitution, so the default cannot skip validation.

A diff that adds a new payload field which ends up in a git argument must
validate it with the same strictness or explain why. Branch or tag names in
particular are not covered by any validator here.

## 5. Secrets in URLs and logs

`FetchAgent::ensure_remote` injects the configured forge token into the clone
URL after parsing the URL and verifying `scheme == "https"` and `host_str() == Some("gitlab.com")`:

```rust
let authenticated_url = if let Some(token) = &self.gitlab_token {
    if let Ok(mut parsed) = url::Url::parse(url)
        && parsed.scheme() == "https"
        && parsed
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case("gitlab.com"))
    {
        let _ = parsed.set_username("oauth2");
        let _ = parsed.set_password(Some(token));
        parsed.to_string()
    } else {
        url.to_string()
    }
} else { url.to_string() };
```

The token is `settings.forge.api_token`, passed to `FetchAgent::new` in
`src/main.rs`. Two things follow, and a reviewer must check both on any diff
here:

- **The URL then contains a live credential**, so every log line that prints it
  goes through `crate::utils::redact_secret` (imported at the top of
  `src/fetcher.rs`; all three `ensure_remote` log sites use it).
  `redact_secret` (`src/utils.rs`, added by `f9c953623496`) applies two
  regexes: `(?i)(key|token|secret)=([a-zA-Z0-9_\-]+)` → `$1=[REDACTED]`, and
  `://([^/:]+):([^/@]+)@` → `://[REDACTED]:[REDACTED]@`. The second is what
  catches `https://oauth2:TOKEN@…`. A new `info!`/`warn!`/`error!` that
  formats a remote URL, or a `FetchRequest`, or anything derived from
  `authenticated_url`, must be wrapped. The regex only matches
  `scheme://user:pass@`; a token in a path segment or a non-matching query
  parameter name is not redacted.
- **`git`'s own stderr must also be passed through `redact_secret`.** `ensure_remote`,
  `fetch_commits`, and `fetch_all` pass `redact_secret(String::from_utf8_lossy(&output.stderr).trim())`
  before constructing errors, because `process_queue` puts that string into
  `Event::IngestionFailed { error }` and git error text can quote the remote URL it
  failed on. Treat any new use of raw git stderr in a user-visible or persisted
  message as a potential token leak, and pass it through `redact_secret`.

Transport hardening is separate and lives in
`crate::git_ops::GIT_PROTOCOL_RESTRICTIONS` (`6b631bc53b8b`):
`protocol.allow=never` plus explicit allows for http, https, git, ssh, file —
so `ext::` and friends are refused. `fetch_with_graph_retry` passes it; the
`git remote add/get-url/set-url` calls in `ensure_remote` do not, which is fine
because those perform no network I/O. **Any new `git` invocation that talks to
a remote must pass `GIT_PROTOCOL_RESTRICTIONS`.**

## 6. What a diff must verify

### Adding a forge provider

- Implement `ForgeProvider` and register it in `ForgeRegistry::new`; an
  unregistered name is a 404, a registered one is reachable by anyone who
  passes the endpoint gate.
- `validate_event` must: reject the wrong event type with 400 **before**
  touching the body; when `secret` is `Some`, require a signature header and
  return 401 when it is absent, malformed, or wrong — never fall through to
  `Ok(())`; compute the MAC over the raw `body` bytes; compare with `ct_eq`.
- Decide explicitly whether the secret goes through `decode_webhook_secret`,
  and say so in a comment — the existing providers disagree.
- `parse_payload` must call `is_valid_git_sha` on every SHA, reject a
  non-positive PR number, and call `is_safe_repo_url` on any URL it returns.
  Compare against `GitHubForge::parse_payload`: missing `pull_request`,
  missing `head.sha`, missing `base.sha`, `number <= 0` and an unsafe
  `clone_url` are each a 400.
- Add the four shapes the existing tests cover: accepts without secret,
  rejects missing signature, accepts a valid signature, rejects a wrong one.

### Adding or changing an endpoint that a forge can reach

- Reproduce the `read_only` check and the `has_secret || presents_local_token
  || allow_all_submit` gate. There is no shared middleware doing it.
- Never gate on the peer address, and never on `forge.enabled`, which is not
  consulted here.
- Keep the body as raw `Bytes` if anything signs over it.

### Adding a new use of a repo URL

- Where did the URL come from? Webhook payloads are validated by
  `parse_payload`; `SubmitRequest::Remote { repo }` from `/api/submit` is
  **not** validated and is trusted on the strength of `Permission::Ingest`;
  `settings.git.custom_remotes` is operator configuration.
- If the new consumer performs network I/O, does it pass
  `GIT_PROTOCOL_RESTRICTIONS`?
- Does the new code compare the URL against a host with `contains()`? That is
  the mistake `c36b518bba15` fixed in `is_safe_repo_url`. Always parse the URL
  with `url::Url::parse` and compare `parsed.host_str()` rather than using
  substring matching (`url.contains(...)`), which would match subdomains or
  path segments (`https://gitlab.com.example.net/x.git`).
- Does any new log line, database column, event payload or HTTP response carry
  the URL after `ensure_remote` may have added credentials?

## Checklist for a diff that touches this area

- [ ] New/changed `validate_event`: 400 on wrong event type, 401 on missing
      *and* invalid signature, `ct_eq` for every comparison, MAC over the raw
      body?
- [ ] Secret handling: is `whsec_` decoding applied consistently with the
      documented intent for that provider, and is the base64-failure fallback
      still only a warning plus a guaranteed mismatch?
- [ ] Secretless path: is the `presents_local_token || allow_all_submit` gate
      still there, and is nothing in it derived from the source address?
- [ ] Body: still `Bytes`, still the same bytes for verification and parsing?
- [ ] Every SHA from a payload passed through `is_valid_git_sha` before it can
      reach a git argument?
- [ ] Every payload-derived URL passed through `is_safe_repo_url`, and is the
      review's confidence in that function's coverage honest (host-based,
      no RFC1918, no DNS rebinding)?
- [ ] New git invocation with network access carrying
      `GIT_PROTOCOL_RESTRICTIONS`?
- [ ] Any new log, event or persisted error that can contain an authenticated
      remote URL or raw git stderr passed through `redact_secret`?
- [ ] New provider registered in `ForgeRegistry::new` and covered by the four
      signature tests plus payload-rejection tests?
